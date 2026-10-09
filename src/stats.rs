//! Figures over the whole logbook, for the statistics page of the viewer.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};

use crate::types::*;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The dives of a year, of a month of the year, or of one month.
#[derive(Debug, Clone, Default)]
pub struct Period {
    pub label: String,
    pub dives: usize,
    /// Dives per country, in the order of `Stats::countries`
    pub by_country: Vec<usize>,
    /// Time under water
    pub time_s: u64,
    depth_sum_m: f64,
    pub max_depth_m: f64,
    sac_sum_l_min: f64,
    /// Dives whose air consumption is known
    pub sac_dives: usize,
}

impl Period {
    fn new(label: String, countries: usize) -> Self {
        Self {
            label,
            by_country: vec![0; countries],
            ..Default::default()
        }
    }

    fn add(&mut self, dive: &DiveLog, country: usize) {
        self.dives += 1;
        self.by_country[country] += 1;
        self.time_s += dive.duration_seconds as u64;
        self.depth_sum_m += dive.max_depth_m;
        self.max_depth_m = self.max_depth_m.max(dive.max_depth_m);
        if let Some(sac) = sac_l_min(dive) {
            self.sac_sum_l_min += sac;
            self.sac_dives += 1;
        }
    }

    /// Mean of the maximum depths of the dives.
    pub fn avg_depth_m(&self) -> f64 {
        self.depth_sum_m / self.dives.max(1) as f64
    }

    /// Mean air consumption of the dives that have one.
    pub fn avg_sac_l_min(&self) -> Option<f64> {
        (self.sac_dives > 0).then(|| self.sac_sum_l_min / self.sac_dives as f64)
    }
}

/// A dive that holds a record: its figure, its date and its site.
pub type Record<T> = Option<(T, NaiveDate, String)>;

/// The logbook in figures. Scuba dives only, ignored dives left out;
/// freedive sessions are just counted.
#[derive(Debug, Default)]
pub struct Stats {
    pub dives: usize,
    pub freedive_sessions: usize,
    pub time_s: u64,
    /// Countries and their dives, the most dived first. The empty name, at
    /// the end, stands for the dives with no country.
    pub countries: Vec<(String, usize)>,
    /// Sites and their dives, the most dived first
    pub sites: Vec<(String, usize)>,
    pub deepest: Record<f64>,
    pub longest: Record<u32>,
    /// Year by year
    pub years: Vec<Period>,
    /// The twelve months of the year, all years together
    pub months: Vec<Period>,
    /// Month by month, those with a known air consumption only
    pub sac_months: Vec<Period>,
}

/// Air a dive used, brought back to surface pressure, in l/min: the gas
/// gone from the tank over the dive time and the mean absolute pressure.
/// `None` without a transmitter or a tank size.
pub fn sac_l_min(dive: &DiveLog) -> Option<f64> {
    let tank = dive.gas_mixes.iter().find_map(|gas| gas.tank.as_ref())?;
    let litres = (tank.start_bar - tank.end_bar) * tank.volume_l? as f64;

    // Absolute pressure over the dive time; the log runs on at the surface
    let pressures: Vec<f64> = dive
        .samples
        .iter()
        .filter(|sample| sample.time_s <= dive.duration_seconds)
        .map(|sample| match sample.ambient_mbar {
            Some(mbar) => mbar as f64 / 1000.0,
            None => 1.013 + sample.depth_m / 10.0,
        })
        .collect();
    if litres <= 0.0 || dive.duration_seconds == 0 || pressures.is_empty() {
        return None;
    }
    let pressure = pressures.iter().sum::<f64>() / pressures.len() as f64;
    Some(litres / (dive.duration_seconds as f64 / 60.0) / pressure)
}

/// Count names, the most frequent first.
fn ranked<'a>(names: impl Iterator<Item = &'a str>) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for name in names {
        *counts.entry(name).or_default() += 1;
    }
    let mut ranked: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(name, count)| (name.to_string(), count))
        .collect();
    ranked.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    ranked
}

pub fn stats(dives: &[DiveLog]) -> Stats {
    let logged = || dives.iter().filter(|dive| !dive.ignored);
    let scuba = || logged().filter(|dive| dive.dive_mode != DiveMode::Freedive);
    let country = |dive: &DiveLog| dive.country.clone().unwrap_or_default();
    let site = |dive: &DiveLog| dive.site.clone().unwrap_or_default();

    let mut countries = ranked(scuba().filter_map(|dive| dive.country.as_deref()));
    let unnamed = scuba().filter(|dive| dive.country.is_none()).count();
    if unnamed > 0 {
        countries.push((String::new(), unnamed));
    }
    let country_index = |dive: &DiveLog| {
        let name = country(dive);
        countries.iter().position(|(c, _)| *c == name).unwrap_or(0)
    };

    let mut years: BTreeMap<i32, Period> = BTreeMap::new();
    let mut months: Vec<Period> = MONTHS
        .iter()
        .map(|month| Period::new(month.to_string(), countries.len()))
        .collect();
    let mut year_months: BTreeMap<(i32, u32), Period> = BTreeMap::new();
    for dive in scuba() {
        let (year, month) = (dive.datetime.year(), dive.datetime.month());
        let index = country_index(dive);
        years
            .entry(year)
            .or_insert_with(|| Period::new(year.to_string(), countries.len()))
            .add(dive, index);
        months[month as usize - 1].add(dive, index);
        year_months
            .entry((year, month))
            .or_insert_with(|| Period::new(format!("{year}-{month:02}"), countries.len()))
            .add(dive, index);
    }

    let deepest = scuba().max_by(|a, b| a.max_depth_m.total_cmp(&b.max_depth_m));
    let longest = scuba().max_by_key(|dive| dive.duration_seconds);
    Stats {
        dives: scuba().count(),
        freedive_sessions: logged().count() - scuba().count(),
        time_s: scuba().map(|dive| dive.duration_seconds as u64).sum(),
        sites: ranked(scuba().filter_map(|dive| dive.site.as_deref())),
        deepest: deepest.map(|dive| (dive.max_depth_m, dive.datetime.date(), site(dive))),
        longest: longest.map(|dive| (dive.duration_seconds, dive.datetime.date(), site(dive))),
        years: years.into_values().collect(),
        months,
        sac_months: year_months
            .into_values()
            .filter(|month| month.sac_dives > 0)
            .collect(),
        countries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dive(date: (i32, u32, u32), country: Option<&str>, depth_m: f64, minutes: u32) -> DiveLog {
        DiveLog {
            datetime: NaiveDate::from_ymd_opt(date.0, date.1, date.2)
                .unwrap()
                .and_hms_opt(10, 0, 0)
                .unwrap(),
            duration_seconds: minutes * 60,
            max_depth_m: depth_m,
            country: country.map(String::from),
            site: Some("Blue Hole".to_string()),
            ..Default::default()
        }
    }

    fn logbook() -> Vec<DiveLog> {
        let mut dives = vec![
            dive((2025, 7, 30), Some("Malte"), 30.0, 40),
            dive((2025, 7, 31), Some("Malte"), 20.0, 50),
            dive((2026, 5, 18), Some("Egypte"), 40.0, 60),
            dive((2026, 7, 2), Some("Malte"), 10.0, 30),
            dive((2026, 7, 3), None, 12.0, 20),
            // Neither of these counts as a dive
            dive((2026, 7, 4), Some("Malte"), 99.0, 99),
            dive((2026, 7, 5), Some("Malte"), 13.0, 20),
        ];
        dives[5].ignored = true;
        dives[6].dive_mode = DiveMode::Freedive;
        dives
    }

    #[test]
    fn dives_are_counted_by_year_month_and_country() {
        let stats = stats(&logbook());

        assert_eq!((stats.dives, stats.freedive_sessions), (5, 1));
        assert_eq!(stats.time_s, 200 * 60);
        // The most dived country first, the dives with none last
        assert_eq!(
            stats.countries,
            [
                ("Malte".to_string(), 3),
                ("Egypte".to_string(), 1),
                (String::new(), 1)
            ]
        );
        assert_eq!(stats.sites, [("Blue Hole".to_string(), 5)]);
        assert_eq!(stats.deepest.as_ref().unwrap().0, 40.0);
        assert_eq!(stats.longest.as_ref().unwrap().0, 3600);

        let years: Vec<(&str, usize, &[usize])> = stats
            .years
            .iter()
            .map(|year| (year.label.as_str(), year.dives, &year.by_country[..]))
            .collect();
        assert_eq!(
            years,
            [("2025", 2, &[2, 0, 0][..]), ("2026", 3, &[1, 1, 1][..])]
        );
        assert_eq!(stats.years[0].avg_depth_m(), 25.0);
        assert_eq!(stats.years[0].max_depth_m, 30.0);
        assert_eq!(stats.years[1].time_s, 110 * 60);

        // July of both years together
        let july = &stats.months[6];
        assert_eq!((july.label.as_str(), july.dives), ("Jul", 4));
        assert_eq!(july.by_country, [3, 0, 1]);
        assert_eq!(stats.months[0].dives, 0);
    }

    #[test]
    fn air_consumption_is_brought_back_to_the_surface() {
        // 100 bar out of a 12 l tank in 40 minutes at 3 bar: 10 l/min
        let mut dive = dive((2026, 5, 18), Some("Egypte"), 20.0, 40);
        dive.gas_mixes = vec![GasMix {
            o2: 21,
            tank: Some(Tank {
                start_bar: 200.0,
                end_bar: 100.0,
                volume_l: Some(12),
                working_bar: Some(200),
            }),
            ..Default::default()
        }];
        dive.samples = vec![
            Sample {
                time_s: 5,
                ambient_mbar: Some(3000),
                ..Default::default()
            },
            // At the surface, once the dive time is over
            Sample {
                time_s: 2500,
                ambient_mbar: Some(1000),
                ..Default::default()
            },
        ];
        assert_eq!(sac_l_min(&dive), Some(10.0));

        // Month by month: only the months with such a dive
        let other = self::dive((2026, 7, 2), Some("Malte"), 10.0, 30);
        let stats = stats(&[other, dive.clone()]);
        assert_eq!(stats.sac_months.len(), 1);
        let may = &stats.sac_months[0];
        assert_eq!((may.label.as_str(), may.sac_dives), ("2026-05", 1));
        assert_eq!(may.avg_sac_l_min(), Some(10.0));

        // No transmitter, no consumption
        dive.gas_mixes[0].tank = None;
        assert_eq!(sac_l_min(&dive), None);
    }
}
