use anyhow::{bail, Result};
use chrono::{NaiveDate, NaiveDateTime};

use crate::types::*;

/// Read a u16 from a byte slice at the given offset (little-endian).
fn read_u16_le(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

/// Read a u32 from a byte slice at the given offset (little-endian).
fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Decode the Mares GENIUS packed datetime format (32-bit LE bitfield).
///
/// Bit layout:
///   bits  0-4:  hour (0-23)
///   bits  5-10: minute (0-59)
///   bits 11-15: day (1-31)
///   bits 16-19: month (1-12)
///   bits 20-31: year (absolute, e.g. 2025)
fn decode_genius_datetime(packed: u32) -> NaiveDateTime {
    let hour = packed & 0x1F;
    let minute = (packed >> 5) & 0x3F;
    let day = (packed >> 11) & 0x1F;
    let month = (packed >> 16) & 0x0F;
    let year = ((packed >> 20) & 0x0FFF) as i32;

    NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|d| d.and_hms_opt(hour, minute, 0))
        .unwrap_or_else(|| {
            NaiveDate::from_ymd_opt(2000, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        })
}

/// Extract the dive number from a raw 200-byte header without doing a full parse.
/// The dive number is at offset 0x04 as a u32 LE.
pub fn dive_number_from_header(header: &[u8]) -> u32 {
    if header.len() >= 8 {
        read_u32_le(header, 0x04)
    } else {
        0
    }
}

/// Parse a dive from ECOP protocol data (header + profile).
///
/// GENIUS header layout (200 bytes, from libdivecomputer mares_iconhd_parser.c):
///   0x00: type (u16 LE) - must be 1
///   0x02: minor version
///   0x03: major version
///   0x04: dive_number (u32 LE)
///   0x08: datetime (u32 LE, packed bitfield)
///   0x0C: settings (u32 LE)
///   0x20: nsamples (u16 LE)
///   0x22: maxdepth (u16 LE, 1/10 m)
///   0x26: temperature_max (u16 LE, 1/10 C)
///   0x28: temperature_min (u16 LE, 1/10 C)
///   0x3E: atmospheric pressure (u16 LE, 1/1000 bar)
///   0x54: gas mixes / tanks (5 entries, 20 bytes each)
pub fn parse_dive_ecop(dive_index: u32, header: &[u8], profile: &[u8]) -> Result<DiveLog> {
    if is_freedive_header(header) {
        return parse_freedive_ecop(dive_index, header, profile);
    }

    if header.len() < 0x60 {
        bail!("Dive header too short: {} bytes", header.len());
    }

    // Dive number at 0x04
    let dive_number = read_u32_le(header, 0x04);

    // Packed datetime at 0x08
    let ts_packed = read_u32_le(header, 0x08);
    let datetime = decode_genius_datetime(ts_packed);

    // Settings at 0x0C
    let settings = read_u32_le(header, 0x0C);
    let mode_val = settings & 0x0F;
    let dive_mode = match mode_val {
        0 => DiveMode::Air,
        1 | 2 | 3 | 6 | 7 => DiveMode::Nitrox,
        4 => DiveMode::Gauge,
        5 => DiveMode::Freedive,
        _ => DiveMode::Air,
    };
    // Surface time in minutes from settings bits 13-18
    let surftime_min = (settings >> 13) & 0x3F;

    // Number of samples at 0x20
    let nsamples = read_u16_le(header, 0x20) as u32;

    // Max depth at 0x22 (1/10 meter)
    let max_depth_raw = read_u16_le(header, 0x22);
    let max_depth_m = max_depth_raw as f64 / 10.0;

    // Duration: GENIUS uses fixed 5-second sample interval
    let sample_interval = 5u32;
    let duration_seconds = (nsamples * sample_interval).saturating_sub(surftime_min * 60);

    // Gas mixes at 0x54 (5 entries, 20 bytes each)
    let mut gas_mixes = Vec::new();
    for i in 0..5 {
        let gas_offset = 0x54 + i * 20;
        if gas_offset + 4 > header.len() {
            break;
        }
        let gas_params = read_u32_le(header, gas_offset);
        let o2 = (gas_params & 0x7F) as u8;
        let state = ((gas_params >> 21) & 0x03) as u8;
        // state: 0=OFF, 1=READY, 2=INUSE, 3=IGNORED
        if state > 0 && state < 3 && o2 > 0 && o2 <= 100 {
            gas_mixes.push(GasMix { o2 });
        }
    }
    if gas_mixes.is_empty() {
        gas_mixes.push(GasMix { o2: 21 });
    }

    // Parse DPRS samples from profile data
    let samples = parse_ecop_profile(profile, sample_interval);

    Ok(DiveLog {
        number: if dive_number > 0 { dive_number } else { dive_index + 1 },
        datetime,
        duration_seconds,
        max_depth_m,
        dive_mode,
        gas_mixes,
        samples,
        dips: Vec::new(),
        site: None,
        country: None,
        buddy: None,
    })
}

/// Header type (u16 LE at offset 0) of a freedive session; scuba dives use 1.
const HEADER_TYPE_FREEDIVE: u16 = 4;

/// Whether a raw header (sub-index 4) describes a freedive session.
/// Its data must then be read from sub-index 5 instead of 3.
pub fn is_freedive_header(header: &[u8]) -> bool {
    header.len() >= 2 && read_u16_le(header, 0) == HEADER_TYPE_FREEDIVE
}

/// Parse a freedive session from ECOP protocol data (header + sub-index 5 data).
///
/// Session header layout (64 bytes, field names from the SSI app):
///   0x00: type (u16 LE) - 4
///   0x04: dive_number (u32 LE)
///   0x08: datetime of the session start (u32 LE, packed bitfield)
///   0x0C: settings (u32 LE), mode = 5
///   0x14: session time (u16 LE, seconds)
///   0x16: time under water (u16 LE, seconds)
///   0x1C: number of dips (u16 LE)
///   0x20: temperature_min (i16 LE, 1/10 C)
///   0x24: maxdepth (u16 LE, 1/10 m)
fn parse_freedive_ecop(dive_index: u32, header: &[u8], data: &[u8]) -> Result<DiveLog> {
    if header.len() < 0x26 {
        bail!("Freedive header too short: {} bytes", header.len());
    }

    let dive_number = read_u32_le(header, 0x04);
    let datetime = decode_genius_datetime(read_u32_le(header, 0x08));
    let duration_seconds = read_u16_le(header, 0x14) as u32;
    let max_depth_m = read_u16_le(header, 0x24) as f64 / 10.0;

    Ok(DiveLog {
        number: if dive_number > 0 { dive_number } else { dive_index + 1 },
        datetime,
        duration_seconds,
        max_depth_m,
        dive_mode: DiveMode::Freedive,
        gas_mixes: Vec::new(),
        samples: Vec::new(),
        dips: parse_freedive_dips(data),
        site: None,
        country: None,
        buddy: None,
    })
}

/// Freedive record sizes, tags and CRC included (firmware 01.10.00).
const RECORD_FSTR: usize = 50;
const RECORD_FEND: usize = 38;
const RECORD_FHDR: usize = 22;

/// Parse the FHDR records (one per dip) of a freedive session.
///
/// Data structure:
///   [4 bytes] object classifier (type 0x20, minor, major)
///   [FSTR 50 bytes] session start record
///   [FEND 38 bytes] session end record
///   [FHDR 22 bytes]* one per dip
///
/// FHDR payload (after the tag): dip number(2) + max depth(2, 1/10 m) +
/// surface time(2, s) + dive time(2, s) + min temp(2, 1/10 C) + ?(2)
fn parse_freedive_dips(data: &[u8]) -> Vec<Dip> {
    let mut dips = Vec::new();

    // Skip the 4-byte SObjectClassifier at the start
    let mut offset = if data.len() >= 8 && &data[4..8] == b"FSTR" {
        4
    } else {
        0
    };

    while offset + 4 <= data.len() {
        match &data[offset..offset + 4] {
            b"FSTR" => {
                offset += RECORD_FSTR;
            }
            b"FEND" => {
                offset += RECORD_FEND;
            }
            b"FHDR" => {
                if offset + RECORD_FHDR > data.len() {
                    break;
                }

                let temp_raw = read_u16_le(data, offset + 12) as i16;
                dips.push(Dip {
                    surface_s: read_u16_le(data, offset + 8) as u32,
                    duration_s: read_u16_le(data, offset + 10) as u32,
                    max_depth_m: read_u16_le(data, offset + 6) as f64 / 10.0,
                    min_temp_c: if temp_raw > 0 {
                        Some(temp_raw as f64 / 10.0)
                    } else {
                        None
                    },
                });

                offset += RECORD_FHDR;
            }
            _ => {
                // Unknown data, scan forward for next known tag
                offset += 1;
            }
        }
    }

    dips
}

/// Known record sizes from libdivecomputer (mares_iconhd_parser.c).
const RECORD_DSTR: usize = 58;
const RECORD_TISS: usize = 138;
const RECORD_DPRS: usize = 34;
const RECORD_AIRS: usize = 16;
const RECORD_DEND: usize = 162;

/// Parse DPRS (depth/pressure) and AIRS samples from ECOP profile data.
///
/// Profile structure:
///   [4 bytes] profile version (type u16 LE, minor u8, major u8)
///   [DSTR 58 bytes] dive start record
///   [TISS 138 bytes] tissue loading
///   [DPRS 34 bytes]* depth/pressure samples (nsamples count)
///   [AIRS 16 bytes]  air supply records (interleaved)
///   [DEND 162 bytes] dive end record (if present)
///
/// Each record: [4-byte tag] [payload] [2-byte CRC] [4-byte tag repeated]
/// DPRS payload (bytes 4-27): depth(2) + ?(2) + temp(2) + ...
/// AIRS payload (bytes 4-9): pressure(2) + ...
fn parse_ecop_profile(profile: &[u8], sample_interval: u32) -> Vec<Sample> {
    let mut samples = Vec::new();
    let mut time_s = 0u32;
    let mut last_pressure_bar: Option<f64> = None;

    // Skip the 4-byte SObjectClassifier at the start
    let mut offset = if profile.len() >= 8 && &profile[4..8] == b"DSTR" {
        4
    } else {
        0
    };

    while offset + 4 <= profile.len() {
        let tag = &profile[offset..offset + 4];

        match tag {
            b"DSTR" => {
                offset += RECORD_DSTR;
            }
            b"TISS" => {
                offset += RECORD_TISS;
            }
            b"DPRS" => {
                if offset + RECORD_DPRS > profile.len() {
                    break;
                }

                // Depth at bytes 4-5 (after tag), LE u16, 1/10 meter
                let depth_raw = read_u16_le(profile, offset + 4);
                let depth_m = depth_raw as f64 / 10.0;

                // Temperature at bytes 8-9 (offset+4+4), LE u16, 1/10 deg C
                let temp_raw = read_u16_le(profile, offset + 8) as i16;
                let temp_c = if temp_raw > 0 {
                    Some(temp_raw as f64 / 10.0)
                } else {
                    None
                };

                samples.push(Sample {
                    time_s,
                    depth_m,
                    temp_c,
                    pressure_bar: last_pressure_bar,
                });

                time_s += sample_interval;
                offset += RECORD_DPRS;
            }
            b"AIRS" => {
                if offset + RECORD_AIRS > profile.len() {
                    break;
                }

                // Pressure at bytes 4-5, LE u16, 1/100 bar
                let pressure_raw = read_u16_le(profile, offset + 4);
                if pressure_raw > 0 {
                    last_pressure_bar = Some(pressure_raw as f64 / 100.0);
                }

                offset += RECORD_AIRS;
            }
            b"DEND" => {
                offset += RECORD_DEND;
            }
            _ => {
                // Unknown data, scan forward for next known tag
                offset += 1;
            }
        }
    }

    samples
}

/// Export a dive as CSV.
pub fn dive_to_csv(dive: &DiveLog) -> String {
    let mut csv = String::from("time_s,depth_m,temp_c,pressure_bar\n");
    for s in &dive.samples {
        csv.push_str(&format!(
            "{},{:.1},{},{}",
            s.time_s,
            s.depth_m,
            s.temp_c
                .map(|t| format!("{t:.1}"))
                .unwrap_or_default(),
            s.pressure_bar
                .map(|p| format!("{p:.1}"))
                .unwrap_or_default(),
        ));
        csv.push('\n');
    }
    csv
}
