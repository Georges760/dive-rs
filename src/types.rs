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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiveMode {
    Air,
    Gauge,
    Nitrox,
    Freedive,
}

/// A single gas mix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GasMix {
    pub o2: u8,
}

/// A single dive sample point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub time_s: u32,
    pub depth_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure_bar: Option<f64>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiveLog {
    pub number: u32,
    #[serde(with = "datetime_format")]
    pub datetime: NaiveDateTime,
    pub duration_seconds: u32,
    pub max_depth_m: f64,
    pub dive_mode: DiveMode,
    pub gas_mixes: Vec<GasMix>,
    pub samples: Vec<Sample>,
    /// Dips of a freedive session; the watch keeps no depth samples for them
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub dips: Vec<Dip>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub buddy: Option<String>,
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
