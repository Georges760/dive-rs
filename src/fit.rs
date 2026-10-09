//! FIT activity export, laid out like a dive logged by a Garmin Descent.
//!
//! Just enough of the FIT protocol to write an activity file: definition
//! messages, little-endian data messages and the CRC. Message and field
//! numbers come from the FIT SDK profile, version 21.218.

use anyhow::{bail, Result};
use chrono::{NaiveDate, NaiveDateTime};

use crate::types::*;

// Global message numbers
const MESG_FILE_ID: u16 = 0;
const MESG_SESSION: u16 = 18;
const MESG_LAP: u16 = 19;
const MESG_RECORD: u16 = 20;
const MESG_EVENT: u16 = 21;
const MESG_DEVICE_INFO: u16 = 23;
const MESG_ACTIVITY: u16 = 34;
const MESG_DIVE_GAS: u16 = 259;
const MESG_DIVE_SUMMARY: u16 = 268;
const MESG_TANK_UPDATE: u16 = 319;
const MESG_TANK_SUMMARY: u16 = 323;

// Field numbers common to all messages
const TIMESTAMP: u8 = 253;
const MESSAGE_INDEX: u8 = 254;

// The device the file claims to come from
const MANUFACTURER_GARMIN: u16 = 1;
const PRODUCT_DESCENT_MK3I: u16 = 4223;

// The log does not say which transmitter the tank pressure came from:
// all of it goes to one made-up sensor
const TANK_SENSOR: u32 = 1;

const SPORT_DIVING: u8 = 53;
const SUB_SPORT_SINGLE_GAS: u8 = 53;
const SUB_SPORT_MULTI_GAS: u8 = 54;
const SUB_SPORT_GAUGE: u8 = 55;
const SUB_SPORT_APNEA: u8 = 56;

/// A field value; the variant is its FIT base type.
#[derive(Debug, Clone, Copy)]
enum Value {
    Enum(u8),
    Sint8(i8),
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Uint32z(u32),
}

impl Value {
    /// Base type byte of the field definition.
    fn base_type(self) -> u8 {
        match self {
            Value::Enum(_) => 0x00,
            Value::Sint8(_) => 0x01,
            Value::Uint8(_) => 0x02,
            Value::Uint16(_) => 0x84,
            Value::Uint32(_) => 0x86,
            Value::Uint32z(_) => 0x8C,
        }
    }

    fn size(self) -> u8 {
        match self {
            Value::Enum(_) | Value::Sint8(_) | Value::Uint8(_) => 1,
            Value::Uint16(_) => 2,
            Value::Uint32(_) | Value::Uint32z(_) => 4,
        }
    }

    fn write(self, out: &mut Vec<u8>) {
        match self {
            Value::Enum(value) | Value::Uint8(value) => out.push(value),
            Value::Sint8(value) => out.push(value as u8),
            Value::Uint16(value) => out.extend(value.to_le_bytes()),
            Value::Uint32(value) | Value::Uint32z(value) => out.extend(value.to_le_bytes()),
        }
    }
}

/// CRC-16 of the FIT protocol (CRC-16/ARC, computed a nibble at a time).
fn crc16(data: &[u8]) -> u16 {
    const TABLE: [u16; 16] = [
        0x0000, 0xCC01, 0xD801, 0x1400, 0xF001, 0x3C00, 0x2800, 0xE401, 0xA001, 0x6C00, 0x7800,
        0xB401, 0x5000, 0x9C01, 0x8801, 0x4400,
    ];
    let mut crc = 0u16;
    for &byte in data {
        for nibble in [byte & 0x0F, byte >> 4] {
            crc = (crc >> 4) ^ TABLE[(crc & 0x0F) as usize] ^ TABLE[nibble as usize];
        }
    }
    crc
}

/// Accumulates the messages of a FIT file.
#[derive(Default)]
struct FitWriter {
    data: Vec<u8>,
    /// Per local message type: its global message number and the
    /// (field number, base type) layout last defined for it
    defined: Vec<(u16, Vec<(u8, u8)>)>,
}

impl FitWriter {
    /// Append a data message, preceded by its definition when the fields
    /// differ from the previous message of that type.
    ///
    /// Definition message:
    ///   0: 0x40 | local message type      1: reserved
    ///   2: architecture (0 = little-endian)
    ///   3: global message number (u16 LE)  5: number of fields
    ///   6: per field: field number, size, base type
    /// Data message:
    ///   0: local message type, then the field values in definition order
    fn message(&mut self, global: u16, fields: &[(u8, Value)]) {
        let layout: Vec<(u8, u8)> = fields
            .iter()
            .map(|&(number, value)| (number, value.base_type()))
            .collect();

        // One local message type per global message number (at most 16)
        let local = match self
            .defined
            .iter()
            .position(|&(number, _)| number == global)
        {
            Some(local) => local,
            None => {
                self.defined.push((global, Vec::new()));
                self.defined.len() - 1
            }
        };
        assert!(local < 16, "out of local message types");

        if self.defined[local].1 != layout {
            self.data.extend([0x40 | local as u8, 0, 0]);
            self.data.extend(global.to_le_bytes());
            self.data.push(fields.len() as u8);
            for &(number, value) in fields {
                self.data.extend([number, value.size(), value.base_type()]);
            }
            self.defined[local].1 = layout;
        }

        self.data.push(local as u8);
        for &(_, value) in fields {
            value.write(&mut self.data);
        }
    }

    /// Wrap the messages in the file header and CRC.
    ///
    /// Header (14 bytes):
    ///   0: header size              1: protocol version (0x20 = 2.0)
    ///   2: profile version (u16 LE, major * 1000 + minor)
    ///   4: data size (u32 LE)       8: ".FIT"
    ///   12: CRC of bytes 0-11 (u16 LE)
    fn finish(self) -> Vec<u8> {
        let mut file = vec![14, 0x20];
        file.extend(21218u16.to_le_bytes());
        file.extend((self.data.len() as u32).to_le_bytes());
        file.extend(b".FIT");
        file.extend(crc16(&file).to_le_bytes());
        file.extend(self.data);
        file.extend(crc16(&file).to_le_bytes());
        file
    }
}

/// Seconds since the FIT epoch, 1989-12-31 00:00:00 UTC.
fn fit_time(datetime: NaiveDateTime) -> u32 {
    let epoch = NaiveDate::from_ymd_opt(1989, 12, 31)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    (datetime - epoch).num_seconds() as u32
}

/// Depth as stored in FIT: millimetres.
fn depth(metres: f64) -> Value {
    Value::Uint32((metres * 1000.0).round() as u32)
}

fn sub_sport(dive: &DiveLog) -> u8 {
    match dive.dive_mode {
        DiveMode::Gauge => SUB_SPORT_GAUGE,
        DiveMode::Freedive => SUB_SPORT_APNEA,
        DiveMode::Air | DiveMode::Nitrox if dive.gas_mixes.len() > 1 => SUB_SPORT_MULTI_GAS,
        DiveMode::Air | DiveMode::Nitrox => SUB_SPORT_SINGLE_GAS,
    }
}

/// The profile at one time: a record message and a tank update.
struct Point {
    time_s: u32,
    depth_m: f64,
    temp_c: Option<f64>,
    pressure_bar: Option<f64>,
}

/// Tank pressure as stored in FIT: 1/100 bar.
fn pressure(bar: f64) -> Value {
    Value::Uint16((bar * 100.0).round() as u16)
}

/// The profile at one point every `interval` seconds, interpolated linearly
/// between the samples. An interval of 0 keeps the samples as logged.
fn resample(samples: &[Sample], interval: u32) -> Vec<Point> {
    if interval == 0 {
        return samples
            .iter()
            .map(|sample| Point {
                time_s: sample.time_s,
                depth_m: sample.depth_m,
                temp_c: sample.temp_c,
                pressure_bar: sample.pressure_bar,
            })
            .collect();
    }

    let last = samples.len() - 1;
    let mut points = Vec::new();
    let mut i = 0;
    for time_s in (samples[0].time_s..=samples[last].time_s).step_by(interval as usize) {
        // Move on to the two samples around this time
        while i < last && samples[i + 1].time_s <= time_s {
            i += 1;
        }
        let (from, to) = (&samples[i], &samples[(i + 1).min(last)]);
        let span = to.time_s.saturating_sub(from.time_s);
        let ratio = if span > 0 {
            (time_s - from.time_s) as f64 / span as f64
        } else {
            0.0
        };
        let lerp = |from: f64, to: f64| from + (to - from) * ratio;
        // Without a value on both sides, hold the earlier one
        let lerp_some = |from: Option<f64>, to: Option<f64>| match (from, to) {
            (Some(from), Some(to)) => Some(lerp(from, to)),
            (from, _) => from,
        };

        points.push(Point {
            time_s,
            depth_m: lerp(from.depth_m, to.depth_m),
            temp_c: lerp_some(from.temp_c, to.temp_c),
            pressure_bar: lerp_some(from.pressure_bar, to.pressure_bar),
        });
    }
    points
}

/// Encode a dive as a FIT activity file.
///
/// `start` is the start of the dive in UTC and `utc_offset` what to add to
/// UTC, in seconds, to get the local time of the dive. The profile is
/// written as one record every `interval` seconds, as a Descent logs every
/// second; an interval of 0 writes the samples as logged.
pub fn encode_dive(
    dive: &DiveLog,
    start: NaiveDateTime,
    utc_offset: i32,
    interval: u32,
) -> Result<Vec<u8>> {
    if dive.samples.is_empty() {
        bail!("no depth samples");
    }
    let points = resample(&dive.samples, interval);

    // The log can run on at the surface after the dive time has stopped
    let elapsed_s = dive.duration_seconds.max(points[points.len() - 1].time_s);
    let dive_time_s = match dive.duration_seconds {
        0 => elapsed_s,
        seconds => seconds,
    };
    let begin = fit_time(start);
    let end = begin + elapsed_s;

    let avg_depth = depth(points.iter().map(|p| p.depth_m).sum::<f64>() / points.len() as f64);
    let max_depth = depth(
        points
            .iter()
            .map(|p| p.depth_m)
            .fold(dive.max_depth_m, f64::max),
    );
    let temperatures: Vec<f64> = points.iter().filter_map(|p| p.temp_c).collect();

    let mut fit = FitWriter::default();

    fit.message(
        MESG_FILE_ID,
        &[
            (0, Value::Enum(4)), // type: activity
            (1, Value::Uint16(MANUFACTURER_GARMIN)),
            (2, Value::Uint16(PRODUCT_DESCENT_MK3I)),
            (4, Value::Uint32(begin)), // time_created
        ],
    );
    fit.message(
        MESG_DEVICE_INFO,
        &[
            (TIMESTAMP, Value::Uint32(begin)),
            (0, Value::Uint8(0)), // device_index: creator
            (2, Value::Uint16(MANUFACTURER_GARMIN)),
            (4, Value::Uint16(PRODUCT_DESCENT_MK3I)),
        ],
    );
    fit.message(
        MESG_EVENT,
        &[
            (TIMESTAMP, Value::Uint32(begin)),
            (0, Value::Enum(0)), // event: timer
            (1, Value::Enum(0)), // event_type: start
        ],
    );
    for (i, gas) in dive.gas_mixes.iter().enumerate() {
        fit.message(
            MESG_DIVE_GAS,
            &[
                (MESSAGE_INDEX, Value::Uint16(i as u16)),
                (0, Value::Uint8(gas.he)), // helium_content
                (1, Value::Uint8(gas.o2)), // oxygen_content
                (2, Value::Enum(1)),       // status: enabled
            ],
        );
    }

    for point in &points {
        fit.message(
            MESG_RECORD,
            &[
                (TIMESTAMP, Value::Uint32(begin + point.time_s)),
                (92, depth(point.depth_m)),
                // temperature, in whole degrees C; 0x7F stands for no value
                (
                    13,
                    Value::Sint8(point.temp_c.map_or(0x7F, |t| t.round() as i8)),
                ),
            ],
        );
        if let Some(bar) = point.pressure_bar {
            fit.message(
                MESG_TANK_UPDATE,
                &[
                    (TIMESTAMP, Value::Uint32(begin + point.time_s)),
                    (0, Value::Uint32z(TANK_SENSOR)),
                    (1, pressure(bar)),
                ],
            );
        }
    }

    fit.message(
        MESG_EVENT,
        &[
            (TIMESTAMP, Value::Uint32(end)),
            (0, Value::Enum(0)), // event: timer
            (1, Value::Enum(4)), // event_type: stop_all
        ],
    );

    let mut pressures = points.iter().filter_map(|p| p.pressure_bar);
    if let Some(first) = pressures.next() {
        fit.message(
            MESG_TANK_SUMMARY,
            &[
                (TIMESTAMP, Value::Uint32(end)),
                (0, Value::Uint32z(TANK_SENSOR)),
                (1, pressure(first)), // start_pressure
                (2, pressure(pressures.next_back().unwrap_or(first))), // end_pressure
            ],
        );
    }

    // Lap and session number these fields alike
    let summary = [
        (MESSAGE_INDEX, Value::Uint16(0)),
        (TIMESTAMP, Value::Uint32(end)),
        (2, Value::Uint32(begin)),            // start_time
        (7, Value::Uint32(elapsed_s * 1000)), // total_elapsed_time
        (8, Value::Uint32(elapsed_s * 1000)), // total_timer_time
    ];

    let mut lap = summary.to_vec();
    lap.extend([(122, avg_depth), (123, max_depth)]);
    fit.message(MESG_LAP, &lap);

    let mut session = summary.to_vec();
    session.extend([
        (5, Value::Enum(SPORT_DIVING)),
        (6, Value::Enum(sub_sport(dive))),
        (25, Value::Uint16(0)), // first_lap_index
        (26, Value::Uint16(1)), // num_laps
        (140, avg_depth),
        (141, max_depth),
        (156, Value::Uint32(dive.number)), // dive_number
    ]);
    if !temperatures.is_empty() {
        let degrees = |t: f64| Value::Sint8(t.round() as i8);
        session.extend([
            (
                57,
                degrees(temperatures.iter().sum::<f64>() / temperatures.len() as f64),
            ),
            (
                58,
                degrees(temperatures.iter().copied().fold(f64::MIN, f64::max)),
            ),
            (
                150,
                degrees(temperatures.iter().copied().fold(f64::MAX, f64::min)),
            ),
        ]);
    }
    fit.message(MESG_SESSION, &session);

    // A Descent writes one dive summary for the lap and one for the session
    for reference in [MESG_LAP, MESG_SESSION] {
        fit.message(
            MESG_DIVE_SUMMARY,
            &[
                (TIMESTAMP, Value::Uint32(end)),
                (0, Value::Uint16(reference)), // reference_mesg
                (1, Value::Uint16(0)),         // reference_index
                (2, avg_depth),
                (3, max_depth),
                (10, Value::Uint32(dive.number)), // dive_number
                (11, Value::Uint32(dive_time_s * 1000)), // bottom_time
            ],
        );
    }

    fit.message(
        MESG_ACTIVITY,
        &[
            (TIMESTAMP, Value::Uint32(end)),
            (0, Value::Uint32(elapsed_s * 1000)), // total_timer_time
            (1, Value::Uint16(1)),                // num_sessions
            (2, Value::Enum(0)),                  // type: manual
            (3, Value::Enum(26)),                 // event: activity
            (4, Value::Enum(1)),                  // event_type: stop
            (5, Value::Uint32(end.wrapping_add_signed(utc_offset))), // local_timestamp
        ],
    );

    Ok(fit.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A FIT message read back: global number and (field number, value) pairs.
    type Message = (u16, Vec<(u8, u32)>);

    /// Decode the messages of a file written by `FitWriter`.
    fn decode(file: &[u8]) -> Vec<Message> {
        let data = &file[14..file.len() - 2];
        let mut definitions: Vec<(u16, Vec<(u8, usize)>)> = vec![(0, Vec::new()); 16];
        let mut messages = Vec::new();
        let mut at = 0;
        while at < data.len() {
            let local = (data[at] & 0x0F) as usize;
            if data[at] & 0x40 != 0 {
                let global = u16::from_le_bytes([data[at + 3], data[at + 4]]);
                let count = data[at + 5] as usize;
                let fields = data[at + 6..at + 6 + count * 3]
                    .chunks(3)
                    .map(|field| (field[0], field[1] as usize))
                    .collect();
                definitions[local] = (global, fields);
                at += 6 + count * 3;
            } else {
                at += 1;
                let (global, fields) = &definitions[local];
                let mut values = Vec::new();
                for &(number, size) in fields {
                    let mut bytes = [0u8; 4];
                    bytes[..size].copy_from_slice(&data[at..at + size]);
                    values.push((number, u32::from_le_bytes(bytes)));
                    at += size;
                }
                messages.push((*global, values));
            }
        }
        messages
    }

    fn field(message: &Message, number: u8) -> u32 {
        let found = message.1.iter().find(|&&(n, _)| n == number);
        found
            .unwrap_or_else(|| panic!("no field {number} in {message:?}"))
            .1
    }

    fn sample(time_s: u32, depth_m: f64, temp_c: Option<f64>) -> Sample {
        Sample {
            time_s,
            depth_m,
            temp_c,
            ..Default::default()
        }
    }

    fn dive() -> DiveLog {
        DiveLog {
            number: 7,
            datetime: start(),
            duration_seconds: 1800,
            max_depth_m: 20.2,
            dive_mode: DiveMode::Nitrox,
            gas_mixes: vec![GasMix {
                o2: 32,
                ..Default::default()
            }],
            samples: vec![
                sample(10, 5.0, Some(20.4)),
                sample(20, 20.0, Some(18.6)),
                sample(30, 2.0, None),
            ],
            ..Default::default()
        }
    }

    fn start() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 5)
            .unwrap()
            .and_hms_opt(12, 30, 0)
            .unwrap()
    }

    #[test]
    fn crc_is_crc16_arc() {
        assert_eq!(crc16(b"123456789"), 0xBB3D);
    }

    #[test]
    fn fit_time_counts_from_the_end_of_1989() {
        let unix = start().and_utc().timestamp();
        assert_eq!(fit_time(start()) as i64, unix - 631_065_600);
    }

    #[test]
    fn file_has_a_header_and_valid_crcs() {
        let file = encode_dive(&dive(), start(), 7200, 0).unwrap();
        assert_eq!(file[0], 14);
        assert_eq!(&file[8..12], b".FIT");
        let data_size = u32::from_le_bytes(file[4..8].try_into().unwrap()) as usize;
        assert_eq!(file.len(), 14 + data_size + 2);
        // A CRC-16/ARC over data followed by its own CRC is 0
        assert_eq!(crc16(&file[..14]), 0);
        assert_eq!(crc16(&file), 0);
    }

    #[test]
    fn dive_is_written_as_a_diving_activity() {
        let begin = fit_time(start());
        let messages = decode(&encode_dive(&dive(), start(), 7200, 0).unwrap());

        let order: Vec<u16> = messages.iter().map(|message| message.0).collect();
        assert_eq!(
            order,
            [0, 23, 21, 259, 20, 20, 20, 21, 19, 18, 268, 268, 34]
        );

        // file_id: an activity from a Garmin Descent
        assert_eq!(field(&messages[0], 0), 4);
        assert_eq!(field(&messages[0], 1), 1);
        assert_eq!(field(&messages[0], 4), begin);

        // dive_gas: EAN32
        assert_eq!((field(&messages[3], 1), field(&messages[3], 0)), (32, 0));

        // record: timestamp, depth in mm, temperature in C
        let record = &messages[5];
        assert_eq!(field(record, 253), begin + 20);
        assert_eq!(field(record, 92), 20_000);
        assert_eq!(field(record, 13), 19);
        assert_eq!(field(&messages[6], 13), 0x7F);

        let session = &messages[9];
        assert_eq!(field(session, 2), begin);
        assert_eq!(field(session, 253), begin + 1800);
        assert_eq!(field(session, 7), 1_800_000);
        assert_eq!(field(session, 5), 53); // sport: diving
        assert_eq!(field(session, 6), 53); // sub_sport: single_gas_diving
        assert_eq!(field(session, 140), 9_000);
        // The logged maximum is deeper than any sample
        assert_eq!(field(session, 141), 20_200);
        assert_eq!(field(session, 156), 7);
        assert_eq!((field(session, 150), field(session, 58)), (19, 20));

        // dive_summary: one for the lap, one for the session
        assert_eq!(field(&messages[10], 0), 19);
        let summary = &messages[11];
        assert_eq!(field(summary, 0), 18);
        assert_eq!(field(summary, 2), 9_000);
        assert_eq!(field(summary, 3), 20_200);
        assert_eq!(field(summary, 10), 7);
        assert_eq!(field(summary, 11), 1_800_000);

        // activity: local time is two hours ahead of UTC
        assert_eq!(field(&messages[12], 5), begin + 1800 + 7200);
    }

    #[test]
    fn log_running_past_the_dive_time_sets_the_elapsed_time() {
        let mut dive = dive();
        dive.duration_seconds = 25;
        let messages = decode(&encode_dive(&dive, start(), 0, 0).unwrap());

        let session = messages.iter().find(|message| message.0 == 18).unwrap();
        assert_eq!(field(session, 7), 30_000);
        let summary = messages.iter().find(|message| message.0 == 268).unwrap();
        assert_eq!(field(summary, 11), 25_000);
    }

    #[test]
    fn profile_is_interpolated_to_the_record_interval() {
        let points = resample(&dive().samples, 5);
        let times: Vec<u32> = points.iter().map(|point| point.time_s).collect();
        assert_eq!(times, [10, 15, 20, 25, 30]);
        let depths: Vec<f64> = points.iter().map(|point| point.depth_m).collect();
        assert_eq!(depths, [5.0, 12.5, 20.0, 11.0, 2.0]);
        assert!((points[1].temp_c.unwrap() - 19.5).abs() < 1e-9);
        // No temperature in the last sample: the previous one carries over
        assert_eq!(points[3].temp_c, Some(18.6));
        assert_eq!(points[4].temp_c, None);

        let messages = decode(&encode_dive(&dive(), start(), 0, 1).unwrap());
        assert_eq!(
            messages.iter().filter(|message| message.0 == 20).count(),
            21
        );
    }

    #[test]
    fn tank_pressure_is_written_as_tank_updates() {
        let mut dive = dive();
        // The transmitter is picked up after the first sample
        dive.samples[1].pressure_bar = Some(200.0);
        dive.samples[2].pressure_bar = Some(180.5);
        let messages = decode(&encode_dive(&dive, start(), 0, 5).unwrap());

        let updates: Vec<(u32, u32)> = messages
            .iter()
            .filter(|message| message.0 == 319)
            .map(|message| (field(message, 253) - fit_time(start()), field(message, 1)))
            .collect();
        assert_eq!(updates, [(20, 20_000), (25, 19_025), (30, 18_050)]);

        let summary = messages.iter().find(|message| message.0 == 323).unwrap();
        assert_eq!(field(summary, 0), TANK_SENSOR);
        assert_eq!((field(summary, 1), field(summary, 2)), (20_000, 18_050));
    }

    #[test]
    fn helium_is_written_with_the_gas() {
        let mut dive = dive();
        dive.gas_mixes[0] = GasMix {
            o2: 18,
            he: 45,
            ..Default::default()
        };
        let messages = decode(&encode_dive(&dive, start(), 0, 0).unwrap());

        let gas = messages.iter().find(|message| message.0 == 259).unwrap();
        assert_eq!((field(gas, 1), field(gas, 0)), (18, 45));
    }

    #[test]
    fn sub_sport_follows_the_mode_and_the_gases() {
        let mut dive = dive();
        assert_eq!(sub_sport(&dive), SUB_SPORT_SINGLE_GAS);
        dive.gas_mixes.push(GasMix {
            o2: 50,
            ..Default::default()
        });
        assert_eq!(sub_sport(&dive), SUB_SPORT_MULTI_GAS);
        dive.dive_mode = DiveMode::Gauge;
        assert_eq!(sub_sport(&dive), SUB_SPORT_GAUGE);
    }

    #[test]
    fn dive_without_samples_is_refused() {
        let mut dive = dive();
        dive.samples.clear();
        assert!(encode_dive(&dive, start(), 0, 1).is_err());
    }
}
