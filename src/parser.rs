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
    try_decode_genius_datetime(packed).unwrap_or_else(|| {
        NaiveDate::from_ymd_opt(2000, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    })
}

/// Same, or `None` when the bits are not a date.
fn try_decode_genius_datetime(packed: u32) -> Option<NaiveDateTime> {
    let hour = packed & 0x1F;
    let minute = (packed >> 5) & 0x3F;
    let day = (packed >> 11) & 0x1F;
    let month = (packed >> 16) & 0x0F;
    let year = ((packed >> 20) & 0x0FFF) as i32;

    NaiveDate::from_ymd_opt(year, month, day).and_then(|d| d.and_hms_opt(hour, minute, 0))
}

/// Decode a packed pair of gradient factors: low in bits 0-6, high in bits 7-13.
fn decode_gradient_factors(packed: u32) -> GradientFactors {
    GradientFactors {
        low: (packed & 0x7F) as u8,
        high: ((packed >> 7) & 0x7F) as u8,
    }
}

/// Water type from the settings field, bits 5-6.
fn decode_water(settings: u32) -> Option<Water> {
    match (settings >> 5) & 0x03 {
        0 => Some(Water::Fresh),
        1 => Some(Water::Salt),
        2 => Some(Water::En13319),
        _ => None,
    }
}

/// Alarm names by bit number, from the SSI app. Bit 0 is not an alarm.
const ALARMS: [&str; 31] = [
    "",
    "slow_down",
    "fast_ascent",
    "uncontrolled_ascent",
    "mod_reached",
    "cns_danger",
    "cns_extreme",
    "missed_deco",
    "dive_violation_deco",
    "low_battery",
    "very_low_battery",
    "probe_low_battery",
    "low_tank_pressure",
    "tank_reserve_reached",
    "tank_lost_link",
    "max_dive_depth",
    "run_away_deco",
    "tank_half_reached",
    "nodeco_2min",
    "nodeco_deco",
    "multigas_atankislow",
    "divetime_halftime",
    "divetime_fulltime",
    "gas_switchpoint",
    "gas_ignored",
    "gas_changed",
    "gas_notchanged",
    "gas_added",
    "rgt_3min",
    "psm_error",
    "ceilcon_1min",
];

/// Names of the alarms set in a `dwAlarms` bitmask (dive header and DPRS).
fn alarm_names(bits: u32) -> Vec<String> {
    (0..32usize)
        .filter(|bit| (bits >> bit) & 1 != 0)
        .map(|bit| match ALARMS.get(bit) {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => format!("bit_{bit}"),
        })
        .collect()
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
/// GENIUS header layout (200 bytes, from libdivecomputer mares_iconhd_parser.c
/// and the SSI app; the fields marked * were worked out from the logs):
///   0x00: type (u16 LE) - must be 1
///   0x02: minor version
///   0x03: major version
///   0x04: dive_number (u32 LE)
///   0x08: datetime (u32 LE, packed bitfield)
///   0x0C: settings (u32 LE)
///   0x14: gradient factor sets 1 and 2 (u32 LE each, packed)
///   0x20: nsamples (u16 LE)
///   0x22: maxdepth (u16 LE, 1/10 m)
///   0x24: avgdepth (u16 LE, 1/10 m)
///   0x26: temperature_max (i16 LE, 1/10 C)
///   0x28: temperature_min (i16 LE, 1/10 C)
///   0x2C: surface interval (u32 LE, seconds)
///   0x30: CNS at the start, at the end (u16 LE each, 1/100 %)
///   0x34: metric units (u8)
///   0x36: OTU at the start, at the end (u16 LE each, 1/100)
///   0x3E: atmospheric pressure (u16 LE, 1/1000 bar)
///   0x40: battery at the end, at the start (u8 each, %) *
///   0x44: maximum ascent speed (u16 LE, 1/10 m/min) *
///   0x4C: alarms raised during the dive (u32 LE, bitmask)
///   0x54: gas mixes / tanks (5 entries, 20 bytes each)
///   0xBC: end datetime (u32 LE, packed bitfield)
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

    // Gradient factor sets at 0x14 and 0x18; an unused set is all zeros
    let gradient_factors = [0x14, 0x18]
        .map(|offset| decode_gradient_factors(read_u32_le(header, offset)))
        .into_iter()
        .filter(|gf| gf.low > 0 || gf.high > 0)
        .collect();

    // Number of samples at 0x20
    let nsamples = read_u16_le(header, 0x20) as u32;

    // Max depth at 0x22 (1/10 meter)
    let max_depth_raw = read_u16_le(header, 0x22);
    let max_depth_m = max_depth_raw as f64 / 10.0;

    // Duration: GENIUS uses fixed 5-second sample interval
    let sample_interval = 5u32;
    let duration_seconds = (nsamples * sample_interval).saturating_sub(surftime_min * 60);

    // Tank size and working pressure are in litres and bar only in metric mode
    let metric = header[0x34] != 0;

    // Gas mixes at 0x54 (5 entries, 20 bytes each)
    let mut gas_mixes = Vec::new();
    for i in 0..5 {
        let gas_offset = 0x54 + i * 20;
        if gas_offset + 12 > header.len() {
            break;
        }
        let gas_params = read_u32_le(header, gas_offset);
        let o2 = (gas_params & 0x7F) as u8;
        let he = ((gas_params >> 14) & 0x7F) as u8;
        let state = ((gas_params >> 21) & 0x03) as u8;
        // state: 0=OFF, 1=READY, 2=INUSE, 3=IGNORED
        if state > 0 && state < 3 && o2 > 0 && o2 <= 100 {
            // Tank pressures in 1/100 bar. Without a transmitter the begin
            // pressure is 0 and the end pressure 0 or 36000.
            let begin = read_u16_le(header, gas_offset + 4);
            let end = read_u16_le(header, gas_offset + 6);
            let tank = (begin != 0 || (end != 0 && end != 36000)).then(|| Tank {
                start_bar: begin as f64 / 100.0,
                end_bar: end as f64 / 100.0,
                volume_l: metric.then(|| read_u16_le(header, gas_offset + 8) as u32),
                working_bar: metric.then(|| read_u16_le(header, gas_offset + 10) as u32),
            });
            gas_mixes.push(GasMix { o2, he, tank });
        }
    }
    if gas_mixes.is_empty() {
        gas_mixes.push(GasMix {
            o2: 21,
            ..Default::default()
        });
    }

    // Parse the samples and the tissue loading from profile data
    let profile = parse_ecop_profile(profile, sample_interval);

    Ok(DiveLog {
        number: if dive_number > 0 { dive_number } else { dive_index + 1 },
        datetime,
        // The header of the first GENIUS firmware stops before this field
        end_datetime: (header.len() >= 0xC0)
            .then(|| try_decode_genius_datetime(read_u32_le(header, 0xBC)))
            .flatten(),
        duration_seconds,
        max_depth_m,
        avg_depth_m: Some(read_u16_le(header, 0x24) as f64 / 10.0),
        min_temp_c: Some(read_u16_le(header, 0x28) as i16 as f64 / 10.0),
        max_temp_c: Some(read_u16_le(header, 0x26) as i16 as f64 / 10.0),
        dive_mode,
        water: decode_water(settings),
        atmospheric_mbar: Some(read_u16_le(header, 0x3E) as u32),
        surface_interval_s: Some(read_u32_le(header, 0x2C)),
        cns_start_pct: Some(read_u16_le(header, 0x30) as f64 / 100.0),
        cns_end_pct: Some(read_u16_le(header, 0x32) as f64 / 100.0),
        otu_start: Some(read_u16_le(header, 0x36) as f64 / 100.0),
        otu_end: Some(read_u16_le(header, 0x38) as f64 / 100.0),
        gradient_factors,
        max_ascent_speed_m_min: Some(read_u16_le(header, 0x44) as f64 / 10.0),
        battery_start_pct: Some(header[0x41]),
        battery_end_pct: Some(header[0x40]),
        alarms: alarm_names(read_u32_le(header, 0x4C)),
        tissue_n2_mbar: profile.tissue_n2_mbar,
        tissue_he_mbar: profile.tissue_he_mbar,
        gas_mixes,
        samples: profile.samples,
        dips: Vec::new(),
        site: None,
        country: None,
        utc_offset: None,
        buddy: None,
        ignored: false,
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
///   0x2C: atmospheric pressure (u16 LE, mbar)
///   0x2E: battery at the end, at the start (u8 each, %)
fn parse_freedive_ecop(dive_index: u32, header: &[u8], data: &[u8]) -> Result<DiveLog> {
    if header.len() < 0x30 {
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
        min_temp_c: Some(read_u16_le(header, 0x20) as i16 as f64 / 10.0),
        dive_mode: DiveMode::Freedive,
        water: decode_water(read_u32_le(header, 0x0C)),
        atmospheric_mbar: Some(read_u16_le(header, 0x2C) as u32),
        battery_start_pct: Some(header[0x2F]),
        battery_end_pct: Some(header[0x2E]),
        dips: parse_freedive_dips(data),
        ..Default::default()
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

/// What the records of a profile hold.
struct Profile {
    samples: Vec<Sample>,
    tissue_n2_mbar: Vec<f64>,
    tissue_he_mbar: Vec<f64>,
}

/// Parse the TISS, DPRS (depth/pressure) and AIRS records of ECOP profile data.
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
///
/// TISS payload: 16 compartments, each N2 then He pressure (f32 LE, mbar)
///
/// DPRS record (field names from the SSI app, units checked on the logs):
///   4: wDepth (u16 LE, 1/10 m)       6: absolute pressure (u16 LE, mbar)
///   8: swTemp (i16 LE, 1/10 C)      10: swSpeed (i16 LE, 1/10 m/min, + is up)
///   14: wNDLorASC (u16 LE, minutes): no-deco time, or deco time in deco
///   16: dwAlarms (u32 LE, bitmask)
///   20: gradient factor now, 22: on surfacing (i16 LE each, 1/10 %)
///   24: misc (u32 LE): bits 0-1 gradient factor set, 2-5 bookmark,
///       6-9 gas mix, 16-17 safety stop (1 due, 2 running, 3 paused),
///       18 deco stop required, 19-25 its depth in m
///
/// AIRS record (bytes 6 and 7 worked out from the logs):
///   4: pressure (u16 LE, 1/100 bar)
///   6: remaining gas time (u8, minutes; 254 = no reading, 255 = no transmitter)
///   7: gas consumption (u8, l/min at surface pressure)
fn parse_ecop_profile(profile: &[u8], sample_interval: u32) -> Profile {
    let mut samples = Vec::new();
    let mut tissue_n2_mbar = Vec::new();
    let mut tissue_he_mbar = Vec::new();
    let mut time_s = 0u32;
    let mut last_pressure_bar: Option<f64> = None;
    let mut last_gas_time_min: Option<u32> = None;
    let mut last_sac_l_min: Option<u32> = None;

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
                // Only the loading at the start of the dive is kept
                if tissue_n2_mbar.is_empty() && offset + RECORD_TISS <= profile.len() {
                    let pressure = |at: usize| {
                        let raw = f32::from_bits(read_u32_le(profile, offset + 4 + at * 4));
                        (raw as f64 * 10.0).round() / 10.0
                    };
                    tissue_n2_mbar = (0..16).map(|i| pressure(i * 2)).collect();
                    tissue_he_mbar = (0..16).map(|i| pressure(i * 2 + 1)).collect();
                    if tissue_he_mbar.iter().all(|&he| he == 0.0) {
                        tissue_he_mbar.clear();
                    }
                }
                offset += RECORD_TISS;
            }
            b"DPRS" => {
                if offset + RECORD_DPRS > profile.len() {
                    break;
                }
                let record = &profile[offset..offset + RECORD_DPRS];

                // Depth at bytes 4-5 (after tag), LE u16, 1/10 meter
                let depth_raw = read_u16_le(record, 4);
                let depth_m = depth_raw as f64 / 10.0;

                // Temperature at bytes 8-9 (offset+4+4), LE u16, 1/10 deg C
                let temp_raw = read_u16_le(record, 8) as i16;
                let temp_c = if temp_raw > 0 {
                    Some(temp_raw as f64 / 10.0)
                } else {
                    None
                };

                // One field counts the no-deco time, then the deco time
                let minutes = read_u16_le(record, 14) as u32;
                let misc = read_u32_le(record, 24);
                let in_deco = (misc >> 18) & 0x01 != 0;

                samples.push(Sample {
                    time_s,
                    depth_m,
                    temp_c,
                    pressure_bar: last_pressure_bar,
                    ambient_mbar: Some(read_u16_le(record, 6) as u32),
                    speed_m_min: Some(read_u16_le(record, 10) as i16 as f64 / 10.0),
                    ndl_min: (!in_deco).then_some(minutes),
                    deco_time_min: in_deco.then_some(minutes),
                    deco_stop_m: in_deco.then_some((misc >> 19) & 0x7F),
                    gf_pct: Some(read_u16_le(record, 20) as i16 as f64 / 10.0),
                    surface_gf_pct: Some(read_u16_le(record, 22) as i16 as f64 / 10.0),
                    gas: ((misc >> 6) & 0x0F) as u8,
                    gf_set: (misc & 0x03) as u8,
                    bookmark: ((misc >> 2) & 0x0F) as u8,
                    safety_stop: match (misc >> 16) & 0x03 {
                        1 => Some(SafetyStop::Due),
                        2 => Some(SafetyStop::Running),
                        3 => Some(SafetyStop::Paused),
                        _ => None,
                    },
                    alarms: alarm_names(read_u32_le(record, 16)),
                    gas_time_min: last_gas_time_min,
                    sac_l_min: last_sac_l_min,
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

                // Gas time 254 or 255: the watch has no reading to work from
                let gas_time = profile[offset + 6];
                let known = gas_time < 254;
                last_gas_time_min = known.then_some(gas_time as u32);
                last_sac_l_min = known.then_some(profile[offset + 7] as u32);

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

    Profile {
        samples,
        tissue_n2_mbar,
        tissue_he_mbar,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u16(data: &mut [u8], offset: usize, value: u16) {
        data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// A scuba header with the values of a real dive (#143).
    fn header() -> Vec<u8> {
        let mut h = vec![0u8; 200];
        put_u16(&mut h, 0x00, 1);
        put_u32(&mut h, 0x04, 143);
        put_u32(&mut h, 0x08, 0x7EAA_204A); // 2026-10-04 10:02
        put_u32(&mut h, 0x0C, 0x61A0); // air, salt water, 3 min surface timeout
        put_u32(&mut h, 0x14, 0x0040_2AD5); // GF 85/85
        put_u32(&mut h, 0x18, 0x0040_2FDF); // GF 95/95
        put_u16(&mut h, 0x22, 340);
        put_u16(&mut h, 0x24, 162);
        put_u16(&mut h, 0x26, 261);
        put_u16(&mut h, 0x28, 204);
        put_u32(&mut h, 0x2C, 16180);
        put_u16(&mut h, 0x30, 21);
        put_u16(&mut h, 0x32, 565);
        h[0x34] = 1;
        put_u16(&mut h, 0x36, 54);
        put_u16(&mut h, 0x38, 1462);
        put_u16(&mut h, 0x3E, 1048);
        h[0x40] = 53;
        h[0x41] = 58;
        put_u16(&mut h, 0x44, 149);
        put_u32(&mut h, 0x4C, 0x0008_4002);
        // Gas 0: air in use, on a 12 l tank with a transmitter
        put_u32(&mut h, 0x54, 0x00C0_2795);
        put_u16(&mut h, 0x58, 20910);
        put_u16(&mut h, 0x5A, 9900);
        put_u16(&mut h, 0x5C, 12);
        put_u16(&mut h, 0x5E, 200);
        // Gas 1: off
        put_u32(&mut h, 0x68, 0x0000_2795);
        put_u16(&mut h, 0x6E, 36000);
        put_u32(&mut h, 0xBC, 0x7EAA_21CB); // 11:14
        h
    }

    /// A record as the watch frames it: tag, payload, CRC, tag again.
    fn record(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut record = tag.to_vec();
        record.extend(payload);
        record.extend([0, 0]); // the CRC is not checked
        record.extend(tag);
        record
    }

    #[allow(clippy::too_many_arguments)]
    fn dprs(
        depth: u16,
        ambient: u16,
        temp: i16,
        speed: i16,
        minutes: u16,
        alarms: u32,
        gf: [i16; 2],
        misc: u32,
    ) -> Vec<u8> {
        let mut payload = vec![0u8; 24];
        put_u16(&mut payload, 0, depth);
        put_u16(&mut payload, 2, ambient);
        put_u16(&mut payload, 4, temp as u16);
        put_u16(&mut payload, 6, speed as u16);
        put_u16(&mut payload, 10, minutes);
        put_u32(&mut payload, 12, alarms);
        put_u16(&mut payload, 16, gf[0] as u16);
        put_u16(&mut payload, 18, gf[1] as u16);
        put_u32(&mut payload, 20, misc);
        record(b"DPRS", &payload)
    }

    fn airs(pressure: u16, gas_time: u8, consumption: u8) -> Vec<u8> {
        let mut payload = vec![0u8; 6];
        put_u16(&mut payload, 0, pressure);
        payload[2] = gas_time;
        payload[3] = consumption;
        record(b"AIRS", &payload)
    }

    fn profile() -> Vec<u8> {
        let mut tissues = Vec::new();
        for _ in 0..16 {
            tissues.extend(725.489f32.to_le_bytes());
            tissues.extend(0f32.to_le_bytes());
        }

        let mut profile = vec![0, 0, 0, 2];
        profile.extend(record(b"DSTR", &[0; 48]));
        profile.extend(record(b"TISS", &tissues));
        // Within the no-deco limit, deeper than the safety stop threshold
        profile.extend(dprs(256, 3629, 254, -32, 17, 0, [-421, 236], 0x0801_0000));
        profile.extend(airs(19050, 28, 18));
        // Into deco: a stop at 3 m
        profile.extend(dprs(
            329,
            4363,
            209,
            73,
            1,
            0x0008_0002,
            [-259, 898],
            0x081D_4800,
        ));
        // The transmitter is lost: the pressure stays, the gas time goes
        profile.extend(airs(17800, 254, 0));
        // Safety stop under way, second gas and gradient factor set, bookmark 2
        profile.extend(dprs(48, 1540, 260, 22, 99, 0, [242, 852], 0x0802_0049));
        profile.extend(record(b"DEND", &[0; 152]));
        profile
    }

    #[test]
    fn header_fields() {
        let dive = parse_dive_ecop(0, &header(), &[]).unwrap();

        assert_eq!(dive.number, 143);
        assert_eq!(dive.datetime.to_string(), "2026-10-04 10:02:00");
        assert_eq!(
            dive.end_datetime.unwrap().to_string(),
            "2026-10-04 11:14:00"
        );
        assert_eq!(dive.max_depth_m, 34.0);
        assert_eq!(dive.avg_depth_m, Some(16.2));
        assert_eq!((dive.min_temp_c, dive.max_temp_c), (Some(20.4), Some(26.1)));
        assert_eq!(dive.water, Some(Water::Salt));
        assert_eq!(dive.atmospheric_mbar, Some(1048));
        assert_eq!(dive.surface_interval_s, Some(16180));
        assert_eq!(
            (dive.cns_start_pct, dive.cns_end_pct),
            (Some(0.21), Some(5.65))
        );
        assert_eq!((dive.otu_start, dive.otu_end), (Some(0.54), Some(14.62)));
        assert_eq!(
            dive.gradient_factors,
            [
                GradientFactors { low: 85, high: 85 },
                GradientFactors { low: 95, high: 95 }
            ]
        );
        assert_eq!(dive.max_ascent_speed_m_min, Some(14.9));
        assert_eq!(
            (dive.battery_start_pct, dive.battery_end_pct),
            (Some(58), Some(53))
        );
        assert_eq!(dive.alarms, ["slow_down", "tank_lost_link", "nodeco_deco"]);
    }

    #[test]
    fn tank_comes_with_its_gas_mix() {
        let dive = parse_dive_ecop(0, &header(), &[]).unwrap();
        assert_eq!(dive.gas_mixes.len(), 1);
        assert_eq!((dive.gas_mixes[0].o2, dive.gas_mixes[0].he), (21, 0));
        let tank = dive.gas_mixes[0].tank.as_ref().unwrap();
        assert_eq!((tank.start_bar, tank.end_bar), (209.1, 99.0));
        assert_eq!((tank.volume_l, tank.working_bar), (Some(12), Some(200)));

        // Without a transmitter: begin pressure 0, end pressure 36000
        let mut header = header();
        put_u16(&mut header, 0x58, 0);
        put_u16(&mut header, 0x5A, 36000);
        let dive = parse_dive_ecop(0, &header, &[]).unwrap();
        assert!(dive.gas_mixes[0].tank.is_none());
    }

    #[test]
    fn sample_fields() {
        let dive = parse_dive_ecop(0, &header(), &profile()).unwrap();
        let [bottom, deco, stop] = &dive.samples[..] else {
            panic!("expected 3 samples, got {}", dive.samples.len());
        };

        assert_eq!(
            (bottom.time_s, bottom.depth_m, bottom.temp_c),
            (0, 25.6, Some(25.4))
        );
        assert_eq!(bottom.ambient_mbar, Some(3629));
        assert_eq!(bottom.speed_m_min, Some(-3.2));
        assert_eq!(
            (bottom.ndl_min, bottom.deco_time_min, bottom.deco_stop_m),
            (Some(17), None, None)
        );
        assert_eq!(
            (bottom.gf_pct, bottom.surface_gf_pct),
            (Some(-42.1), Some(23.6))
        );
        assert_eq!(bottom.safety_stop, Some(SafetyStop::Due));
        assert!(bottom.alarms.is_empty());
        // The first tank reading comes after this sample
        assert_eq!(
            (bottom.pressure_bar, bottom.gas_time_min, bottom.sac_l_min),
            (None, None, None)
        );

        assert_eq!(
            (deco.ndl_min, deco.deco_time_min, deco.deco_stop_m),
            (None, Some(1), Some(3))
        );
        assert_eq!(deco.speed_m_min, Some(7.3));
        assert_eq!(deco.alarms, ["slow_down", "nodeco_deco"]);
        assert_eq!(
            (deco.pressure_bar, deco.gas_time_min, deco.sac_l_min),
            (Some(190.5), Some(28), Some(18))
        );
        assert_eq!((deco.gas, deco.gf_set, deco.bookmark), (0, 0, 0));

        assert_eq!(stop.safety_stop, Some(SafetyStop::Running));
        assert_eq!((stop.gas, stop.gf_set, stop.bookmark), (1, 1, 2));
        assert_eq!(
            (stop.pressure_bar, stop.gas_time_min, stop.sac_l_min),
            (Some(178.0), None, None)
        );
    }

    #[test]
    fn tissue_loading_at_the_start() {
        let dive = parse_dive_ecop(0, &header(), &profile()).unwrap();
        assert_eq!(dive.tissue_n2_mbar, [725.5; 16]);
        // No helium in any compartment: left out
        assert!(dive.tissue_he_mbar.is_empty());
    }

    #[test]
    fn no_gas_time_without_a_transmitter() {
        // What every AIRS record holds on a dive without a transmitter
        let mut profile = vec![0, 0, 0, 2];
        profile.extend(airs(0, 255, 0));
        profile.extend(dprs(256, 3629, 254, 0, 17, 0, [0, 0], 0));
        let dive = parse_dive_ecop(0, &header(), &profile).unwrap();

        let sample = &dive.samples[0];
        assert_eq!(
            (sample.pressure_bar, sample.gas_time_min, sample.sac_l_min),
            (None, None, None)
        );
    }

    #[test]
    fn alarm_bits_without_a_name_are_kept() {
        assert!(alarm_names(0).is_empty());
        assert_eq!(alarm_names(1 << 28 | 1 << 31), ["rgt_3min", "bit_31"]);
    }
}
