use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

/// Model IDs from libdivecomputer descriptor table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Model {
    IconHD = 0x14,
    IconAir = 0x15,
    PuckPro = 0x18,
    NemoWide2 = 0x19,
    Genius = 0x1C,
    Puck2 = 0x1F,
    QuadAir = 0x23,
    SmartAir = 0x24,
    Quad = 0x29,
    Horizon = 0x2C,
    PuckAir2 = 0x2D,
    Sirius = 0x2F,
    QuadCi = 0x31,
    Quad2 = 0x32,
    Puck4 = 0x35,
    Unknown = 0xFF,
}

impl Model {
    pub fn from_name(name: &str) -> Self {
        match name.trim_end_matches('\0').trim() {
            "Icon HD" => Model::IconHD,
            "Icon AIR" => Model::IconAir,
            "Puck Pro" | "Puck Pro+" => Model::PuckPro,
            "Nemo Wide 2" => Model::NemoWide2,
            "Genius" => Model::Genius,
            "Puck 2" => Model::Puck2,
            "Quad Air" => Model::QuadAir,
            "Smart Air" => Model::SmartAir,
            "Quad" => Model::Quad,
            "Horizon" => Model::Horizon,
            "Puck Air 2" => Model::PuckAir2,
            "Sirius" => Model::Sirius,
            "Quad Ci" => Model::QuadCi,
            "Quad2" => Model::Quad2,
            "Puck4" | "Puck Lite" | "Puck" | "Puck Pro U" => Model::Puck4,
            _ => Model::Unknown,
        }
    }
}

/// Dive mode from the GENIUS settings field.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiveMode {
    #[default]
    Air,
    Gauge,
    Nitrox,
    Freedive,
}

/// Water type set on the watch.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Water {
    Fresh,
    Salt,
    En13319,
}

/// A pair of gradient factors, in percent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct GradientFactors {
    pub low: u8,
    pub high: u8,
}

/// Where the safety stop stands. Worked out from the logs: no reference
/// names these states.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SafetyStop {
    /// The dive went deep enough to call for one
    Due,
    /// Being counted down
    Running,
    /// Interrupted by going back below the stop depths
    Paused,
}

fn is_zero(value: &u8) -> bool {
    *value == 0
}

/// A tank whose pressure a transmitter reported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tank {
    pub start_bar: f64,
    pub end_bar: f64,
    /// Size and working pressure as set on the watch
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_l: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_bar: Option<u32>,
}

/// A single gas mix.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GasMix {
    pub o2: u8,
    /// Helium %, for trimix
    #[serde(skip_serializing_if = "is_zero", default)]
    pub he: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tank: Option<Tank>,
}

/// A single dive sample point.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sample {
    pub time_s: u32,
    pub depth_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    /// Tank pressure
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure_bar: Option<f64>,
    /// Absolute pressure at the watch
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambient_mbar: Option<u32>,
    /// Vertical speed, positive going up
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_m_min: Option<f64>,
    /// No-decompression time left; the watch counts no higher than 99
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ndl_min: Option<u32>,
    /// Decompression time, once a stop is required
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deco_time_min: Option<u32>,
    /// Depth of the required decompression stop
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deco_stop_m: Option<u32>,
    /// Gradient factor at the current depth, negative while on-gassing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gf_pct: Option<f64>,
    /// Gradient factor the diver would have on surfacing now
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface_gf_pct: Option<f64>,
    /// Index in `gas_mixes` of the gas in use
    #[serde(skip_serializing_if = "is_zero", default)]
    pub gas: u8,
    /// Index in `gradient_factors` of the set in use
    #[serde(skip_serializing_if = "is_zero", default)]
    pub gf_set: u8,
    #[serde(skip_serializing_if = "is_zero", default)]
    pub bookmark: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_stop: Option<SafetyStop>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub alarms: Vec<String>,
    /// Time the gas left in the tank lasts at this depth; the watch counts
    /// no higher than 99. Worked out from the logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_time_min: Option<u32>,
    /// Gas consumption brought back to surface pressure. Worked out from
    /// the logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sac_l_min: Option<u32>,
}

/// One immersion of a freedive session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dip {
    /// Time spent at the surface before this dip
    pub surface_s: u32,
    pub duration_s: u32,
    pub max_depth_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_temp_c: Option<f64>,
}

/// A parsed dive log entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiveLog {
    /// Number the dive computer gives the dive; 0 for a dive that comes
    /// from a logbook only. Not the number shown: see `logbook_numbers`.
    pub number: u32,
    #[serde(with = "datetime_format")]
    pub datetime: NaiveDateTime,
    /// When the watch closed the dive, surface timeout included
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_datetime: Option<NaiveDateTime>,
    pub duration_seconds: u32,
    pub max_depth_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_depth_m: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_temp_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_temp_c: Option<f64>,
    pub dive_mode: DiveMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub water: Option<Water>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atmospheric_mbar: Option<u32>,
    /// Time at the surface since the previous dive; 0 when the watch counts
    /// this dive as the first
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface_interval_s: Option<u32>,
    /// Oxygen toxicity: CNS clock and pulmonary dose, before and after
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cns_start_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cns_end_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otu_start: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otu_end: Option<f64>,
    /// Gradient factor sets configured on the watch
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub gradient_factors: Vec<GradientFactors>,
    /// Fastest ascent of the dive. Worked out from the logs, as are the
    /// battery levels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_ascent_speed_m_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_start_pct: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_end_pct: Option<u8>,
    /// Every alarm raised during the dive
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub alarms: Vec<String>,
    /// Inert gas pressure in the 16 tissue compartments at the start
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tissue_n2_mbar: Vec<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tissue_he_mbar: Vec<f64>,
    pub gas_mixes: Vec<GasMix>,
    pub samples: Vec<Sample>,
    /// Dips of a freedive session; the watch keeps no depth samples for them
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub dips: Vec<Dip>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub country: Option<String>,
    /// Time zone the FIT export takes the dive computer to be on for this
    /// dive, as an offset from UTC such as "+03:00". Set by hand, to
    /// overrule the time zone of the country.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub utc_offset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub buddy: Option<String>,
    /// Set aside in the viewer. Kept in the file all the same: a dive that
    /// is not there would come back with the next download.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub ignored: bool,
}

impl DiveLog {
    /// Whether the dive was imported from a logbook: no dive computer
    /// handed it over, so it has no number of its own and no profile.
    pub fn is_logbook_only(&self) -> bool {
        self.number == 0
    }
}

/// Number of each dive in the logbook, counted as the SSI logbook does: by
/// date, scuba dives and freedive sessions each on their own. An ignored
/// dive has none, and leaves no gap.
pub fn logbook_numbers(dives: &[DiveLog]) -> Vec<Option<u32>> {
    let mut by_date: Vec<usize> = (0..dives.len()).collect();
    by_date.sort_by_key(|&dive| (dives[dive].datetime, dives[dive].number));

    let mut numbers = vec![None; dives.len()];
    let (mut scuba, mut freedives) = (0, 0);
    for dive in by_date {
        if dives[dive].ignored {
            continue;
        }
        let count = match dives[dive].dive_mode {
            DiveMode::Freedive => &mut freedives,
            _ => &mut scuba,
        };
        *count += 1;
        numbers[dive] = Some(*count);
    }
    numbers
}

/// Collection of all parsed dives.
#[derive(Debug, Serialize, Deserialize)]
pub struct DiveData {
    pub dives: Vec<DiveLog>,
}

/// Device info returned by CMD_VERSION.
#[derive(Debug)]
pub struct DeviceInfo {
    pub model_name: String,
    pub model: Model,
}

mod datetime_format {
    use chrono::NaiveDateTime;
    use serde::{self, Deserializer, Serializer};

    const FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

    pub fn serialize<S>(date: &NaiveDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = date.format(FORMAT).to_string();
        serializer.serialize_str(&s)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<NaiveDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = serde::Deserialize::deserialize(deserializer)?;
        NaiveDateTime::parse_from_str(&s, FORMAT).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logbook_counts_by_date_without_the_ignored_dives() {
        let dive = |day: u32, dive_mode: DiveMode, ignored: bool| DiveLog {
            datetime: chrono::NaiveDate::from_ymd_opt(2026, 8, day)
                .unwrap()
                .and_hms_opt(10, 0, 0)
                .unwrap(),
            dive_mode,
            ignored,
            ..Default::default()
        };
        // Not in date order: the dive of the 1st, from a logbook, comes last
        let dives = [
            dive(2, DiveMode::Air, false),
            dive(3, DiveMode::Freedive, false),
            dive(4, DiveMode::Air, true),
            dive(5, DiveMode::Nitrox, false),
            dive(6, DiveMode::Freedive, false),
            dive(1, DiveMode::Air, false),
        ];
        // Scuba dives 1 to 3, freedive sessions 1 and 2, nothing for the ignored
        assert_eq!(
            logbook_numbers(&dives),
            [Some(2), Some(1), None, Some(3), Some(2), Some(1)]
        );
    }

    #[test]
    fn dives_saved_before_the_extra_fields_read_and_write_back_unchanged() {
        let json = concat!(
            r#"{"dives":[{"number":7,"datetime":"2025-08-01T16:03:00","#,
            r#""duration_seconds":2940,"max_depth_m":31.2,"dive_mode":"air","#,
            r#""gas_mixes":[{"o2":21}],"samples":[{"time_s":0,"depth_m":2.1,"#,
            r#""temp_c":27.0},{"time_s":5,"depth_m":3.0,"pressure_bar":201.5}],"#,
            r#""site":"Blue Hole"}]}"#
        );
        let data: DiveData = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&data).unwrap(), json);
    }
}
