//! Flash layout and dive format of the Apeks DSX.
//!
//! Ported from the DSX branches of libdivecomputer `oceanic_atom2_parser.c`
//! and `oceanic_common.c`, with field names from DiverLog+.
//! Not yet checked against a real dive.

use anyhow::{bail, Result};
use chrono::{NaiveDate, NaiveDateTime};

use crate::types::*;

pub const LOGBOOK_BEGIN: u32 = 0x0080_0000;
pub const LOGBOOK_END: u32 = 0x0088_0000;
pub const LOGBOOK_ENTRY: u32 = 512;
pub const PROFILE_BEGIN: u32 = 0x0100_0000;
pub const PROFILE_END: u32 = 0x0200_0000;

const HEADER_SIZE: usize = 256;
const SAMPLE_SIZE: usize = 32;
const FOOTER_SIZE: usize = 64;
/// The no-deco time reads hundreds of minutes in shallow water: no limit
const NO_LIMIT_NDL: u16 = 500;

const FEET: f64 = 0.3048;
const PSI: f64 = 0.068_947_572_9;

// Dive modes (logbook entry offset 45)
const MODE_CC: u8 = 0;
const MODE_OC: u8 = 1;
const MODE_SIDEMOUNT: u8 = 2;

fn read_u16_le(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

/// A GPS position, kept to about a metre; zeros when there was no fix.
fn read_position(data: &[u8], offset: usize) -> Option<Position> {
    let degrees = |at: usize| {
        let value = f32::from_bits(read_u32_le(data, at)) as f64;
        (value * 100_000.0).round() / 100_000.0
    };
    let (lat, lon) = (degrees(offset), degrees(offset + 4));
    let valid = lat.abs() <= 90.0 && lon.abs() <= 180.0 && (lat != 0.0 || lon != 0.0);
    valid.then_some(Position { lat, lon })
}

fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Bytes from `from` up to `to` going forward in the ring `[begin, end)`.
/// Equal pointers count as a full ring.
pub fn ring_distance(from: u32, to: u32, begin: u32, end: u32) -> u32 {
    let size = end - begin;
    if to > from {
        to - from
    } else {
        size - (from - to)
    }
}

/// Step `delta` bytes back from `address` in the ring `[begin, end)`.
pub fn ring_decrement(address: u32, delta: u32, begin: u32, end: u32) -> u32 {
    let size = end - begin;
    begin + (address - begin + size - delta % size) % size
}

/// Ring pointers from the 256-byte flash map.
#[derive(Debug, Clone, Copy)]
pub struct FlashMap {
    /// Oldest logbook entry
    pub logbook_first: u32,
    /// One past the newest logbook entry
    pub logbook_last: u32,
    pub profile_first: u32,
    pub profile_last: u32,
}

impl FlashMap {
    /// Flash map layout (u32 LE):
    ///   0x50: logbook area min     0x54: logbook area max (inclusive)
    ///   0x58: logbook first        0x5C: logbook last
    ///   0x70: profile area min     0x74: profile area max (inclusive)
    ///   0x78: profile first        0x7C: profile last
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 0x80 {
            bail!("Flash map too short: {} bytes", data.len());
        }

        // A different area means a memory layout this code does not know
        let areas = [
            ("logbook", 0x50, LOGBOOK_BEGIN, LOGBOOK_END),
            ("profile", 0x70, PROFILE_BEGIN, PROFILE_END),
        ];
        for (name, offset, begin, end) in areas {
            let min = read_u32_le(data, offset);
            let max = read_u32_le(data, offset + 4);
            if min != begin || max.wrapping_add(1) != end {
                bail!("Unexpected {name} area 0x{min:08X}..=0x{max:08X}");
            }
        }

        Ok(Self {
            logbook_first: read_u32_le(data, 0x58),
            logbook_last: read_u32_le(data, 0x5C),
            profile_first: read_u32_le(data, 0x78),
            profile_last: read_u32_le(data, 0x7C),
        })
    }

    /// Flash addresses of the logbook entries in use, newest first.
    pub fn logbook_entries(&self) -> Result<Vec<u32>> {
        for pointer in [self.logbook_first, self.logbook_last] {
            if !(LOGBOOK_BEGIN..=LOGBOOK_END).contains(&pointer) {
                bail!("Logbook pointer 0x{pointer:08X} outside the logbook area");
            }
        }
        let used = ring_distance(
            self.logbook_first,
            self.logbook_last,
            LOGBOOK_BEGIN,
            LOGBOOK_END,
        );
        Ok((1..=used / LOGBOOK_ENTRY)
            .map(|n| {
                ring_decrement(
                    self.logbook_last,
                    n * LOGBOOK_ENTRY,
                    LOGBOOK_BEGIN,
                    LOGBOOK_END,
                )
            })
            .collect())
    }
}

/// Serial number from the hardware calibration block (BCD at offset 12).
pub fn serial_number(hw_cal: &[u8]) -> Option<u32> {
    let bcd = |byte: u8| (byte >> 4) as u32 * 10 + (byte & 0x0F) as u32;
    let bytes = hw_cal.get(12..15)?;
    Some(bcd(bytes[0]) + bcd(bytes[1]) * 100 + bcd(bytes[2]) * 10_000)
}

/// What a 512-byte logbook entry says about its dive.
///
/// Logbook entry layout:
///   0: dive number (u16 LE)   3: hour       4: minute
///   5: day                    6: month      7: year - 2000
///   8: profile start address (u32 LE)
///   12: profile end address (u32 LE)
///   18: average depth (u16 LE, 1/10 ft)
///   45: dive mode (0 CC, 1 OC, 2 sidemount, 3 sidemount gauge, 4 gauge)
///   240: start latitude, longitude (f32 LE, degrees; 0 without a fix)
///   248: end latitude, longitude
#[derive(Debug, Clone)]
pub struct LogbookEntry {
    pub number: u32,
    pub datetime: NaiveDateTime,
    pub profile_begin: u32,
    pub profile_end: u32,
    pub mode: u8,
    pub avg_depth_m: f64,
    pub start_position: Option<Position>,
    pub end_position: Option<Position>,
}

impl LogbookEntry {
    /// Returns `None` for an unused entry (erased flash).
    pub fn parse(entry: &[u8]) -> Result<Option<Self>> {
        if entry.len() < LOGBOOK_ENTRY as usize {
            bail!("Logbook entry too short: {} bytes", entry.len());
        }
        if entry.iter().all(|&byte| byte == 0xFF) {
            return Ok(None);
        }

        let datetime =
            NaiveDate::from_ymd_opt(2000 + entry[7] as i32, entry[6] as u32, entry[5] as u32)
                .and_then(|date| date.and_hms_opt(entry[3] as u32, entry[4] as u32, 0));
        let Some(datetime) = datetime else {
            bail!(
                "Invalid date in logbook entry: {:02X} {:02X} {:02X} {:02X} {:02X}",
                entry[3],
                entry[4],
                entry[5],
                entry[6],
                entry[7]
            );
        };

        Ok(Some(Self {
            number: read_u16_le(entry, 0) as u32,
            datetime,
            profile_begin: read_u32_le(entry, 8),
            profile_end: read_u32_le(entry, 12),
            mode: entry[45],
            avg_depth_m: read_u16_le(entry, 18) as f64 / 10.0 * FEET,
            start_position: read_position(entry, 240),
            end_position: read_position(entry, 248),
        }))
    }

    /// The two flash ranges (the second may be empty) holding the profile.
    pub fn profile_ranges(&self) -> Result<[(u32, u32); 2]> {
        for pointer in [self.profile_begin, self.profile_end] {
            if !(PROFILE_BEGIN..=PROFILE_END).contains(&pointer) {
                bail!("Profile pointer 0x{pointer:08X} outside the profile area");
            }
        }
        if self.profile_begin == self.profile_end {
            bail!("Empty profile");
        }
        Ok(if self.profile_end > self.profile_begin {
            [
                (self.profile_begin, self.profile_end - self.profile_begin),
                (PROFILE_BEGIN, 0),
            ]
        } else {
            // The profile wraps around the end of the ring
            [
                (self.profile_begin, PROFILE_END - self.profile_begin),
                (PROFILE_BEGIN, self.profile_end - PROFILE_BEGIN),
            ]
        })
    }
}

/// Parse a dive from its logbook entry and profile region.
///
/// Profile region: 256-byte header, 32-byte samples, 64-byte footer.
/// Units on the device are imperial: 1/10 ft, 1/10 F, psi.
///
/// Header:
///   36: sample interval (seconds)
///   0x89 + mode * 16: O2 % of gases 1-6
///   0xB9 + mode * 16: He % of gases 1-6
/// Footer:
///   8: dive time (u16 LE, seconds)
///   10: max depth (u16 LE, 1/10 ft)
/// Sample:
///   0: gas / tank number in the high nibble
///   2: depth (u16 LE, 1/10 ft)
///   10: temperature (u16 LE, 1/10 F)
///   14: tank pressure (u16 LE, psi)
pub fn parse_dive(entry: &LogbookEntry, profile: &[u8]) -> Result<DiveLog> {
    if profile.len() < HEADER_SIZE + FOOTER_SIZE {
        bail!("Dive profile too short: {} bytes", profile.len());
    }

    let footer = profile.len() - FOOTER_SIZE;
    let duration_seconds = read_u16_le(profile, footer + 8) as u32;
    let max_depth_m = read_u16_le(profile, footer + 10) as f64 / 10.0 * FEET;

    // Erased or zeroed slots are padding, not samples
    let slots: Vec<&[u8; SAMPLE_SIZE]> = profile[HEADER_SIZE..footer]
        .as_chunks::<SAMPLE_SIZE>()
        .0
        .iter()
        .filter(|sample| {
            !sample.iter().all(|&byte| byte == 0x00) && !sample.iter().all(|&byte| byte == 0xFF)
        })
        .collect();

    // The header lists every gas set on the computer, the samples tell which
    // were breathed. Gauge modes have no gas table.
    let mut gas_mixes = Vec::new();
    let mut gas_numbers = Vec::new();
    if entry.mode <= MODE_SIDEMOUNT {
        let table = entry.mode as usize * 16;
        for number in 1..=6u8 {
            if !slots.iter().any(|sample| sample[0] >> 4 == number) {
                continue;
            }
            let o2 = profile[0x89 + table + number as usize - 1];
            let he = profile[0xB9 + table + number as usize - 1];
            if o2 > 0 && o2 as u32 + he as u32 <= 100 {
                gas_mixes.push(GasMix { o2, he, tank: None });
                gas_numbers.push(number);
            }
        }
    }

    let dive_mode = match entry.mode {
        MODE_CC => DiveMode::Ccr,
        MODE_OC | MODE_SIDEMOUNT => {
            if gas_mixes.iter().all(|gas| gas.o2 == 21 && gas.he == 0) {
                DiveMode::Air
            } else {
                DiveMode::Nitrox
            }
        }
        _ => DiveMode::Gauge,
    };

    let interval = (profile[36] as u32).max(1);
    let mut samples = Vec::new();
    let mut time_s = 0;
    for sample in slots {
        time_s += interval;

        let temperature = read_u16_le(sample, 10);
        let pressure = read_u16_le(sample, 14);
        let ndl = read_u16_le(sample, 4);
        let stop = sample[8];
        let tank = sample[0] >> 4;

        samples.push(Sample {
            time_s,
            depth_m: read_u16_le(sample, 2) as f64 / 10.0 * FEET,
            temp_c: (temperature > 0).then(|| (temperature as f64 / 10.0 - 32.0) * 5.0 / 9.0),
            pressure_bar: (tank > 0 && pressure > 0).then_some(pressure as f64 * PSI),
            ndl_min: (stop == 0 && ndl < NO_LIMIT_NDL).then_some(ndl as u32),
            deco_time_min: (stop > 0).then_some(read_u16_le(sample, 6) as u32),
            deco_stop_m: (stop > 0).then_some((stop as f64 * FEET).round() as u32),
            gas: gas_numbers
                .iter()
                .position(|&number| number == tank)
                .unwrap_or(0) as u8,
            ..Default::default()
        });
    }

    // The log goes on for minutes after the dive, out of the water
    let temperatures = || {
        samples
            .iter()
            .filter(|sample| sample.time_s <= duration_seconds)
            .filter_map(|sample| sample.temp_c)
    };
    let min_temp_c = temperatures().reduce(f64::min);
    let max_temp_c = temperatures().reduce(f64::max);

    Ok(DiveLog {
        dsx_number: Some(entry.number),
        datetime: entry.datetime,
        avg_depth_m: Some(entry.avg_depth_m),
        min_temp_c,
        max_temp_c,
        start_position: entry.start_position,
        end_position: entry.end_position,
        duration_seconds,
        max_depth_m,
        dive_mode,
        gas_mixes,
        samples,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_arithmetic_wraps() {
        assert_eq!(ring_distance(0x10, 0x30, 0, 0x100), 0x20);
        assert_eq!(ring_distance(0xF0, 0x10, 0, 0x100), 0x20);
        // Equal pointers mean a full ring
        assert_eq!(ring_distance(0x40, 0x40, 0, 0x100), 0x100);
        assert_eq!(ring_decrement(0x110, 0x20, 0x100, 0x200), 0x1F0);
        assert_eq!(ring_decrement(0x200, 0x20, 0x100, 0x200), 0x1E0);
    }

    fn flash_map(first: u32, last: u32) -> Vec<u8> {
        let mut map = vec![0u8; 256];
        let fields = [
            (0x50, LOGBOOK_BEGIN),
            (0x54, LOGBOOK_END - 1),
            (0x58, first),
            (0x5C, last),
            (0x70, PROFILE_BEGIN),
            (0x74, PROFILE_END - 1),
        ];
        for (offset, value) in fields {
            map[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        map
    }

    #[test]
    fn logbook_entries_are_listed_newest_first() {
        let map = FlashMap::parse(&flash_map(LOGBOOK_BEGIN, LOGBOOK_BEGIN + 3 * 512)).unwrap();
        assert_eq!(
            map.logbook_entries().unwrap(),
            [LOGBOOK_BEGIN + 1024, LOGBOOK_BEGIN + 512, LOGBOOK_BEGIN]
        );

        // Wrapped: two entries at the end of the ring, one at the start
        let map = FlashMap::parse(&flash_map(LOGBOOK_END - 1024, LOGBOOK_BEGIN + 512)).unwrap();
        assert_eq!(
            map.logbook_entries().unwrap(),
            [LOGBOOK_BEGIN, LOGBOOK_END - 512, LOGBOOK_END - 1024]
        );
    }

    #[test]
    fn flash_map_with_another_layout_is_refused() {
        let mut map = flash_map(LOGBOOK_BEGIN, LOGBOOK_BEGIN);
        map[0x50] = 1;
        assert!(FlashMap::parse(&map).is_err());
    }

    fn logbook_entry(begin: u32, end: u32) -> Vec<u8> {
        let mut entry = vec![0u8; 512];
        entry[0] = 0x2C; // dive number 300
        entry[1] = 0x01;
        entry[2] = 2; // second dive of the day
        entry[3..8].copy_from_slice(&[14, 30, 5, 10, 26]); // 14:30, 5 Oct 2026
        entry[8..12].copy_from_slice(&begin.to_le_bytes());
        entry[12..16].copy_from_slice(&end.to_le_bytes());
        entry[18..20].copy_from_slice(&328u16.to_le_bytes()); // 32.8 ft
        entry[45] = MODE_OC;
        entry[240..244].copy_from_slice(&36.0175f32.to_le_bytes());
        entry[244..248].copy_from_slice(&14.2528f32.to_le_bytes());
        entry
    }

    #[test]
    fn logbook_entry_fields() {
        let entry = LogbookEntry::parse(&logbook_entry(PROFILE_BEGIN, PROFILE_BEGIN + 400))
            .unwrap()
            .unwrap();
        assert_eq!(entry.number, 300);
        assert!((entry.avg_depth_m - 9.997).abs() < 0.001);
        let position = entry.start_position.unwrap();
        assert_eq!((position.lat, position.lon), (36.0175, 14.2528));
        assert_eq!(entry.end_position, None); // no fix
        assert_eq!(entry.datetime.to_string(), "2026-10-05 14:30:00");
        assert_eq!(entry.profile_ranges().unwrap()[0], (PROFILE_BEGIN, 400));

        assert!(LogbookEntry::parse(&[0xFF; 512]).unwrap().is_none());
    }

    #[test]
    fn profile_wrapping_the_ring_is_split_in_two_reads() {
        let entry = LogbookEntry::parse(&logbook_entry(PROFILE_END - 100, PROFILE_BEGIN + 50))
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.profile_ranges().unwrap(),
            [(PROFILE_END - 100, 100), (PROFILE_BEGIN, 50)]
        );
    }

    #[test]
    fn dive_is_converted_to_metric() {
        let entry = LogbookEntry::parse(&logbook_entry(PROFILE_BEGIN, PROFILE_BEGIN + 400))
            .unwrap()
            .unwrap();

        // Header, two samples around an erased slot, footer
        let mut profile = vec![0u8; HEADER_SIZE + 3 * SAMPLE_SIZE + FOOTER_SIZE];
        profile[36] = 10; // sample interval
        profile[0x89 + 16] = 32; // OC gas 1: EAN32
        let samples = [(0usize, 328u16, 680u16, 2900u16), (2, 656, 500, 2000)];
        for (slot, depth, temperature, pressure) in samples {
            let sample = &mut profile[HEADER_SIZE + slot * SAMPLE_SIZE..][..SAMPLE_SIZE];
            sample[0] = 0x10; // tank 1
            sample[2..4].copy_from_slice(&depth.to_le_bytes());
            sample[10..12].copy_from_slice(&temperature.to_le_bytes());
            sample[14..16].copy_from_slice(&pressure.to_le_bytes());
            sample[4..6].copy_from_slice(&(if slot == 0 { 598u16 } else { 66 }).to_le_bytes());
        }
        profile[HEADER_SIZE + SAMPLE_SIZE..][..SAMPLE_SIZE].fill(0xFF);
        let footer = profile.len() - FOOTER_SIZE;
        profile[footer + 8..footer + 10].copy_from_slice(&1800u16.to_le_bytes());
        profile[footer + 10..footer + 12].copy_from_slice(&656u16.to_le_bytes());

        let dive = parse_dive(&entry, &profile).unwrap();
        assert_eq!(dive.duration_seconds, 1800);
        assert!((dive.max_depth_m - 19.995).abs() < 0.001); // 65.6 ft
        assert!(matches!(dive.dive_mode, DiveMode::Nitrox));
        assert_eq!(dive.gas_mixes.len(), 1);
        assert_eq!((dive.gas_mixes[0].o2, dive.gas_mixes[0].he), (32, 0));

        assert_eq!(dive.samples.len(), 2);
        assert_eq!((dive.samples[0].time_s, dive.samples[1].time_s), (10, 20));
        assert!((dive.samples[0].depth_m - 9.997).abs() < 0.001); // 32.8 ft
        assert!((dive.samples[0].temp_c.unwrap() - 20.0).abs() < 0.001); // 68.0 F
        assert!((dive.samples[0].pressure_bar.unwrap() - 199.95).abs() < 0.01); // 2900 psi
        assert_eq!(
            (dive.samples[0].ndl_min, dive.samples[1].ndl_min),
            (None, Some(66))
        );
        assert!(dive.number == 0 && dive.dsx_number == Some(300));
        assert_eq!(
            (
                dive.min_temp_c.map(f64::round),
                dive.max_temp_c.map(f64::round)
            ),
            (Some(10.0), Some(20.0))
        );

        // A sample after the end of the dive is not the water temperature
        profile[footer + 8..footer + 10].copy_from_slice(&15u16.to_le_bytes());
        let dive = parse_dive(&entry, &profile).unwrap();
        assert_eq!(dive.max_temp_c.map(f64::round), Some(20.0));
        assert_eq!(dive.min_temp_c.map(f64::round), Some(20.0));
    }

    #[test]
    fn only_the_gases_breathed_are_kept() {
        let entry = LogbookEntry::parse(&logbook_entry(PROFILE_BEGIN, PROFILE_BEGIN + 400))
            .unwrap()
            .unwrap();
        let dive_on = |gases: &[u8]| {
            let mut profile = vec![0u8; HEADER_SIZE + gases.len() * SAMPLE_SIZE + FOOTER_SIZE];
            // Gases set on the computer: air, EAN32, EAN50
            profile[0x89 + 16..0x89 + 16 + 3].copy_from_slice(&[21, 32, 50]);
            for (slot, gas) in gases.iter().enumerate() {
                profile[HEADER_SIZE + slot * SAMPLE_SIZE] = gas << 4;
            }
            parse_dive(&entry, &profile).unwrap()
        };

        // On air, whatever else is set
        let dive = dive_on(&[1, 1]);
        assert!(matches!(dive.dive_mode, DiveMode::Air));
        assert_eq!(dive.gas_mixes.len(), 1);
        assert_eq!(dive.gas_mixes[0].o2, 21);

        // A switch: `gas` counts in the gases kept, not in those set
        let dive = dive_on(&[1, 3]);
        assert!(matches!(dive.dive_mode, DiveMode::Nitrox));
        let o2: Vec<u8> = dive.gas_mixes.iter().map(|gas| gas.o2).collect();
        assert_eq!(o2, [21, 50]);
        assert_eq!((dive.samples[0].gas, dive.samples[1].gas), (0, 1));
    }
}
