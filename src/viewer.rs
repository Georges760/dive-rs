//! What the two viewers share, in the terminal and in a window: the list of
//! dives and the file it is written back to, and what they say of a dive.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::stats::{sac_l_min, Stats};
use crate::types::{logbook_numbers, DiveData, DiveLog, DiveMode, SafetyStop, Sample, Water};

/// The dives of a file as the viewers list them.
pub struct Logbook {
    /// The file the dives come from, written back when one is ignored
    pub path: PathBuf,
    /// The dives, in the order of the file
    pub dives: Vec<DiveLog>,
    /// Number of each dive in the logbook; none for an ignored dive
    pub numbers: Vec<Option<u32>>,
    /// Indexes of the dives in the list, most recent first
    pub listed: Vec<usize>,
    /// Row of the list that is selected
    pub selected: usize,
    /// Whether the list has the ignored dives too
    pub show_ignored: bool,
    /// What went wrong when the file was last written
    pub error: Option<String>,
}

impl Logbook {
    /// Read the dives of a file; none when it holds no dive.
    pub fn open(path: PathBuf) -> Result<Option<Self>> {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let data: DiveData = serde_json::from_str(&contents)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok((!data.dives.is_empty()).then(|| Self::new(path, data.dives)))
    }

    pub fn new(path: PathBuf, dives: Vec<DiveLog>) -> Self {
        let mut logbook = Self {
            path,
            dives,
            numbers: Vec::new(),
            listed: Vec::new(),
            selected: 0,
            show_ignored: false,
            error: None,
        };
        logbook.list_dives(None);
        logbook
    }

    /// Index of the selected dive, if the list has any.
    pub fn current(&self) -> Option<usize> {
        self.listed.get(self.selected).copied()
    }

    /// Row of the last dive of the list.
    pub fn last_row(&self) -> usize {
        self.listed.len().saturating_sub(1)
    }

    pub fn ignored(&self) -> usize {
        self.dives.iter().filter(|dive| dive.ignored).count()
    }

    /// Rebuild the list: latest dive first, without the ignored ones
    /// unless they are asked for. `keep` is the dive to leave selected; when
    /// it is no longer listed, the selection stays on its row.
    pub fn list_dives(&mut self, keep: Option<usize>) {
        self.numbers = logbook_numbers(&self.dives);
        self.listed = (0..self.dives.len())
            .filter(|&dive| self.show_ignored || !self.dives[dive].ignored)
            .collect();
        // By date, as the dives are numbered
        self.listed.sort_by_key(|&dive| {
            let dive = &self.dives[dive];
            std::cmp::Reverse((dive.datetime, dive.number))
        });

        let row = keep.and_then(|keep| self.listed.iter().position(|&dive| dive == keep));
        self.selected = row.unwrap_or(self.selected).min(self.last_row());
    }

    /// List the ignored dives too, or no longer.
    pub fn toggle_show_ignored(&mut self) {
        self.show_ignored = !self.show_ignored;
        self.list_dives(self.current());
    }

    /// Ignore the selected dive, or take it back, and write the file.
    pub fn toggle_ignored(&mut self) {
        let Some(dive) = self.current() else {
            return;
        };
        self.dives[dive].ignored = !self.dives[dive].ignored;
        self.error = self.save().err().map(|error| format!("{error:#}"));
        if self.error.is_some() {
            // Not written: do not show what the file does not hold
            self.dives[dive].ignored = !self.dives[dive].ignored;
        }
        self.list_dives(Some(dive));
    }

    /// Write the dives back to the file they were read from.
    fn save(&mut self) -> Result<()> {
        let data = DiveData {
            dives: std::mem::take(&mut self.dives),
        };
        let json = serde_json::to_string_pretty(&data);
        self.dives = data.dives;

        // Through a temporary file: an interrupted write must not cost the log
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        std::fs::write(&temporary, json?)
            .and_then(|()| std::fs::rename(&temporary, &self.path))
            .with_context(|| format!("Failed to write {}", self.path.display()))
    }
}

pub fn mode_short(mode: &DiveMode) -> &'static str {
    match mode {
        DiveMode::Air => "Air",
        DiveMode::Nitrox => "Nx",
        DiveMode::Gauge => "Gau",
        DiveMode::Freedive => "Free",
        DiveMode::Ccr => "CCR",
    }
}

/// The numbers the dive has on each computer, to find it there.
pub fn computer_numbers(dive: &DiveLog) -> Vec<String> {
    let watch = (dive.number > 0).then(|| format!("watch #{}", dive.number));
    let dsx = dive.dsx_number.map(|number| format!("DSX #{number}"));
    watch.into_iter().chain(dsx).collect()
}

/// What is known of a dive, as (label, value) in two columns.
pub fn dive_fields(dive: &DiveLog) -> [Vec<(&'static str, String)>; 2] {
    let gas = dive
        .gas_mixes
        .iter()
        .map(|g| match g.he {
            0 => format!("{}% O2", g.o2),
            he => format!("{}% O2 {he}% He", g.o2),
        })
        .collect::<Vec<_>>()
        .join(", ");

    // Temperature range from the header, else from the samples, or from the
    // dips of a freedive session
    let (temp_min, temp_max) = match (dive.min_temp_c, dive.max_temp_c) {
        (Some(min), Some(max)) => (min, max),
        _ => dive
            .samples
            .iter()
            .filter_map(|s| s.temp_c)
            .chain(dive.dips.iter().filter_map(|d| d.min_temp_c))
            .fold((f64::MAX, f64::MIN), |(min, max), t| {
                (min.min(t), max.max(t))
            }),
    };

    // Pressure: per tank from the header, else the first and last samples
    let tanks: Vec<_> = dive
        .gas_mixes
        .iter()
        .filter_map(|g| g.tank.as_ref())
        .collect();
    let pressure = if tanks.is_empty() {
        let start = dive.samples.iter().find_map(|s| s.pressure_bar);
        let end = dive.samples.iter().rev().find_map(|s| s.pressure_bar);
        start
            .zip(end)
            .map(|(start, end)| format!("{start:.0} -> {end:.0} bar"))
    } else {
        Some(
            tanks
                .iter()
                .map(|tank| format!("{:.0} -> {:.0} bar", tank.start_bar, tank.end_bar))
                .collect::<Vec<_>>()
                .join(", "),
        )
    };
    let tank_sizes = tanks
        .iter()
        .filter_map(|tank| tank.volume_l.zip(tank.working_bar))
        .map(|(volume, working)| format!("{volume} l, {working} bar"))
        .collect::<Vec<_>>()
        .join("; ");

    let mut date = dive.datetime.format("%Y-%m-%d %H:%M").to_string();
    if let Some(end) = dive.end_datetime {
        date.push_str(&format!(" - {}", end.format("%H:%M")));
    }
    let mut depth = format!("{:.1} m", dive.max_depth_m);
    if let Some(avg) = dive.avg_depth_m {
        depth.push_str(&format!(" (avg {avg:.1} m)"));
    }

    let mut left = vec![
        ("Date", date),
        (
            "Duration",
            format!(
                "{:02}:{:02}",
                dive.duration_seconds / 60,
                dive.duration_seconds % 60
            ),
        ),
        ("Max depth", depth),
    ];

    // A dive from a logbook has no gas to name
    let mut right = Vec::new();
    if !dive.dips.is_empty() {
        right.push(("Dips", dive.dips.len().to_string()));
    } else if !gas.is_empty() {
        right.push(("Gas", gas));
    }

    if temp_min != f64::MAX {
        right.push(("Temp", format!("{temp_min:.1} - {temp_max:.1} C")));
    }

    if let Some(pressure) = pressure {
        right.push(("Pressure", pressure));
    }

    if let Some(ref site) = dive.site {
        left.push(("Site", site.clone()));
    }
    if let Some(ref country) = dive.country {
        right.push(("Country", country.clone()));
    }
    if let Some(ref buddy) = dive.buddy {
        left.push(("Buddy/Ctr", buddy.clone()));
    }

    // 0 when the watch counts the dive as the first
    if let Some(interval) = dive.surface_interval_s.filter(|&s| s > 0) {
        left.push((
            "Surface",
            format!(
                "{}:{:02} h since last dive",
                interval / 3600,
                interval % 3600 / 60
            ),
        ));
    }
    let water = dive.water.map(|water| match water {
        Water::Fresh => "fresh".to_string(),
        Water::Salt => "salt".to_string(),
        Water::En13319 => "EN 13319".to_string(),
    });
    let atmospheric = dive.atmospheric_mbar.map(|mbar| format!("{mbar} mbar"));
    if water.is_some() || atmospheric.is_some() {
        let parts: Vec<String> = water.into_iter().chain(atmospheric).collect();
        left.push(("Water", parts.join(", ")));
    }
    if let (Some(start), Some(end)) = (dive.battery_start_pct, dive.battery_end_pct) {
        left.push(("Battery", format!("{start} -> {end} %")));
    }

    if !tank_sizes.is_empty() {
        right.push(("Tank", tank_sizes));
    }
    if let Some(sac) = sac_l_min(dive) {
        right.push(("Air", format!("{sac:.1} l/min at the surface")));
    }
    if let Some(start) = dive.start_position {
        right.push(("GPS", format!("{:.5}, {:.5}", start.lat, start.lon)));
    }
    if !dive.gradient_factors.is_empty() {
        let sets: Vec<String> = dive
            .gradient_factors
            .iter()
            .map(|gf| format!("{}/{}", gf.low, gf.high))
            .collect();
        right.push(("GF", sets.join(", ")));
    }
    if let (Some(start), Some(end)) = (dive.cns_start_pct, dive.cns_end_pct) {
        right.push(("CNS", format!("{start:.1} -> {end:.1} %")));
    }
    if let (Some(start), Some(end)) = (dive.otu_start, dive.otu_end) {
        right.push(("OTU", format!("{start:.1} -> {end:.1}")));
    }
    if let Some(speed) = dive.max_ascent_speed_m_min {
        right.push(("Ascent", format!("{speed:.1} m/min max")));
    }

    [left, right]
}

/// The alarms of a dive or of a sample, in words.
pub fn alarm_text(alarms: &[String]) -> String {
    alarms.join(", ").replace('_', " ")
}

/// Lowest and highest of the tissue pressures, as one figure when equal.
pub fn tissue_range(pressures: &[f64]) -> String {
    let (min, max) = range(pressures.iter().copied());
    if max > min {
        format!("{min:.0}-{max:.0}")
    } else {
        format!("{min:.0}")
    }
}

/// Lowest and highest of some values; (MAX, MIN) when there is none.
pub fn range(values: impl Iterator<Item = f64>) -> (f64, f64) {
    values.fold((f64::MAX, f64::MIN), |(min, max), v| {
        (min.min(v), max.max(v))
    })
}

/// Time into the dive and of day, depth, tank pressure and temperature at
/// a sample.
pub fn sample_values(dive: &DiveLog, sample: &Sample) -> [String; 4] {
    [
        format!(
            "{:02}:{:02} ({})",
            sample.time_s / 60,
            sample.time_s % 60,
            (dive.datetime + chrono::Duration::seconds(sample.time_s as i64)).format("%H:%M:%S"),
        ),
        format!("{:.1} m", sample.depth_m),
        sample
            .pressure_bar
            .map_or("- bar".to_string(), |p| format!("{p:.0} bar")),
        sample
            .temp_c
            .map_or("- C".to_string(), |t| format!("{t:.1} C")),
    ]
}

/// What the newer logs hold at a sample, as three pieces of text: where the
/// dive stands, what the watch works out, and the alarms it raises.
pub fn sample_readout(sample: &Sample) -> [String; 3] {
    let mut state = Vec::new();
    if let Some(speed) = sample.speed_m_min {
        let arrow = match speed {
            s if s > 0.0 => '↑',
            s if s < 0.0 => '↓',
            _ => '→',
        };
        state.push(format!("{arrow} {:.1} m/min", speed.abs()));
    }
    if let Some(ndl) = sample.ndl_min {
        state.push(format!("NDL {ndl} min"));
    }
    if let Some(time) = sample.deco_time_min {
        state.push(match sample.deco_stop_m {
            Some(stop) => format!("deco {time} min at {stop} m"),
            None => format!("deco {time} min"),
        });
    }
    if let (Some(now), Some(surface)) = (sample.gf_pct, sample.surface_gf_pct) {
        state.push(format!("GF {now:.0} %, surface {surface:.0} %"));
    }
    if let Some(ambient) = sample.ambient_mbar {
        state.push(format!("{ambient} mbar"));
    }

    let mut signals = Vec::new();
    if let Some(minutes) = sample.gas_time_min {
        signals.push(format!("gas for {minutes} min"));
    }
    if let Some(sac) = sample.sac_l_min {
        signals.push(format!("{sac} l/min"));
    }
    if let Some(stop) = sample.safety_stop {
        signals.push(format!(
            "safety stop {}",
            match stop {
                SafetyStop::Due => "due",
                SafetyStop::Running => "running",
                SafetyStop::Paused => "paused",
            }
        ));
    }
    if sample.gas > 0 {
        signals.push(format!("mix {}", sample.gas + 1));
    }
    if sample.gf_set > 0 {
        signals.push(format!("GF set {}", sample.gf_set + 1));
    }
    if sample.bookmark > 0 {
        signals.push(format!("bookmark {}", sample.bookmark));
    }

    [
        state.join("  "),
        signals.join("  "),
        alarm_text(&sample.alarms),
    ]
}

/// The curves of a dive profile on one scale: minutes across, metres down.
pub struct Profile {
    /// End of the time axis, in minutes, rounded up for its labels
    pub time_bound: f32,
    /// Bottom of the depth axis, in metres, rounded up for its labels
    pub depth_bound: f32,
    pub depth: Vec<(f32, f32)>,
    pub temp: Option<Overlay>,
    pub pressure: Option<Overlay>,
}

/// A curve with a range of its own, stretched over the depth axis, its
/// highest value at the top.
pub struct Overlay {
    pub points: Vec<(f32, f32)>,
    pub min: f64,
    pub max: f64,
}

impl Profile {
    pub fn new(dive: &DiveLog) -> Self {
        let max_time = dive.samples.iter().map(|s| s.time_s).max().unwrap_or(0) as f32 / 60.0;
        let max_depth = dive.samples.iter().map(|s| s.depth_m).fold(0.0, f64::max);
        let time_bound = ((max_time / 5.0).ceil() * 5.0).max(5.0);
        let depth_bound = ((max_depth / 5.0).ceil() * 5.0).max(5.0);

        let overlay = |value: fn(&Sample) -> Option<f64>| {
            if !dive.samples.iter().any(|s| value(s).is_some()) {
                return None;
            }
            let (min, max) = range(dive.samples.iter().filter_map(value));
            let span = (max - min).max(1.0);
            let points = dive
                .samples
                .iter()
                .filter_map(|s| {
                    value(s).map(|v| {
                        (
                            s.time_s as f32 / 60.0,
                            ((max - v) / span * depth_bound) as f32,
                        )
                    })
                })
                .collect();
            Some(Overlay { points, min, max })
        };

        Self {
            time_bound,
            depth_bound: depth_bound as f32,
            depth: dive
                .samples
                .iter()
                .map(|s| (s.time_s as f32 / 60.0, s.depth_m as f32))
                .collect(),
            temp: overlay(|s| s.temp_c),
            pressure: overlay(|s| s.pressure_bar),
        }
    }
}

/// Index of the sample nearest to a time into the dive.
pub fn nearest_sample(dive: &DiveLog, time_s: f32) -> Option<usize> {
    dive.samples
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let distance = |s: &Sample| (s.time_s as f32 - time_s).abs();
            distance(a).total_cmp(&distance(b))
        })
        .map(|(index, _)| index)
}

/// The dips of a freedive session: number, start, depth, time, surface
/// before, temperature.
pub fn dip_rows(dive: &DiveLog) -> Vec<[String; 6]> {
    // Start times are rebuilt by adding up surface and dive times
    let mut start_s = 0;
    dive.dips
        .iter()
        .enumerate()
        .map(|(i, dip)| {
            start_s += dip.surface_s;
            let row = [
                (i + 1).to_string(),
                format!("{}:{:02}", start_s / 60, start_s % 60),
                format!("{:.1} m", dip.max_depth_m),
                format!("{}:{:02}", dip.duration_s / 60, dip.duration_s % 60),
                format!("{}:{:02}", dip.surface_s / 60, dip.surface_s % 60),
                dip.min_temp_c
                    .map(|t| format!("{t:.1} C"))
                    .unwrap_or_default(),
            ];
            start_s += dip.duration_s;
            row
        })
        .collect()
}

pub fn hours(seconds: u64) -> String {
    format!("{:.0} h", seconds as f64 / 3600.0)
}

/// The totals, the records and the most dived sites, a line each.
pub fn stats_summary(stats: &Stats) -> [String; 3] {
    let named = stats.countries.iter().filter(|(name, _)| !name.is_empty());
    let totals = format!(
        "{} dives, {} freedive sessions, {} under water, {} sites, {} countries",
        stats.dives,
        stats.freedive_sessions,
        hours(stats.time_s),
        stats.sites.len(),
        named.count(),
    );

    let place = |site: &String| match site.as_str() {
        "" => String::new(),
        site => format!(", {site}"),
    };
    let mut records = Vec::new();
    if let Some((depth, date, site)) = &stats.deepest {
        records.push(format!("Deepest {depth:.1} m ({date}{})", place(site)));
    }
    if let Some((seconds, date, site)) = &stats.longest {
        records.push(format!(
            "Longest {} min ({date}{})",
            seconds / 60,
            place(site)
        ));
    }

    let sites: Vec<String> = stats
        .sites
        .iter()
        .take(4)
        .map(|(site, dives)| format!("{site} ({dives})"))
        .collect();
    [
        totals,
        records.join("   "),
        format!("Most dived: {}", sites.join(", ")),
    ]
}
