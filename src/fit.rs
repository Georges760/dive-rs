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
const MESG_DIVE_SETTINGS: u16 = 258;
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
    Sint32(i32),
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
            Value::Sint32(_) => 0x85,
            Value::Uint32(_) => 0x86,
            Value::Uint32z(_) => 0x8C,
        }
    }

    fn size(self) -> u8 {
        match self {
            Value::Enum(_) | Value::Sint8(_) | Value::Uint8(_) => 1,
            Value::Uint16(_) => 2,
            Value::Sint32(_) | Value::Uint32(_) | Value::Uint32z(_) => 4,
        }
    }

    fn write(self, out: &mut Vec<u8>) {
        match self {
            Value::Enum(value) | Value::Uint8(value) => out.push(value),
            Value::Sint8(value) => out.push(value as u8),
            Value::Uint16(value) => out.extend(value.to_le_bytes()),
            Value::Sint32(value) => out.extend(value.to_le_bytes()),
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
    ambient_mbar: Option<f64>,
    /// Index of the sample this time falls under, for what the watch
    /// counts in steps: no-deco time, deco stop, gas time...
    sample: usize,
}

/// The FIT event a Descent logs for an alarm of the watch, as (event, data).
/// Most are a dive alert, with the kind of alert as data. `None` for the
/// alarms it has no counterpart of.
fn alarm_event(alarm: &str) -> Option<(u8, Option<u32>)> {
    const DIVE_ALERT: u8 = 56;
    let alert = |kind: u32| Some((DIVE_ALERT, Some(kind)));
    match alarm {
        "nodeco_deco" => alert(0),                             // ndl_reached
        "gas_switchpoint" => alert(1),                         // gas_switch_prompted
        "nodeco_2min" => alert(3),                             // approaching_ndl
        "mod_reached" => alert(4),                             // po2_warn
        "divetime_halftime" | "divetime_fulltime" => alert(7), // time_alert
        "max_dive_depth" => alert(8),                          // depth_alert
        "missed_deco" | "dive_violation_deco" => alert(9),     // deco_ceiling_broken
        "cns_danger" => alert(13),                             // cns_warning
        "cns_extreme" => alert(14),                            // cns_critical
        "fast_ascent" | "uncontrolled_ascent" => alert(17),    // ascent_critical
        "low_battery" => alert(20),                            // battery_low
        "very_low_battery" => alert(21),                       // battery_critical
        "probe_low_battery" => alert(32),                      // tank_battery_low
        "tank_reserve_reached" => Some((71, None)),            // tank_pressure_reserve
        "low_tank_pressure" => Some((72, None)),               // tank_pressure_critical
        "tank_lost_link" => Some((73, None)),                  // tank_lost
        _ => None,
    }
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
            .enumerate()
            .map(|(index, sample)| Point {
                time_s: sample.time_s,
                depth_m: sample.depth_m,
                temp_c: sample.temp_c,
                pressure_bar: sample.pressure_bar,
                ambient_mbar: sample.ambient_mbar.map(f64::from),
                sample: index,
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
            ambient_mbar: lerp_some(
                from.ambient_mbar.map(f64::from),
                to.ambient_mbar.map(f64::from),
            ),
            sample: i,
        });
    }
    points
}

/// Encode a dive as a FIT activity file.
///
/// `number` is that of the dive in the logbook, which is not the one the
/// dive computer gave it. `start` is the start of the dive in UTC and `utc_offset` what to add to
/// UTC, in seconds, to get the local time of the dive. The profile is
/// written as one record every `interval` seconds, as a Descent logs every
/// second; an interval of 0 writes the samples as logged.
pub fn encode_dive(
    dive: &DiveLog,
    number: u32,
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

    // The watch's own figures where it logs them, else worked out
    let avg_depth =
        depth(dive.avg_depth_m.unwrap_or_else(|| {
            points.iter().map(|p| p.depth_m).sum::<f64>() / points.len() as f64
        }));
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
    let device_info = |timestamp: u32, battery: Option<u8>| {
        let mut fields = vec![
            (TIMESTAMP, Value::Uint32(timestamp)),
            (0, Value::Uint8(0)), // device_index: creator
            (2, Value::Uint16(MANUFACTURER_GARMIN)),
            (4, Value::Uint16(PRODUCT_DESCENT_MK3I)),
        ];
        fields.extend(battery.map(|level| (32, Value::Uint8(level)))); // battery_level
        fields
    };
    fit.message(
        MESG_DEVICE_INFO,
        &device_info(begin, dive.battery_start_pct),
    );
    fit.message(
        MESG_EVENT,
        &[
            (TIMESTAMP, Value::Uint32(begin)),
            (0, Value::Enum(0)), // event: timer
            (1, Value::Enum(0)), // event_type: start
        ],
    );
    if !dive.gradient_factors.is_empty() || dive.water.is_some() {
        let mut settings = vec![(TIMESTAMP, Value::Uint32(begin))];
        // The set in use when the dive starts
        let set = dive.samples[0].gf_set as usize;
        if let Some(gf) = dive
            .gradient_factors
            .get(set)
            .or(dive.gradient_factors.first())
        {
            settings.extend([
                (1, Value::Enum(0)),        // model: zhl_16c
                (2, Value::Uint8(gf.low)),  // gf_low
                (3, Value::Uint8(gf.high)), // gf_high
            ]);
        }
        if let Some(water) = dive.water {
            let water_type = match water {
                Water::Fresh => 0,
                Water::Salt => 1,
                Water::En13319 => 2,
            };
            settings.push((4, Value::Enum(water_type)));
        }
        fit.message(MESG_DIVE_SETTINGS, &settings);
    }
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

    // A record has the same fields all along the dive: those the log has
    // for some sample at least, with the "no value" of their type elsewhere
    let logs = |has: fn(&Sample) -> bool| dive.samples.iter().any(has);
    let with_ambient = logs(|s| s.ambient_mbar.is_some());
    let with_speed = logs(|s| s.speed_m_min.is_some());
    let with_deco = logs(|s| s.ndl_min.is_some() || s.deco_time_min.is_some());
    let with_gas_time = logs(|s| s.gas_time_min.is_some());
    let with_sac = logs(|s| s.sac_l_min.is_some());

    // First sample whose alarms and gas are not yet written as events
    let mut pending = 0;
    for point in &points {
        while pending <= point.sample {
            let sample = &dive.samples[pending];
            let before = pending.checked_sub(1).map(|i| &dive.samples[i]);
            let timestamp = (TIMESTAMP, Value::Uint32(begin + sample.time_s));
            let marker = (1, Value::Enum(3)); // event_type: marker

            // An alarm is one event, when it comes up
            let raised = |alarm: &&String| before.is_none_or(|b| !b.alarms.contains(alarm));
            for (event, data) in sample
                .alarms
                .iter()
                .filter(raised)
                .filter_map(|alarm| alarm_event(alarm))
            {
                let mut fields = vec![timestamp, (0, Value::Enum(event)), marker];
                fields.extend(data.map(|data| (3, Value::Uint32(data))));
                fit.message(MESG_EVENT, &fields);
            }
            if before.is_some_and(|b| b.gas != sample.gas) {
                fit.message(
                    MESG_EVENT,
                    &[
                        timestamp,
                        (0, Value::Enum(57)), // event: dive_gas_switched
                        marker,
                        (3, Value::Uint32(sample.gas as u32)), // data: the dive_gas message
                    ],
                );
            }
            pending += 1;
        }

        let sample = &dive.samples[point.sample];
        let mut record = vec![
            (TIMESTAMP, Value::Uint32(begin + point.time_s)),
            (92, depth(point.depth_m)),
            // temperature, in whole degrees C; 0x7F stands for no value
            (
                13,
                Value::Sint8(point.temp_c.map_or(0x7F, |t| t.round() as i8)),
            ),
        ];
        if with_ambient {
            // absolute_pressure, in Pa
            let pascals = point.ambient_mbar.map(|mbar| (mbar * 100.0).round() as u32);
            record.push((91, Value::Uint32(pascals.unwrap_or(u32::MAX))));
        }
        if with_speed {
            // ascent_rate, in mm/s, positive going up
            let rate = sample
                .speed_m_min
                .map(|speed| (speed / 60.0 * 1000.0).round() as i32);
            record.push((127, Value::Sint32(rate.unwrap_or(i32::MAX))));
        }
        if with_deco {
            // In deco a Descent has no no-deco time left, out of deco no stop
            let (ndl, stop_depth, stop_time) = match (sample.deco_time_min, sample.ndl_min) {
                (Some(time), _) => (0, sample.deco_stop_m.unwrap_or(0) * 1000, time * 60),
                (None, Some(ndl)) => (ndl * 60, 0, 0),
                (None, None) => (u32::MAX, u32::MAX, u32::MAX),
            };
            record.extend([
                (96, Value::Uint32(ndl)),        // ndl_time
                (93, Value::Uint32(stop_depth)), // next_stop_depth
                (94, Value::Uint32(stop_time)),  // next_stop_time
            ]);
        }
        if with_gas_time {
            // air_time_remaining
            let seconds = sample.gas_time_min.map(|minutes| minutes * 60);
            record.push((123, Value::Uint32(seconds.unwrap_or(u32::MAX))));
        }
        if with_sac {
            // volume_sac, in 1/100 l/min
            let sac = sample.sac_l_min.map(|sac| (sac * 100) as u16);
            record.push((125, Value::Uint16(sac.unwrap_or(u16::MAX))));
        }
        fit.message(MESG_RECORD, &record);
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

    if dive.battery_end_pct.is_some() {
        fit.message(MESG_DEVICE_INFO, &device_info(end, dive.battery_end_pct));
    }

    // Tank pressures as the watch sums them up, else the first and last readings
    let mut pressures = points.iter().filter_map(|p| p.pressure_bar);
    let tank = dive.gas_mixes.iter().find_map(|gas| gas.tank.as_ref());
    let first_and_last = match tank {
        Some(tank) => Some((tank.start_bar, tank.end_bar)),
        None => pressures
            .next()
            .map(|first| (first, pressures.next_back().unwrap_or(first))),
    };
    if let Some((first, last)) = first_and_last {
        let mut summary = vec![
            (TIMESTAMP, Value::Uint32(end)),
            (0, Value::Uint32z(TANK_SENSOR)),
            (1, pressure(first)), // start_pressure
            (2, pressure(last)),  // end_pressure
        ];
        // volume_used, in 1/100 l: the pressure drop times the tank size
        if let Some(litres) = tank.and_then(|tank| tank.volume_l) {
            let used = ((first - last).max(0.0) * litres as f64 * 100.0).round() as u32;
            summary.push((3, Value::Uint32(used)));
        }
        fit.message(MESG_TANK_SUMMARY, &summary);
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
        (156, Value::Uint32(number)), // dive_number
    ]);
    if !temperatures.is_empty() {
        let degrees = |t: f64| Value::Sint8(t.round() as i8);
        let max = temperatures.iter().copied().fold(f64::MIN, f64::max);
        let min = temperatures.iter().copied().fold(f64::MAX, f64::min);
        session.extend([
            (
                57,
                degrees(temperatures.iter().sum::<f64>() / temperatures.len() as f64),
            ),
            (58, degrees(dive.max_temp_c.unwrap_or(max))),
            (150, degrees(dive.min_temp_c.unwrap_or(min))),
        ]);
    }
    // Surface interval (0 when the watch counts none), CNS clock and OTU,
    // which the session and the dive summary number differently
    let interval = dive.surface_interval_s.filter(|&seconds| seconds > 0);
    let percent = |cns: f64| Value::Uint8(cns.round() as u8);
    let toxicity = |fields: [u8; 4]| {
        let [surface_interval, start_cns, end_cns, o2_toxicity] = fields;
        let mut values = Vec::new();
        values.extend(interval.map(|seconds| (surface_interval, Value::Uint32(seconds))));
        values.extend(dive.cns_start_pct.map(|cns| (start_cns, percent(cns))));
        values.extend(dive.cns_end_pct.map(|cns| (end_cns, percent(cns))));
        values.extend(
            dive.otu_end
                .map(|otu| (o2_toxicity, Value::Uint16(otu.round() as u16))),
        );
        values
    };
    session.extend(toxicity([142, 143, 144, 155]));
    fit.message(MESG_SESSION, &session);

    // A Descent writes one dive summary for the lap and one for the session
    for reference in [MESG_LAP, MESG_SESSION] {
        let mut summary = vec![
            (TIMESTAMP, Value::Uint32(end)),
            (0, Value::Uint16(reference)), // reference_mesg
            (1, Value::Uint16(0)),         // reference_index
            (2, avg_depth),
            (3, max_depth),
            (10, Value::Uint32(number)),             // dive_number
            (11, Value::Uint32(dive_time_s * 1000)), // bottom_time
        ];
        summary.extend(toxicity([4, 5, 6, 9]));
        // max_ascent_rate, in mm/s
        summary.extend(dive.max_ascent_speed_m_min.map(|speed| {
            let rate = (speed / 60.0 * 1000.0).round() as u32;
            (23, Value::Uint32(rate))
        }));
        fit.message(MESG_DIVE_SUMMARY, &summary);
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
        let file = encode_dive(&dive(), 7, start(), 7200, 0).unwrap();
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
        let messages = decode(&encode_dive(&dive(), 7, start(), 7200, 0).unwrap());

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
        let messages = decode(&encode_dive(&dive, 7, start(), 0, 0).unwrap());

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

        let messages = decode(&encode_dive(&dive(), 7, start(), 0, 1).unwrap());
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
        let messages = decode(&encode_dive(&dive, 7, start(), 0, 5).unwrap());

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

    /// The test dive with what a newer log holds besides.
    fn full_dive() -> DiveLog {
        let mut dive = dive();
        dive.avg_depth_m = Some(12.5);
        dive.min_temp_c = Some(18.2);
        dive.max_temp_c = Some(21.0);
        dive.water = Some(Water::Salt);
        dive.surface_interval_s = Some(16180);
        dive.cns_start_pct = Some(0.21);
        dive.cns_end_pct = Some(5.65);
        dive.otu_end = Some(14.62);
        dive.gradient_factors = vec![
            GradientFactors { low: 85, high: 80 },
            GradientFactors { low: 95, high: 90 },
        ];
        dive.max_ascent_speed_m_min = Some(14.9);
        dive.battery_start_pct = Some(58);
        dive.battery_end_pct = Some(53);
        dive.gas_mixes[0].tank = Some(Tank {
            start_bar: 209.1,
            end_bar: 99.0,
            volume_l: Some(12),
            working_bar: Some(200),
        });

        // 10 s: within the no-deco limit, the transmitter not heard yet
        let bottom = &mut dive.samples[0];
        bottom.ambient_mbar = Some(1500);
        bottom.speed_m_min = Some(-3.0);
        bottom.ndl_min = Some(17);
        bottom.alarms = vec!["tank_lost_link".to_string(), "slow_down".to_string()];

        // 20 s: into deco, on the second gas
        let deco = &mut dive.samples[1];
        deco.ambient_mbar = Some(3000);
        deco.speed_m_min = Some(7.2);
        deco.deco_time_min = Some(2);
        deco.deco_stop_m = Some(3);
        deco.gas = 1;
        deco.pressure_bar = Some(150.0);
        deco.gas_time_min = Some(28);
        deco.sac_l_min = Some(18);
        deco.alarms = vec!["tank_lost_link".to_string(), "nodeco_deco".to_string()];

        // 30 s: nothing but depth, and the gas in use
        dive.samples[2].gas = 1;
        dive
    }

    #[test]
    fn summary_carries_the_figures_of_the_watch() {
        let begin = fit_time(start());
        let messages = decode(&encode_dive(&full_dive(), 7, start(), 0, 0).unwrap());
        let find = |global: u16| messages.iter().find(|message| message.0 == global).unwrap();

        let session = find(18);
        assert_eq!(field(session, 140), 12_500); // avg_depth, not the 9 m of the samples
        assert_eq!((field(session, 150), field(session, 58)), (18, 21));
        assert_eq!(field(session, 142), 16180); // surface_interval
        assert_eq!((field(session, 143), field(session, 144)), (0, 6)); // CNS
        assert_eq!(field(session, 155), 15); // o2_toxicity

        let summary = find(268);
        assert_eq!(field(summary, 2), 12_500);
        assert_eq!(field(summary, 4), 16180);
        assert_eq!((field(summary, 5), field(summary, 6)), (0, 6));
        assert_eq!(field(summary, 9), 15);
        assert_eq!(field(summary, 23), 248); // max_ascent_rate: 14.9 m/min in mm/s

        // dive_settings: Buhlmann with the first gradient factor set, salt water
        let settings = find(258);
        assert_eq!(field(settings, 1), 0);
        assert_eq!((field(settings, 2), field(settings, 3)), (85, 80));
        assert_eq!(field(settings, 4), 1);

        // device_info: battery at the start, then at the end
        let battery: Vec<(u32, u32)> = messages
            .iter()
            .filter(|message| message.0 == 23)
            .map(|message| (field(message, 253) - begin, field(message, 32)))
            .collect();
        assert_eq!(battery, [(0, 58), (1800, 53)]);

        // tank_summary: the pressures of the header, 110.1 bar out of 12 l
        let tank = find(323);
        assert_eq!((field(tank, 1), field(tank, 2)), (20_910, 9_900));
        assert_eq!(field(tank, 3), 132_120);
    }

    #[test]
    fn records_carry_deco_and_gas_data() {
        let messages = decode(&encode_dive(&full_dive(), 7, start(), 0, 0).unwrap());
        let records: Vec<&Message> = messages.iter().filter(|message| message.0 == 20).collect();
        let [bottom, deco, last] = records[..] else {
            panic!("expected 3 records, got {}", records.len());
        };

        assert_eq!(field(bottom, 91), 150_000); // absolute_pressure, Pa
        assert_eq!(field(bottom, 127) as i32, -50); // ascent_rate: -3 m/min in mm/s
                                                    // ndl_time, with no stop
        assert_eq!(
            (field(bottom, 96), field(bottom, 93), field(bottom, 94)),
            (1020, 0, 0)
        );
        // no gas time before the transmitter is heard
        assert_eq!((field(bottom, 123), field(bottom, 125)), (u32::MAX, 0xFFFF));

        assert_eq!(field(deco, 127), 120);
        // no no-deco time left: a stop of 2 minutes at 3 m
        assert_eq!(
            (field(deco, 96), field(deco, 93), field(deco, 94)),
            (0, 3000, 120)
        );
        assert_eq!((field(deco, 123), field(deco, 125)), (1680, 1800));

        // what the last sample lacks is written as "no value"
        assert_eq!(
            (field(last, 91), field(last, 127)),
            (u32::MAX, i32::MAX as u32)
        );
        assert_eq!(field(last, 96), u32::MAX);
    }

    #[test]
    fn records_take_the_step_values_of_the_sample_before() {
        // At one record per 5 s: 15 s still falls under the sample of 10 s
        let messages = decode(&encode_dive(&full_dive(), 7, start(), 0, 5).unwrap());
        let records: Vec<&Message> = messages.iter().filter(|message| message.0 == 20).collect();
        assert_eq!(records.len(), 5);

        assert_eq!(field(records[1], 96), 1020);
        // while the pressure between the two is interpolated
        assert_eq!(field(records[1], 91), 225_000);
        assert_eq!((field(records[2], 96), field(records[2], 94)), (0, 120));
    }

    #[test]
    fn alarms_and_gas_switches_become_events() {
        let begin = fit_time(start());
        let messages = decode(&encode_dive(&full_dive(), 7, start(), 0, 1).unwrap());

        // Without the timer start and stop: (seconds, event, data)
        let data = |message: &Message| {
            message
                .1
                .iter()
                .find(|field| field.0 == 3)
                .map(|field| field.1)
        };
        let events: Vec<(u32, u32, Option<u32>)> = messages
            .iter()
            .filter(|message| message.0 == 21 && field(message, 1) == 3)
            .map(|message| {
                (
                    field(message, 253) - begin,
                    field(message, 0),
                    data(message),
                )
            })
            .collect();
        assert_eq!(
            events,
            [
                // tank_lost, once though it lasts; no event for "slow down"
                (10, 73, None),
                // dive_alert ndl_reached, then dive_gas_switched to gas 1
                (20, 56, Some(0)),
                (20, 57, Some(1)),
            ]
        );
    }

    #[test]
    fn helium_is_written_with_the_gas() {
        let mut dive = dive();
        dive.gas_mixes[0] = GasMix {
            o2: 18,
            he: 45,
            ..Default::default()
        };
        let messages = decode(&encode_dive(&dive, 7, start(), 0, 0).unwrap());

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
        assert!(encode_dive(&dive, 7, start(), 0, 1).is_err());
    }
}
