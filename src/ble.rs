use std::time::Duration;

use anyhow::{bail, Context, Result};
use btleplug::api::{
    Central, CentralEvent, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
    WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time;
use uuid::Uuid;

/// Known BLE UUIDs — confirmed matching the Sirius built-in BLE.
pub const KNOWN_WRITE_UUID: Uuid =
    Uuid::from_u128(0x99a91ebd_b21f_1689_bb43_681f1f55e966);
pub const KNOWN_READ_UUID: Uuid =
    Uuid::from_u128(0x1d1aae28_d2a8_91a1_1242_9d2973fbe571);

/// BLE name prefixes that identify Mares dive computers.
const MARES_NAME_PREFIXES: &[&str] = &[
    "Mares",
    "Sirius",
    "Quad Ci",
    "Quad2",
    "Puck4",
    "Puck Lite",
    "Puck Pro U",
    "Puck",
];

/// Which protocol family a discovered device speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Mares,
    Dsx,
}

/// Discovered BLE device info.
#[derive(Debug)]
pub struct DiscoveredDevice {
    pub name: String,
    pub kind: DeviceKind,
    pub address: String,
    pub rssi: Option<i16>,
    pub peripheral: Peripheral,
}

/// GATT service with its characteristics.
#[derive(Debug)]
pub struct GattService {
    pub uuid: Uuid,
    pub characteristics: Vec<GattCharacteristic>,
}

/// GATT characteristic info.
#[derive(Debug)]
pub struct GattCharacteristic {
    pub uuid: Uuid,
    pub properties: String,
}

/// An active BLE connection to a Mares device with a persistent notification channel.
pub struct BleConnection {
    pub peripheral: Peripheral,
    pub write_char: Characteristic,
    rx: mpsc::Receiver<Vec<u8>>,
    // Keep the task handle alive so the background listener doesn't get dropped
    _listener: tokio::task::JoinHandle<()>,
}

/// Get the default BLE adapter.
pub async fn get_adapter() -> Result<Adapter> {
    let manager = Manager::new().await.context("Failed to create BLE manager")?;
    let adapters = manager
        .adapters()
        .await
        .context("Failed to get BLE adapters")?;
    adapters
        .into_iter()
        .next()
        .context("No BLE adapters found")
}

/// Scan for supported BLE dive computers.
pub async fn scan_for_devices(
    adapter: &Adapter,
    timeout: Duration,
) -> Result<Vec<DiscoveredDevice>> {
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("Failed to start BLE scan")?;

    let mut events = adapter
        .events()
        .await
        .context("Failed to get adapter events")?;

    let deadline = time::Instant::now() + timeout;

    let mut found_addresses = std::collections::HashSet::new();
    loop {
        let remaining = deadline.saturating_duration_since(time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match time::timeout(remaining, events.next()).await {
            Ok(Some(CentralEvent::DeviceDiscovered(id))) => {
                if found_addresses.contains(&id) {
                    continue;
                }
                if let Ok(peripheral) = adapter.peripheral(&id).await {
                    if let Ok(Some(props)) = peripheral.properties().await {
                        if let Some(ref name) = props.local_name {
                            if device_kind(name).is_some() {
                                found_addresses.insert(id);
                                eprintln!(
                                    "  Found: {} [{}] RSSI: {}",
                                    name,
                                    props.address,
                                    props
                                        .rssi
                                        .map(|r| r.to_string())
                                        .unwrap_or_else(|| "?".into())
                                );
                            }
                        }
                    }
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    adapter.stop_scan().await.ok();

    let mut devices = Vec::new();
    let peripherals = adapter.peripherals().await?;
    for p in peripherals {
        if let Ok(Some(props)) = p.properties().await {
            if let Some(ref name) = props.local_name {
                if let Some(kind) = device_kind(name) {
                    devices.push(DiscoveredDevice {
                        name: name.clone(),
                        kind,
                        address: props.address.to_string(),
                        rssi: props.rssi,
                        peripheral: p,
                    });
                }
            }
        }
    }

    Ok(devices)
}

/// Enumerate all GATT services and characteristics on a connected peripheral.
pub async fn enumerate_gatt(peripheral: &Peripheral) -> Result<Vec<GattService>> {
    peripheral
        .discover_services()
        .await
        .context("Failed to discover GATT services")?;

    let services_raw = peripheral.services();
    let mut services = Vec::new();

    for svc in &services_raw {
        let mut chars = Vec::new();
        for c in &svc.characteristics {
            let props = format!("{:?}", c.properties);
            chars.push(GattCharacteristic {
                uuid: c.uuid,
                properties: props,
            });
        }
        services.push(GattService {
            uuid: svc.uuid,
            characteristics: chars,
        });
    }

    Ok(services)
}

/// Connect to a Mares device and set up a persistent notification listener.
pub async fn connect(
    peripheral: &Peripheral,
    write_uuid: Option<Uuid>,
    read_uuid: Option<Uuid>,
) -> Result<BleConnection> {
    if !peripheral.is_connected().await? {
        peripheral
            .connect()
            .await
            .context("Failed to connect to device")?;
    }

    peripheral
        .discover_services()
        .await
        .context("Failed to discover services")?;

    let write_target = write_uuid.unwrap_or(KNOWN_WRITE_UUID);
    let read_target = read_uuid.unwrap_or(KNOWN_READ_UUID);

    let chars = peripheral.characteristics();

    let write_char = chars
        .iter()
        .find(|c| c.uuid == write_target)
        .cloned()
        .with_context(|| format!("Write characteristic {write_target} not found"))?;

    let read_char = chars
        .iter()
        .find(|c| c.uuid == read_target)
        .cloned()
        .with_context(|| format!("Read characteristic {read_target} not found"))?;

    // Subscribe to notifications
    peripheral
        .subscribe(&read_char)
        .await
        .context("Failed to subscribe to notifications")?;

    // Spawn a persistent background task that forwards notifications into an mpsc channel.
    // This ensures no notifications are lost between reads.
    let (tx, rx) = mpsc::channel::<Vec<u8>>(64);
    let mut stream = peripheral.notifications().await?;
    let read_uuid_filter = read_char.uuid;

    let listener = tokio::spawn(async move {
        while let Some(notification) = stream.next().await {
            if notification.uuid == read_uuid_filter
                && tx.send(notification.value).await.is_err()
            {
                break; // receiver dropped
            }
        }
    });

    Ok(BleConnection {
        peripheral: peripheral.clone(),
        write_char,
        rx,
        _listener: listener,
    })
}

impl BleConnection {
    /// Write data to the device, splitting into 20-byte BLE chunks.
    pub async fn write(&self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(20) {
            self.peripheral
                .write(&self.write_char, chunk, WriteType::WithoutResponse)
                .await
                .context("BLE write failed")?;
        }
        Ok(())
    }

    /// Write a whole packet in one GATT write, for protocols whose packets
    /// must not be split (the DSX).
    pub async fn write_packet(&self, data: &[u8]) -> Result<()> {
        let write_type = if self.write_char.properties.contains(CharPropFlags::WRITE) {
            WriteType::WithResponse
        } else {
            WriteType::WithoutResponse
        };
        self.peripheral
            .write(&self.write_char, data, write_type)
            .await
            .context("BLE write failed")
    }

    /// Receive the next notification packet with timeout.
    pub async fn recv(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        match time::timeout(Duration::from_millis(timeout_ms), self.rx.recv()).await {
            Ok(Some(data)) => Ok(data),
            Ok(None) => bail!("Notification channel closed"),
            Err(_) => bail!("BLE read timed out after {timeout_ms}ms"),
        }
    }

    /// Receive notification data, accumulating until we have at least `min_bytes` or timeout.
    pub async fn recv_accumulated(
        &mut self,
        min_bytes: usize,
        timeout_ms: u64,
    ) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let deadline = time::Instant::now() + Duration::from_millis(timeout_ms);

        loop {
            let remaining = deadline.saturating_duration_since(time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            match time::timeout(remaining, self.rx.recv()).await {
                Ok(Some(data)) => {
                    buf.extend_from_slice(&data);
                    if buf.len() >= min_bytes {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        if buf.is_empty() {
            bail!("No data received within timeout");
        }

        Ok(buf)
    }

    /// Drain any buffered notifications (to clear stale data between commands).
    pub fn drain(&mut self) {
        while self.rx.try_recv().is_ok() {}
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.peripheral
            .disconnect()
            .await
            .context("Failed to disconnect")?;
        Ok(())
    }
}

fn device_kind(name: &str) -> Option<DeviceKind> {
    if is_mares_device(name) {
        Some(DeviceKind::Mares)
    } else if is_dsx_device(name) {
        Some(DeviceKind::Dsx)
    } else {
        None
    }
}

/// The Apeks DSX advertises its model code "GA" followed by its serial number.
fn is_dsx_device(name: &str) -> bool {
    name.strip_prefix("GA")
        .is_some_and(|serial| !serial.is_empty() && serial.bytes().all(|b| b.is_ascii_digit()))
}

fn is_mares_device(name: &str) -> bool {
    MARES_NAME_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}
