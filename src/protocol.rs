use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::ble::BleConnection;
use crate::types::{DeviceInfo, Model};

// Protocol constants
const ACK: u8 = 0xAA;
const END: u8 = 0xEA;
const XOR: u8 = 0xA5;

const CMD_VERSION: u8 = 0xC2;
// ECOP (CANopen SDO over BLE) protocol commands - discovered from SSI app logcat
const CMD_SDO_UPLOAD: u8 = 0xBF; // Initiate SDO upload (open object + request data)
const CMD_SDO_SEGMENT_0: u8 = 0xAC; // SDO segment with toggle=0
const CMD_SDO_SEGMENT_1: u8 = 0xFE; // SDO segment with toggle=1
const CMD_SET_DATETIME: u8 = 0xB0; // Set device date/time (C_SET_DATETIME)

// SDO response status codes (byte 0 of BF response)
const SDO_SEGMENTED: u8 = 0x41; // Data too large for response, use AC/FE segments
const SDO_EXPEDITED: u8 = 0x42; // Data fits in response (12 bytes)
const SDO_ABORT: u8 = 0x80; // Abort, bytes 4-7 = LE u32 CANopen abort code

const VERSION_SIZE: usize = 140;
const TIMEOUT_MS: u64 = 5000;

/// Build a command header: [cmd, cmd ^ XOR].
fn cmd_header(cmd: u8) -> [u8; 2] {
    [cmd, cmd ^ XOR]
}

pub fn hex_dump(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The device refused an SDO upload (BF response status 0x80).
/// The link is still usable afterwards, unlike a transport error.
#[derive(Debug)]
pub struct SdoAbort {
    pub index: u16,
    pub sub_index: u8,
    /// CANopen (CiA 301) abort code
    pub code: u32,
}

impl SdoAbort {
    fn reason(&self) -> &'static str {
        match self.code {
            0x0602_0000 => "object does not exist",
            0x0609_0011 => "sub-index does not exist",
            // Returned for dive indexes past the end of the logbook
            0x0800_0000 => "general error",
            0x0800_0020 => "data cannot be transferred",
            _ => "unknown abort code",
        }
    }
}

impl std::fmt::Display for SdoAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SDO abort: object 0x{:04X} sub {}: {} (0x{:08X})",
            self.index,
            self.sub_index,
            self.reason(),
            self.code
        )
    }
}

impl std::error::Error for SdoAbort {}

/// Send a command with no payload using VARIABLE packet mode.
/// Returns the data between ACK and END.
async fn packet_variable_no_payload(conn: &mut BleConnection, cmd: u8) -> Result<Vec<u8>> {
    conn.drain();
    conn.write(&cmd_header(cmd)).await?;

    let response = conn
        .recv_accumulated(2, TIMEOUT_MS)
        .await
        .context("Failed to read response")?;

    validate_response(&response)?;
    Ok(response[1..response.len() - 1].to_vec())
}

/// Send a command header, wait for ACK, send payload, collect response until END.
/// Returns the full response (ACK + data + END) accumulated from notifications.
async fn send_with_payload(
    conn: &mut BleConnection,
    cmd: u8,
    payload: &[u8],
) -> Result<Vec<u8>> {
    conn.drain();
    conn.write(&cmd_header(cmd)).await?;

    // Wait for ACK (first notification)
    let ack = conn
        .recv(TIMEOUT_MS)
        .await
        .context("No ACK after header")?;

    if ack.is_empty() || ack[0] != ACK {
        bail!(
            "Expected ACK, got [{}]",
            hex_dump(&ack)
        );
    }

    // Send payload immediately after ACK
    conn.write(payload).await?;

    // Collect response until END marker
    let mut response = ack;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(TIMEOUT_MS);

    loop {
        if response.len() >= 2 && *response.last().unwrap() == END {
            break;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!(
                "Timeout waiting for END (got {} bytes: [{}])",
                response.len(),
                hex_dump(&response)
            );
        }
        let chunk = conn.recv(remaining.as_millis() as u64).await?;
        response.extend_from_slice(&chunk);
    }

    Ok(response)
}

/// Receive a single SDO segment response (for AC or FE).
/// The response format is: [AA, toggle_byte, data..., EA]
/// Returns the raw data bytes (everything between AA and EA, including toggle byte).
async fn recv_sdo_segment(conn: &mut BleConnection, expected_data_len: usize) -> Result<Vec<u8>> {
    conn.drain();

    // Total expected: AA + (1 toggle/status + data) + EA
    let total_expected = 1 + 1 + expected_data_len + 1; // AA + toggle + data + EA

    let mut response = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(TIMEOUT_MS);

    loop {
        // Check if we have enough data and it ends with EA
        if response.len() >= 3 && *response.last().unwrap() == END {
            break;
        }
        // Also break if we have more than expected
        if response.len() >= total_expected {
            break;
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!(
                "Timeout waiting for SDO segment (got {} bytes: [{}])",
                response.len(),
                hex_dump(&response)
            );
        }
        let chunk = conn.recv(remaining.as_millis() as u64).await?;
        response.extend_from_slice(&chunk);
    }

    if response.is_empty() || response[0] != ACK {
        bail!(
            "SDO segment: expected ACK, got [{}]",
            hex_dump(&response)
        );
    }

    // Strip ACK and END, return data bytes (toggle + payload)
    let end = if *response.last().unwrap() == END {
        response.len() - 1
    } else {
        response.len()
    };

    Ok(response[1..end].to_vec())
}

fn validate_response(data: &[u8]) -> Result<()> {
    if data.is_empty() {
        bail!("Empty response");
    }
    if data[0] != ACK {
        bail!(
            "Expected ACK (0x{ACK:02X}), got 0x{:02X} (full: [{}])",
            data[0],
            hex_dump(data)
        );
    }
    if *data.last().unwrap() != END {
        bail!(
            "Expected END (0x{END:02X}), got 0x{:02X} (full: [{}])",
            data.last().unwrap(),
            hex_dump(data)
        );
    }
    Ok(())
}

// ── ECOP SDO Protocol ──
// The Sirius uses CANopen-like SDO (Service Data Object) transfers over BLE.
//
// Object addressing: index (16-bit, e.g. 0x2000) + sub-index (8-bit, e.g. 0x04)
//   Index 0x2000+i = device/config objects (type 0x20)
//   Index 0x3000+i = dive log objects (type 0x30)
//
// BF payload format (18 bytes): [0x40, index_lo, index_hi, sub_index, 0x00 * 14]
// BF response (17 bytes):       [status, index_lo, index_hi, sub_index, data[12], 0xEA]
//   status 0x41 = segmented: bytes 4-5 = LE u16 data size, use AC/FE to read
//   status 0x42 = expedited: bytes 4-15 = the 12 data bytes directly
//   status 0x80 = abort / not found
//
// AC (toggle=0) / FE (toggle=1): alternating segment reads, up to 241 bytes each

/// Read an object from the device using the ECOP SDO protocol.
/// Returns the data bytes for the requested object+sub-index.
pub async fn ecop_read(
    conn: &mut BleConnection,
    index: u16,
    sub_index: u8,
) -> Result<Vec<u8>> {
    let index_lo = (index & 0xFF) as u8;
    let index_hi = ((index >> 8) & 0xFF) as u8;

    // Build 18-byte BF payload: [0x40, index_lo, index_hi, sub_index, 0x00 * 14]
    let mut payload = [0u8; 18];
    payload[0] = 0x40;
    payload[1] = index_lo;
    payload[2] = index_hi;
    payload[3] = sub_index;

    // Send BF with payload
    let response = send_with_payload(conn, CMD_SDO_UPLOAD, &payload).await?;

    // Parse BF response: strip ACK, keep until END
    // Format: [AA, status, idx_lo, idx_hi, sub, data[12], EA]
    if response.len() < 6 {
        bail!(
            "BF response too short: {} bytes [{}]",
            response.len(),
            hex_dump(&response)
        );
    }

    // Strip AA prefix, get the ecop data (before EA)
    let ecop_end = if *response.last().unwrap() == END {
        response.len() - 1
    } else {
        response.len()
    };
    let ecop = &response[1..ecop_end]; // status + index + sub + data

    if ecop.is_empty() {
        bail!("Empty ECOP response");
    }

    let status = ecop[0];

    match status {
        SDO_ABORT => {
            let code = match ecop.get(4..8) {
                Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                None => 0,
            };
            Err(SdoAbort {
                index,
                sub_index,
                code,
            }
            .into())
        }
        SDO_EXPEDITED => {
            // Data is directly in bytes 4..16 of ecop response
            if ecop.len() < 16 {
                bail!("Expedited response too short: {} bytes", ecop.len());
            }
            Ok(ecop[4..16].to_vec())
        }
        SDO_SEGMENTED => {
            // Bytes 4-5 = LE u16 data size
            if ecop.len() < 6 {
                bail!("Segmented response too short: {} bytes", ecop.len());
            }
            let data_size = u16::from_le_bytes([ecop[4], ecop[5]]) as usize;

            // Read data via alternating AC/FE segments
            let mut data = Vec::with_capacity(data_size);
            let mut toggle = 0u8; // start with AC (toggle=0)
            let max_segment = 241; // max data per segment (from SSI app: maxSegmentDataLength)

            while data.len() < data_size {
                let remaining = data_size - data.len();
                let segment_size = remaining.min(max_segment);

                // Send segment command
                let cmd = if toggle == 0 {
                    CMD_SDO_SEGMENT_0 // AC
                } else {
                    CMD_SDO_SEGMENT_1 // FE
                };

                conn.drain();
                conn.write(&cmd_header(cmd)).await?;

                // Receive segment: [AA, toggle_byte, data..., EA]
                let segment = recv_sdo_segment(conn, segment_size).await?;

                // First byte of segment data is toggle/status, rest is payload
                if segment.is_empty() {
                    bail!("Empty SDO segment");
                }
                let segment_data = &segment[1..]; // skip toggle byte
                data.extend_from_slice(segment_data);

                toggle ^= 1; // alternate
            }

            // Trim to exact size
            data.truncate(data_size);
            Ok(data)
        }
        _ => {
            bail!(
                "Unknown SDO status 0x{status:02X} [{}]",
                hex_dump(ecop)
            );
        }
    }
}

/// Send C_SET_DATETIME command to set the device's clock.
/// The payload is a 4-byte LE Unix timestamp.
pub async fn set_datetime(conn: &mut BleConnection) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;

    let payload = now.to_le_bytes();
    eprintln!(
        "Setting device datetime (timestamp: {now}, payload: [{}])",
        hex_dump(&payload)
    );

    let response = send_with_payload(conn, CMD_SET_DATETIME, &payload).await?;

    // B0 response should be just [AA, EA] or [EA]
    if response.is_empty() {
        bail!("No response to SET_DATETIME");
    }

    // Response might be [AA, EA] or notifications might come separately
    eprintln!("  SET_DATETIME response: [{}]", hex_dump(&response));
    Ok(())
}

/// Query device version info (CMD_VERSION).
pub async fn get_device_info(conn: &mut BleConnection) -> Result<DeviceInfo> {
    eprintln!("Querying device info...");

    let data = packet_variable_no_payload(conn, CMD_VERSION)
        .await
        .context("CMD_VERSION failed")?;

    if data.len() < VERSION_SIZE {
        bail!(
            "Version response too short: {} bytes (expected {})",
            data.len(),
            VERSION_SIZE
        );
    }

    // Model name is at offset 0x46, null-terminated string
    let name_start = 0x46;
    let name_end = data[name_start..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| name_start + p)
        .unwrap_or(data.len().min(name_start + 16));
    let model_name = String::from_utf8_lossy(&data[name_start..name_end]).to_string();

    let model = Model::from_name(&model_name);

    Ok(DeviceInfo { model_name, model })
}

/// Read the PCB number / serial string from object 0x2000, sub-index 4.
pub async fn read_pcb_number(conn: &mut BleConnection) -> Result<String> {
    let data = ecop_read(conn, 0x2000, 4).await?;
    let s = String::from_utf8_lossy(&data)
        .trim_end_matches('\0')
        .to_string();
    Ok(s)
}

/// Read a dive header (200 bytes) for the given dive index.
/// Returns the raw 200-byte header data.
pub async fn read_dive_header(conn: &mut BleConnection, dive_index: u16) -> Result<Vec<u8>> {
    let index = 0x3000 + dive_index;
    ecop_read(conn, index, 4).await
}

/// Read a dive profile (variable size) for the given dive index.
/// Returns the raw profile data containing DSTR, TISS, DPRS, AIRS records.
pub async fn read_dive_profile(conn: &mut BleConnection, dive_index: u16) -> Result<Vec<u8>> {
    let index = 0x3000 + dive_index;
    ecop_read(conn, index, 3).await
}

/// Enumerate dive objects by trying to open them sequentially.
/// Returns the number of valid dive objects found.
pub async fn count_dives(conn: &mut BleConnection) -> Result<u16> {
    let mut count = 0u16;

    // Build BF payload for index 0x3000+count, sub 4
    loop {
        let index = 0x3000 + count;
        let index_lo = (index & 0xFF) as u8;
        let index_hi = ((index >> 8) & 0xFF) as u8;

        let mut payload = [0u8; 18];
        payload[0] = 0x40;
        payload[1] = index_lo;
        payload[2] = index_hi;
        payload[3] = 4; // sub-index 4 = header

        let response = send_with_payload(conn, CMD_SDO_UPLOAD, &payload).await?;

        let ecop_end = if *response.last().unwrap() == END {
            response.len() - 1
        } else {
            response.len()
        };
        let ecop = &response[1..ecop_end];

        if ecop.is_empty() || ecop[0] == SDO_ABORT {
            break;
        }

        count += 1;

        // Safety limit
        if count >= 256 {
            break;
        }
    }

    Ok(count)
}
