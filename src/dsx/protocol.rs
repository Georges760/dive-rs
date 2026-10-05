//! Pelagic packet protocol of the Apeks DSX (shared with the Aqualung i330R).
//!
//! Ported from libdivecomputer `pelagic_i330r.c` and cross-checked against
//! DiverLog+ 1.4.9. Not yet run against a device.

use anyhow::{bail, Result};
use uuid::Uuid;

use crate::ble::BleConnection;

pub const WRITE_UUID: Uuid = Uuid::from_u128(0xca7b0003_f785_4c38_b599_c7c5fbadb034);
pub const NOTIFY_UUID: Uuid = Uuid::from_u128(0xca7b0002_f785_4c38_b599_c7c5fbadb034);

/// Model code reported in the wakeup response ("GA").
pub const MODEL_DSX: u16 = 0x4741;

const START: u8 = 0xCD;

const FLAG_NONE: u8 = 0x00;
const FLAG_REQUEST: u8 = 0x40;
const FLAG_DATA: u8 = 0x80;
const FLAG_LAST: u8 = 0xC0;

const CMD_ACCESS_REQUEST: u8 = 0xFA;
const CMD_ACCESS_CODE: u8 = 0xFB;
const CMD_AUTHENTICATION: u8 = 0x97;
const CMD_WAKEUP_RDWR: u8 = 0x22;
const CMD_READ_HW_CAL: u8 = 0x27;
const CMD_READ_EXFLASHMAP: u8 = 0x2F;
const CMD_READ_FLASH: u8 = 0x0D;

const RSP_READY: u8 = 1;
const RSP_DONE: u8 = 2;
const RSP_NO_PASSCODE: u8 = 12;

const HEADER_LEN: usize = 5;
const MAX_PAYLOAD: usize = 255;
const TIMEOUT_MS: u64 = 3000;

/// Largest flash read per command, as libdivecomputer does.
const FLASH_CHUNK: u32 = 4096;

/// The device answered a command with a response code other than the expected one.
#[derive(Debug)]
pub struct ResponseCode {
    pub cmd: u8,
    pub code: u8,
}

impl ResponseCode {
    /// Code 13: the access code or passcode was rejected.
    pub fn is_access_denied(&self) -> bool {
        self.code == 13
    }
}

impl std::fmt::Display for ResponseCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Names from DiverLog+ (BLE5_CMD)
        let name = match self.code {
            1 => "ready",
            2 => "done",
            6 => "no read/write access",
            7 => "unknown command",
            12 => "no passcode required",
            13 => "access code rejected",
            255 => "abort",
            _ => "unexpected",
        };
        write!(
            f,
            "DSX command 0x{:02X}: response {} ({})",
            self.cmd, self.code, name
        )
    }
}

impl std::error::Error for ResponseCode {}

/// Packet checksum, computed with the checksum byte set to zero.
fn checksum(data: &[u8]) -> u8 {
    let mut csum = 0u8;
    for &byte in data {
        let a = csum ^ byte;
        let b = (a >> 7) ^ (a >> 4) ^ a;
        csum = (b << 4) ^ (b << 1);
    }
    csum
}

/// CRC16-CCITT (init 0xFFFF) protecting the 256-byte blocks.
fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Build a packet: [0xCD, flag, cmd, checksum, length, payload].
fn build_packet(cmd: u8, flag: u8, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= MAX_PAYLOAD);
    let mut packet = vec![START, flag, cmd, 0, payload.len() as u8];
    packet.extend_from_slice(payload);
    packet[3] = checksum(&packet);
    packet
}

/// Validate one received packet and return its flag and payload.
fn parse_packet(raw: &[u8], cmd: u8) -> Result<(u8, &[u8])> {
    if raw.len() < HEADER_LEN {
        bail!("DSX packet too short: {} bytes", raw.len());
    }
    if raw[0] != START {
        bail!("Unexpected DSX packet start byte 0x{:02X}", raw[0]);
    }
    if raw[2] != cmd {
        bail!(
            "Unexpected DSX packet command 0x{:02X} (expected 0x{cmd:02X})",
            raw[2]
        );
    }
    let end = HEADER_LEN + raw[4] as usize;
    if end > raw.len() {
        bail!(
            "DSX packet length {} exceeds the {} bytes received",
            raw[4],
            raw.len()
        );
    }

    let mut packet = raw[..end].to_vec();
    packet[3] = 0;
    let expected = checksum(&packet);
    if raw[3] != expected {
        bail!(
            "DSX packet checksum 0x{:02X}, expected 0x{expected:02X}",
            raw[3]
        );
    }

    Ok((raw[1], &raw[HEADER_LEN..end]))
}

/// Send a command and collect its reply: data packets, then one response
/// packet whose first byte is the response code.
async fn transfer(
    conn: &mut BleConnection,
    cmd: u8,
    flag: u8,
    payload: &[u8],
    answer_len: usize,
    expected: u8,
) -> Result<Vec<u8>> {
    conn.drain();
    conn.write_packet(&build_packet(cmd, flag, payload)).await?;

    let mut answer = Vec::with_capacity(answer_len);
    let code = loop {
        // One notification carries one packet
        let raw = conn.recv(TIMEOUT_MS).await?;
        let (flag, data) = parse_packet(&raw, cmd)?;
        if flag & FLAG_LAST == FLAG_LAST {
            break data.first().copied().unwrap_or(0);
        }
        answer.extend_from_slice(data);
    };

    if code != expected {
        return Err(ResponseCode { cmd, code }.into());
    }
    if answer.len() != answer_len {
        bail!(
            "DSX command 0x{cmd:02X}: received {} bytes, expected {answer_len}",
            answer.len()
        );
    }
    Ok(answer)
}

/// Ask for access with a stored access code. `Ok(false)` means the device
/// wants a passcode exchange first (see `request_access_code`).
pub async fn request_access(conn: &mut BleConnection, access_code: &[u8; 16]) -> Result<bool> {
    transfer(
        conn,
        CMD_ACCESS_REQUEST,
        FLAG_REQUEST,
        &[0; 9],
        0,
        RSP_READY,
    )
    .await?;

    match transfer(
        conn,
        CMD_ACCESS_REQUEST,
        FLAG_DATA,
        access_code,
        0,
        RSP_DONE,
    )
    .await
    {
        // An all-zero code is acknowledged but grants nothing
        Ok(_) => Ok(access_code != &[0; 16]),
        Err(e) => match e.downcast_ref::<ResponseCode>() {
            Some(response) if response.code == RSP_NO_PASSCODE => Ok(true),
            Some(response) if response.is_access_denied() => Ok(false),
            _ => Err(e),
        },
    }
}

/// Exchange a 6-digit passcode for the 16-byte access code to store.
pub async fn request_access_code(conn: &mut BleConnection, passcode: &[u8; 6]) -> Result<[u8; 16]> {
    transfer(conn, CMD_ACCESS_CODE, FLAG_REQUEST, &[0; 9], 0, RSP_READY).await?;
    let code = transfer(conn, CMD_ACCESS_CODE, FLAG_DATA, passcode, 16, RSP_DONE).await?;
    Ok(code.try_into().unwrap())
}

/// Passcode the way DiverLog+ makes one up for a DSX: four random digits
/// followed by the two-digit sum of those digits.
pub fn generate_passcode() -> [u8; 6] {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    passcode_from(1001 + nanos % 8999)
}

fn passcode_from(number: u32) -> [u8; 6] {
    let digits = [
        number / 1000 % 10,
        number / 100 % 10,
        number / 10 % 10,
        number % 10,
    ];
    let sum: u32 = digits.iter().sum();
    [
        digits[0] as u8,
        digits[1] as u8,
        digits[2] as u8,
        digits[3] as u8,
        (sum / 10) as u8,
        (sum % 10) as u8,
    ]
}

/// Identity returned by the wakeup command.
#[derive(Debug)]
pub struct DeviceId {
    pub model: u16,
    pub firmware: u32,
    pub raw: [u8; 16],
}

/// Wake the device up for reading, then authenticate. Needs access first.
pub async fn wakeup(conn: &mut BleConnection) -> Result<DeviceId> {
    let args = [0, 0, 0, 0, 0x0C, 0, 0, 0, 0];
    let id = transfer(conn, CMD_WAKEUP_RDWR, FLAG_REQUEST, &args, 16, RSP_DONE).await?;
    let raw: [u8; 16] = id.try_into().unwrap();

    let model = u16::from_be_bytes([raw[12], raw[13]]);
    // DSX authentication code; the i330R uses 37 30 31 55
    let auth: [u8; 9] = [0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0];
    transfer(conn, CMD_AUTHENTICATION, FLAG_REQUEST, &auth, 0, RSP_READY).await?;

    Ok(DeviceId {
        model,
        firmware: u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
        raw,
    })
}

/// Read a 256-byte block that ends with its CRC16.
async fn read_block(conn: &mut BleConnection, cmd: u8, args: &[u8; 9]) -> Result<Vec<u8>> {
    let block = transfer(conn, cmd, FLAG_REQUEST, args, 256, RSP_DONE).await?;
    let stored = u16::from_be_bytes([block[254], block[255]]);
    let computed = crc16_ccitt(&block[..254]);
    if stored != computed {
        bail!("DSX block 0x{cmd:02X}: CRC 0x{stored:04X}, expected 0x{computed:04X}");
    }
    Ok(block)
}

/// Hardware calibration block; holds the serial number.
pub async fn read_hw_cal(conn: &mut BleConnection) -> Result<Vec<u8>> {
    read_block(conn, CMD_READ_HW_CAL, &[0, 0, 0, 0, 0, 1, 0, 0, 0]).await
}

/// Flash map block; holds the logbook and profile ring pointers.
pub async fn read_flash_map(conn: &mut BleConnection) -> Result<Vec<u8>> {
    read_block(conn, CMD_READ_EXFLASHMAP, &[0; 9]).await
}

/// Read `size` bytes of flash starting at `address`.
pub async fn read_flash(conn: &mut BleConnection, address: u32, size: u32) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(size as usize);
    let mut offset = 0;
    while offset < size {
        let chunk = (size - offset).min(FLASH_CHUNK);
        let mut args = [0u8; 9];
        args[..4].copy_from_slice(&(address + offset).to_le_bytes());
        args[4..8].copy_from_slice(&chunk.to_le_bytes());
        data.extend(
            transfer(
                conn,
                CMD_READ_FLASH,
                FLAG_NONE,
                &args,
                chunk as usize,
                RSP_DONE,
            )
            .await?,
        );
        offset += chunk;
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_matches_the_ccitt_false_check_value() {
        assert_eq!(crc16_ccitt(b"123456789"), 0x29B1);
    }

    #[test]
    fn packet_round_trip() {
        let packet = build_packet(CMD_READ_FLASH, FLAG_NONE, &[1, 2, 3]);
        assert_eq!(packet[..3], [START, FLAG_NONE, CMD_READ_FLASH]);
        assert_eq!(packet[4], 3);
        let (flag, payload) = parse_packet(&packet, CMD_READ_FLASH).unwrap();
        assert_eq!((flag, payload), (FLAG_NONE, &[1u8, 2, 3][..]));
    }

    #[test]
    fn packet_rejects_corruption_and_wrong_command() {
        let mut packet = build_packet(CMD_WAKEUP_RDWR, FLAG_REQUEST, &[0; 9]);
        assert!(parse_packet(&packet, CMD_READ_FLASH).is_err());
        packet[7] ^= 0x10;
        assert!(parse_packet(&packet, CMD_WAKEUP_RDWR).is_err());
    }

    #[test]
    fn packet_ignores_bytes_after_the_declared_length() {
        let mut packet = build_packet(CMD_ACCESS_CODE, FLAG_LAST, &[RSP_DONE]);
        packet.extend_from_slice(&[0; 10]);
        let (flag, payload) = parse_packet(&packet, CMD_ACCESS_CODE).unwrap();
        assert_eq!((flag, payload), (FLAG_LAST, &[RSP_DONE][..]));
    }

    #[test]
    fn passcode_ends_with_its_digit_sum() {
        assert_eq!(passcode_from(1234), [1, 2, 3, 4, 1, 0]);
        assert_eq!(passcode_from(9999), [9, 9, 9, 9, 3, 6]);
    }
}
