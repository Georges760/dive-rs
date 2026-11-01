//! A dive logged by two computers becomes one entry with the best of both:
//! the profile of the one that samples more often, and from the other
//! whatever that one does not have, such as tank pressure.

use crate::types::*;

/// Farthest apart two computers can start the same dive
const MAX_START_GAP_S: i64 = 600;
/// Farthest apart their profiles are searched for the same moment
const MAX_SHIFT_S: i64 = 120;
/// Samples below this depth say little about when things happened
const MIN_DEPTH_M: f64 = 1.0;
const MIN_POINTS: usize = 10;

/// Add downloaded dives to the logbook, into the entry of the same dive if
/// another computer logged it. Returns how many went into an existing entry.
pub fn add_dives(dives: &mut Vec<DiveLog>, new: Vec<DiveLog>) -> usize {
    let mut merged = 0;
    for dive in new {
        let same = dives
            .iter()
            .enumerate()
            .filter(|(_, existing)| same_dive(existing, &dive))
            .min_by_key(|(_, existing)| (existing.datetime - dive.datetime).num_seconds().abs())
            .map(|(index, _)| index);
        match same {
            Some(index) => {
                dives[index] = merge(std::mem::take(&mut dives[index]), dive);
                merged += 1;
            }
            None => dives.push(dive),
        }
    }
    dives.sort_by_key(|dive| (dive.datetime, dive.number));
    merged
}

/// Whether two entries are one dive: they share most of their time and about
/// the same depth, and do not come from the same computer.
fn same_dive(a: &DiveLog, b: &DiveLog) -> bool {
    if a.dive_mode == DiveMode::Freedive || b.dive_mode == DiveMode::Freedive {
        return false;
    }
    if (a.number > 0 && b.number > 0) || (a.dsx_number.is_some() && b.dsx_number.is_some()) {
        return false;
    }
    if (a.datetime - b.datetime).num_seconds().abs() > MAX_START_GAP_S {
        return false;
    }

    let end =
        |dive: &DiveLog| dive.datetime + chrono::Duration::seconds(dive.duration_seconds as i64);
    let shared = (end(a).min(end(b)) - a.datetime.max(b.datetime)).num_seconds();
    let shorter = a.duration_seconds.min(b.duration_seconds) as i64;
    let deepest = a.max_depth_m.max(b.max_depth_m);
    shorter > 0
        && shared * 2 >= shorter
        && (a.max_depth_m - b.max_depth_m).abs() <= (deepest * 0.2).max(3.0)
}

/// Seconds between samples, as most of them are.
fn interval(dive: &DiveLog) -> Option<u32> {
    let mut steps: Vec<u32> = dive
        .samples
        .windows(2)
        .map(|pair| pair[1].time_s.saturating_sub(pair[0].time_s))
        .collect();
    steps.sort_unstable();
    steps.get(steps.len() / 2).copied()
}

fn merge(existing: DiveLog, incoming: DiveLog) -> DiveLog {
    let incoming_is_finer = match (interval(&existing), interval(&incoming)) {
        (None, Some(_)) => true,
        (Some(old), Some(new)) => new < old,
        _ => false,
    };
    let (mut base, other) = if incoming_is_finer {
        (incoming, existing)
    } else {
        (existing, incoming)
    };

    if base.number == 0 {
        base.number = other.number;
    }
    base.dsx_number = base.dsx_number.or(other.dsx_number);
    base.ignored |= other.ignored;

    macro_rules! or_other {
        ($($field:ident),* $(,)?) => {
            $(base.$field = base.$field.take().or(other.$field.clone());)*
        };
    }
    or_other!(
        end_datetime,
        avg_depth_m,
        min_temp_c,
        max_temp_c,
        water,
        atmospheric_mbar,
        surface_interval_s,
        cns_start_pct,
        cns_end_pct,
        otu_start,
        otu_end,
        max_ascent_speed_m_min,
        battery_start_pct,
        battery_end_pct,
        start_position,
        end_position,
        site,
        country,
        utc_offset,
        buddy,
    );
    macro_rules! if_empty {
        ($($field:ident),* $(,)?) => {
            $(if base.$field.is_empty() {
                base.$field = other.$field.clone();
            })*
        };
    }
    if_empty!(gradient_factors, tissue_n2_mbar, tissue_he_mbar, dips);
    for alarm in &other.alarms {
        if !base.alarms.contains(alarm) {
            base.alarms.push(alarm.clone());
        }
    }

    // The gases come from the computer that knows the tanks
    let has_tank = |dive: &DiveLog| dive.gas_mixes.iter().any(|gas| gas.tank.is_some());
    let other_gases = base.gas_mixes.is_empty() || (has_tank(&other) && !has_tank(&base));
    if other_gases {
        base.gas_mixes = other.gas_mixes.clone();
        base.dive_mode = other.dive_mode;
    }

    fill_samples(
        &mut base.samples,
        &other.samples,
        other_gases,
        interval(&other),
    );
    base
}

/// Depth at a moment of a profile, between its samples.
fn depth_at(samples: &[Sample], time_s: f64) -> Option<f64> {
    let after = samples.partition_point(|sample| (sample.time_s as f64) < time_s);
    let next = samples.get(after)?;
    if next.time_s as f64 == time_s {
        return Some(next.depth_m);
    }
    let previous = samples.get(after.checked_sub(1)?)?;
    let part = (time_s - previous.time_s as f64) / (next.time_s - previous.time_s) as f64;
    Some(previous.depth_m + part * (next.depth_m - previous.depth_m))
}

/// Seconds to add to a moment of `base` to find it in `other`: where their
/// depths follow each other best. Neither log keeps the seconds of its start.
fn best_shift(base: &[Sample], other: &[Sample]) -> i64 {
    let points: Vec<&Sample> = other
        .iter()
        .filter(|sample| sample.depth_m > MIN_DEPTH_M)
        .collect();

    let mut best = (f64::INFINITY, 0);
    for shift in -MAX_SHIFT_S..=MAX_SHIFT_S {
        let differences: Vec<f64> = points
            .iter()
            .filter_map(|point| {
                let depth = depth_at(base, point.time_s as f64 - shift as f64)?;
                Some(depth - point.depth_m)
            })
            .collect();
        if differences.len() < MIN_POINTS {
            continue;
        }
        let mean = differences.iter().sum::<f64>() / differences.len() as f64;
        let spread = differences.iter().map(|d| (d - mean).powi(2)).sum::<f64>();
        let spread = spread / differences.len() as f64;
        if spread < best.0 || (spread == best.0 && shift.abs() < i64::abs(best.1)) {
            best = (spread, shift);
        }
    }
    best.1
}

/// Index of the sample nearest to a moment.
fn nearest(samples: &[Sample], time_s: i64) -> Option<usize> {
    let after = samples.partition_point(|sample| (sample.time_s as i64) < time_s);
    let distance = |index: usize| (samples[index].time_s as i64 - time_s).abs();
    match (
        after.checked_sub(1),
        (after < samples.len()).then_some(after),
    ) {
        (Some(before), Some(after)) if distance(before) <= distance(after) => Some(before),
        (_, Some(after)) => Some(after),
        (before, None) => before,
    }
}

/// Complete the samples of `base` with what the other computer logged at the
/// same moment. What marks a moment, alarms and bookmarks, goes on one sample
/// only.
fn fill_samples(base: &mut [Sample], other: &[Sample], other_gases: bool, interval: Option<u32>) {
    let (Some(interval), false) = (interval, base.is_empty()) else {
        return;
    };
    let shift = best_shift(base, other);

    for sample in base.iter_mut() {
        let moment = sample.time_s as i64 + shift;
        let Some(index) = nearest(other, moment) else {
            continue;
        };
        let source = &other[index];
        if (source.time_s as i64 - moment).abs() > interval as i64 {
            continue;
        }
        macro_rules! or_source {
            ($($field:ident),* $(,)?) => {
                $(sample.$field = sample.$field.take().or(source.$field.clone());)*
            };
        }
        or_source!(
            temp_c,
            pressure_bar,
            ambient_mbar,
            speed_m_min,
            ndl_min,
            deco_time_min,
            deco_stop_m,
            gf_pct,
            surface_gf_pct,
            safety_stop,
            gas_time_min,
            sac_l_min,
        );
        if other_gases {
            sample.gas = source.gas;
        }
        if sample.gf_set == 0 {
            sample.gf_set = source.gf_set;
        }
    }

    for source in other {
        if source.alarms.is_empty() && source.bookmark == 0 {
            continue;
        }
        if let Some(index) = nearest(base, source.time_s as i64 - shift) {
            base[index].alarms.extend(source.alarms.iter().cloned());
            if source.bookmark != 0 {
                base[index].bookmark = source.bookmark;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(hour: u32, minute: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 1)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    /// A profile that cannot be mistaken at another moment.
    fn depth(time_s: f64) -> f64 {
        12.0 + 6.0 * (time_s / 97.0).sin() + 3.0 * (time_s / 31.0).cos()
    }

    /// A Sirius logs every 5 s and knows the tank. Its clock runs 3 s ahead
    /// of the other log.
    fn sirius() -> DiveLog {
        DiveLog {
            number: 146,
            datetime: at(9, 42),
            duration_seconds: 1800,
            max_depth_m: 21.0,
            dive_mode: DiveMode::Air,
            battery_end_pct: Some(94),
            alarms: vec!["slow_down".to_string()],
            gas_mixes: vec![GasMix {
                o2: 21,
                tank: Some(Tank {
                    start_bar: 200.0,
                    end_bar: 80.0,
                    volume_l: Some(12),
                    working_bar: Some(200),
                }),
                ..Default::default()
            }],
            samples: (0..360)
                .map(|i| Sample {
                    time_s: i * 5,
                    depth_m: depth(i as f64 * 5.0 - 3.0),
                    pressure_bar: Some(200.0 - i as f64 / 3.0),
                    alarms: if i == 100 {
                        vec!["slow_down".to_string()]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    /// A DSX logs every 2 s and has a GPS, not the tank.
    fn dsx() -> DiveLog {
        DiveLog {
            dsx_number: Some(20),
            datetime: at(9, 42),
            duration_seconds: 1780,
            max_depth_m: 21.2,
            dive_mode: DiveMode::Air,
            start_position: Some(Position {
                lat: 36.0175,
                lon: 14.2528,
            }),
            gas_mixes: vec![GasMix {
                o2: 21,
                ..Default::default()
            }],
            samples: (0..900)
                .map(|i| Sample {
                    time_s: i * 2,
                    depth_m: depth(i as f64 * 2.0),
                    temp_c: Some(23.0),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn check_merged(dive: &DiveLog) {
        assert_eq!((dive.number, dive.dsx_number), (146, Some(20)));
        // The finer profile, with the tank of the other computer on it
        assert_eq!(dive.samples.len(), 900);
        assert_eq!(dive.samples[1].time_s, 2);
        assert_eq!(dive.duration_seconds, 1780);
        let sample = &dive.samples[300]; // 600 s here, 603 s on the Sirius
        assert_eq!(sample.temp_c, Some(23.0));
        assert!((sample.pressure_bar.unwrap() - (200.0 - 121.0 / 3.0)).abs() < 1e-9);
        assert!(dive.gas_mixes[0].tank.is_some());
        // Each computer brings what it alone has
        assert!(dive.start_position.is_some());
        assert_eq!(dive.battery_end_pct, Some(94));
        assert_eq!(dive.alarms, ["slow_down"]);
        let raised: usize = dive.samples.iter().map(|sample| sample.alarms.len()).sum();
        assert_eq!(raised, 1);
    }

    #[test]
    fn the_same_dive_merges_whichever_computer_comes_first() {
        for (first, second) in [(sirius(), dsx()), (dsx(), sirius())] {
            let mut dives = vec![first];
            assert_eq!(add_dives(&mut dives, vec![second]), 1);
            assert_eq!(dives.len(), 1);
            check_merged(&dives[0]);
        }
    }

    #[test]
    fn profiles_are_lined_up_on_their_depths() {
        assert_eq!(best_shift(&dsx().samples, &sirius().samples), 3);
        assert_eq!(best_shift(&sirius().samples, &dsx().samples), -3);
    }

    #[test]
    fn what_was_set_by_hand_stays() {
        let mut first = sirius();
        first.site = Some("Blue Hole".to_string());
        first.country = Some("Malte".to_string());
        first.utc_offset = Some("+04:00".to_string());
        first.ignored = true;
        let mut dives = vec![first];

        add_dives(&mut dives, vec![dsx()]);
        assert_eq!(dives[0].site.as_deref(), Some("Blue Hole"));
        assert_eq!(dives[0].country.as_deref(), Some("Malte"));
        assert_eq!(dives[0].utc_offset.as_deref(), Some("+04:00"));
        assert!(dives[0].ignored);
    }

    #[test]
    fn a_logbook_entry_gets_the_profile() {
        let logbook = DiveLog {
            datetime: at(9, 42),
            duration_seconds: 1800,
            max_depth_m: 21.0,
            site: Some("Xwejni Bay".to_string()),
            ..Default::default()
        };
        assert!(logbook.is_logbook_only());
        let mut dives = vec![logbook];

        assert_eq!(add_dives(&mut dives, vec![dsx()]), 1);
        assert_eq!(dives[0].samples.len(), 900);
        assert_eq!(dives[0].site.as_deref(), Some("Xwejni Bay"));
        assert!(!dives[0].is_logbook_only());
    }

    #[test]
    fn other_dives_stay_apart() {
        let new = |change: fn(&mut DiveLog)| {
            let mut dive = dsx();
            change(&mut dive);
            let mut dives = vec![sirius()];
            (add_dives(&mut dives, vec![dive]), dives.len())
        };

        assert_eq!(new(|_| ()), (1, 1));
        // The next dive of the day
        assert_eq!(new(|dive| dive.datetime = at(13, 0)), (0, 2));
        // Not the same depth
        assert_eq!(new(|dive| dive.max_depth_m = 40.0), (0, 2));
        // A freedive session
        assert_eq!(new(|dive| dive.dive_mode = DiveMode::Freedive), (0, 2));
        // Two dives of the same computer are two dives
        assert_eq!(new(|dive| dive.number = 147), (0, 2));

        // Starts within the limit, but barely shares any time
        let mut short = sirius();
        short.duration_seconds = 300;
        let mut next = dsx();
        next.datetime = at(9, 46);
        let mut dives = vec![short];
        assert_eq!(add_dives(&mut dives, vec![next]), 0);
    }
}
