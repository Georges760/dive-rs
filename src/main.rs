mod ble;
mod fit;
mod parser;
mod protocol;
mod tui;
mod types;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use btleplug::api::Peripheral as _;
use clap::{Parser, Subcommand, ValueEnum};

use crate::types::*;

#[derive(Parser)]
#[command(name = "sirius-dive")]
#[command(about = "Extract dive logs from Mares Sirius dive computer via BLE")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan for Mares BLE devices and enumerate their GATT services
    Scan {
        /// Scan duration in seconds
        #[arg(short, long, default_value = "10")]
        timeout: u64,

        /// Connect to the first found device and enumerate GATT
        #[arg(short, long)]
        enumerate: bool,
    },

    /// Connect and query device info (model, serial, firmware)
    Info {
        /// BLE device address (e.g. "AA:BB:CC:DD:EE:FF"). If omitted, connects to first Mares device found.
        #[arg(short, long)]
        address: Option<String>,
    },

    /// Download dive logs from the device
    Download {
        /// BLE device address. If omitted, connects to first Mares device found.
        #[arg(short, long)]
        address: Option<String>,

        /// Output file path
        #[arg(short, long, default_value = "dives.json")]
        output: PathBuf,

        /// Output format
        #[arg(short, long, default_value = "json")]
        format: OutputFormat,

        /// Save raw dive data for debugging
        #[arg(long)]
        save_raw: Option<PathBuf>,
    },

    /// Raw protocol debug: test ECOP SDO communication
    Debug {
        /// BLE device address. If omitted, connects to first Mares device found.
        #[arg(short, long)]
        address: Option<String>,
    },

    /// Read raw SDO objects and hex-dump them (protocol exploration)
    Sdo {
        /// Objects to read in order, as INDEX:SUB[,SUB...], e.g. 0x3014:4,3 0x2008:1
        #[arg(required = true, value_parser = parse_sdo_spec)]
        objects: Vec<SdoSpec>,

        /// BLE device address. If omitted, connects to first Mares device found.
        #[arg(short, long)]
        address: Option<String>,

        /// Save each object to <DIR>/<index>_<sub>.bin and shorten the hex dump
        #[arg(long)]
        save: Option<PathBuf>,
    },

    /// View dive logs in an interactive TUI (offline, no BLE needed)
    View {
        /// Input JSON file with dive data
        #[arg(short, long, default_value = "dives.json")]
        input: PathBuf,
    },

    /// Correlate dive logs with SSI dive log CSV to import site, country, and buddy info
    Correlate {
        /// Path to SSI dive log CSV export
        #[arg(short, long, default_value = "my.DiveSSI.com - mydivelog.csv")]
        ssi: PathBuf,

        /// Path to dives.json to enrich
        #[arg(short, long, default_value = "dives.json")]
        json: PathBuf,
    },

    /// Overlay dive data (depth, temp, pressure) onto a video using ffmpeg
    Watermark {
        /// Path to the video file
        #[arg(short, long)]
        video: PathBuf,

        /// Path to dives.json
        #[arg(short, long, default_value = "dives.json")]
        json: PathBuf,

        /// Time offset in seconds added to dive log start time to correct for
        /// missing second precision. Dive logs are truncated to the minute, so the
        /// recorded start is typically 0–59s early. A positive offset shifts the
        /// dive time forward (common case); a negative offset shifts it back (rare).
        #[arg(short, long, default_value = "0", allow_hyphen_values = true)]
        offset: i64,

        /// Start time of the video on the dive computer's clock, as
        /// "YYYY-MM-DD HH:MM[:SS]". Replaces the capture time stored in the
        /// video, for a camera clock that was wrong or an export without one.
        #[arg(short, long, value_parser = parse_start_time)]
        start: Option<chrono::NaiveDateTime>,
    },

    /// Export dives as FIT activities that look like Garmin Descent dives,
    /// e.g. for the stats dashboard of the Insta360 app (offline, no BLE needed)
    Fit {
        /// Input JSON file with dive data
        #[arg(short, long, default_value = "dives.json")]
        json: PathBuf,

        /// Directory to write one .fit file per dive into
        #[arg(short, long, default_value = "fit")]
        output: PathBuf,

        /// Only export the dives of this day, as YYYY-MM-DD
        #[arg(short, long)]
        date: Option<chrono::NaiveDate>,

        /// Only export the dive with this number in the logbook, as the viewer shows it
        #[arg(short, long)]
        number: Option<u32>,

        /// Time zone the dive computer's clock was set to, as an offset from
        /// UTC such as "+02:00". Defaults to this machine's time zone on the
        /// day of the dive. FIT timestamps are in UTC.
        #[arg(long, allow_hyphen_values = true)]
        utc_offset: Option<chrono::FixedOffset>,

        /// Time offset in seconds added to the dive start time, which the log
        /// only keeps to the minute (as for watermark).
        #[arg(long, default_value = "0", allow_hyphen_values = true)]
        offset: i64,

        /// Seconds between records: the profile is interpolated, as a Descent
        /// logs every second. 0 writes the samples as logged.
        #[arg(long, default_value = "1")]
        interval: u32,
    },

    /// Parse previously downloaded raw dive data (offline, no BLE needed)
    Parse {
        /// Directory containing raw dive data (dive_NNN_header.bin / dive_NNN_profile.bin)
        #[arg(short, long)]
        raw_dir: PathBuf,

        /// Output file path
        #[arg(short, long, default_value = "dives.json")]
        output: PathBuf,

        /// Output format
        #[arg(short, long, default_value = "json")]
        format: OutputFormat,
    },
}

#[derive(Clone, ValueEnum)]
enum OutputFormat {
    Json,
    Csv,
}

fn parse_start_time(s: &str) -> Result<chrono::NaiveDateTime, String> {
    ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|format| chrono::NaiveDateTime::parse_from_str(s, format).ok())
        .ok_or_else(|| "expected \"YYYY-MM-DD HH:MM[:SS]\"".to_string())
}

fn parse_u16(s: &str) -> Result<u16, std::num::ParseIntError> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16),
        None => s.parse(),
    }
}

/// One object index and the sub-indexes to read from it
#[derive(Clone)]
struct SdoSpec {
    index: u16,
    subs: Vec<u8>,
}

fn parse_sdo_spec(s: &str) -> Result<SdoSpec, String> {
    let (index, subs) = s
        .split_once(':')
        .ok_or("expected INDEX:SUB[,SUB...]")?;
    let index = parse_u16(index).map_err(|e| e.to_string())?;
    let subs = subs
        .split(',')
        .map(|sub| sub.parse().map_err(|e| format!("sub-index {sub:?}: {e}")))
        .collect::<Result<_, _>>()?;
    Ok(SdoSpec { index, subs })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Scan { timeout, enumerate } => cmd_scan(timeout, enumerate).await,
        Commands::Info { address } => cmd_info(address).await,
        Commands::Download {
            address,
            output,
            format,
            save_raw,
        } => cmd_download(address, output, format, save_raw).await,
        Commands::Debug { address } => cmd_debug(address).await,
        Commands::Sdo {
            objects,
            address,
            save,
        } => cmd_sdo(address, objects, save).await,
        Commands::View { input } => tui::run(input),
        Commands::Correlate { ssi, json } => cmd_correlate(ssi, json),
        Commands::Watermark {
            video,
            json,
            offset,
            start,
        } => cmd_watermark(video, json, offset, start),
        Commands::Fit {
            json,
            output,
            date,
            number,
            utc_offset,
            offset,
            interval,
        } => cmd_fit(json, output, date, number, utc_offset, offset, interval),
        Commands::Parse {
            raw_dir,
            output,
            format,
        } => cmd_parse(raw_dir, output, format),
    }
}

// ── Scan ──

async fn cmd_scan(timeout_secs: u64, enumerate: bool) -> Result<()> {
    let adapter = ble::get_adapter().await?;

    eprintln!("Scanning for Mares BLE devices ({timeout_secs}s)...");
    let devices = ble::scan_for_devices(&adapter, Duration::from_secs(timeout_secs)).await?;

    if devices.is_empty() {
        eprintln!("No Mares devices found. Make sure the dive computer is in Bluetooth mode.");
        return Ok(());
    }

    println!("\nFound {} device(s):", devices.len());
    for (i, dev) in devices.iter().enumerate() {
        println!(
            "  [{}] {} - {} (RSSI: {})",
            i,
            dev.name,
            dev.address,
            dev.rssi
                .map(|r| format!("{r} dBm"))
                .unwrap_or_else(|| "?".into())
        );
    }

    if enumerate {
        let dev = &devices[0];
        eprintln!("\nConnecting to {}...", dev.name);
        dev.peripheral.connect().await?;

        let services = ble::enumerate_gatt(&dev.peripheral).await?;
        println!("\nGATT Profile for {}:", dev.name);
        for svc in &services {
            println!("  Service: {}", svc.uuid);
            for c in &svc.characteristics {
                println!("    Characteristic: {} [{}]", c.uuid, c.properties);
            }
        }

        dev.peripheral.disconnect().await?;
    }

    Ok(())
}

// ── Sdo ──

async fn cmd_sdo(
    address: Option<String>,
    objects: Vec<SdoSpec>,
    save: Option<PathBuf>,
) -> Result<()> {
    let adapter = ble::get_adapter().await?;
    let peripheral = find_device(&adapter, address.as_deref()).await?;
    let mut conn = ble::connect(&peripheral, None, None).await?;
    protocol::get_device_info(&mut conn).await?;

    // With --save the file has the full data, so only show the start
    let max_rows = if save.is_some() { 4 } else { usize::MAX };

    for SdoSpec { index, subs } in objects {
        for sub in subs {
            // Keep going on errors: one refused object should not cost the session
            let data = match protocol::ecop_read(&mut conn, index, sub).await {
                Ok(data) => data,
                Err(e) => {
                    println!("0x{index:04X} sub {sub}: {e:#}");
                    continue;
                }
            };
            println!("0x{index:04X} sub {sub}: {} bytes", data.len());
            for (i, row) in data.chunks(16).enumerate().take(max_rows) {
                println!("  {:04X}  {}", i * 16, protocol::hex_dump(row));
            }
            if data.len() > max_rows.saturating_mul(16) {
                println!("  ...");
            }
            if let Some(ref dir) = save {
                std::fs::create_dir_all(dir)?;
                std::fs::write(dir.join(format!("{index:04X}_{sub}.bin")), &data)?;
            }
        }
    }

    conn.disconnect().await?;
    Ok(())
}

// ── Debug ──

async fn cmd_debug(address: Option<String>) -> Result<()> {
    let adapter = ble::get_adapter().await?;
    let peripheral = find_device(&adapter, address.as_deref()).await?;
    let mut conn = ble::connect(&peripheral, None, None).await?;

    eprintln!("=== ECOP SDO Protocol Test ===\n");

    // Step 1: CMD_VERSION
    eprintln!("--- Step 1: CMD_VERSION ---");
    let info = protocol::get_device_info(&mut conn).await?;
    eprintln!("  Model: {}", info.model_name);

    // Step 2: Read device info via ECOP
    eprintln!("\n--- Step 2: ECOP reads (device objects 0x2000) ---");

    eprintln!("  Reading 0x2000 sub 4 (PCB number)...");
    match protocol::ecop_read(&mut conn, 0x2000, 4).await {
        Ok(data) => {
            let s = String::from_utf8_lossy(&data);
            eprintln!("    Data ({} bytes): {:?}", data.len(), s.trim_end_matches('\0'));
            eprintln!("    Hex: [{}]", protocol::hex_dump(&data));
        }
        Err(e) => eprintln!("    Error: {e}"),
    }

    eprintln!("  Reading 0x2000 sub 8 (warranty)...");
    match protocol::ecop_read(&mut conn, 0x2000, 8).await {
        Ok(data) => eprintln!("    Data ({} bytes): [{}]", data.len(), protocol::hex_dump(&data)),
        Err(e) => eprintln!("    Error: {e}"),
    }

    eprintln!("  Reading 0x2008 sub 1...");
    match protocol::ecop_read(&mut conn, 0x2008, 1).await {
        Ok(data) => eprintln!("    Data ({} bytes): [{}]", data.len(), protocol::hex_dump(&data)),
        Err(e) => eprintln!("    Error: {e}"),
    }

    eprintln!("  Reading 0x2006 sub 12 (dive mode name)...");
    match protocol::ecop_read(&mut conn, 0x2006, 12).await {
        Ok(data) => {
            let s = String::from_utf8_lossy(&data);
            eprintln!("    Data: {:?}", s.trim_end_matches('\0'));
        }
        Err(e) => eprintln!("    Error: {e}"),
    }

    // Step 3: Set datetime
    eprintln!("\n--- Step 3: Set datetime (B0) ---");
    match protocol::set_datetime(&mut conn).await {
        Ok(()) => eprintln!("  DateTime set OK"),
        Err(e) => eprintln!("  DateTime failed: {e}"),
    }

    // Step 4: Count dives
    eprintln!("\n--- Step 4: Count dive objects ---");
    match protocol::count_dives(&mut conn).await {
        Ok(count) => eprintln!("  Found {count} dive object(s)"),
        Err(e) => eprintln!("  Count failed: {e}"),
    }

    // Step 5: Read first dive header
    eprintln!("\n--- Step 5: Read first dive header (0x3000 sub 4) ---");
    match protocol::read_dive_header(&mut conn, 0).await {
        Ok(data) => {
            eprintln!("  Header ({} bytes)", data.len());
            if data.len() >= 4 {
                let obj_type = data[0];
                eprintln!("  Object type: {} ({})", obj_type, match obj_type {
                    1 => "SCUBA",
                    2 => "FREEDIVE",
                    3 => "GAUGE",
                    _ => "unknown",
                });
            }
            if data.len() >= 12 {
                // Bytes 4-7 should be a timestamp
                let ts = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                if let Some(dt) = chrono::DateTime::from_timestamp(ts as i64, 0) {
                    eprintln!("  Timestamp: {} ({})", ts, dt.format("%Y-%m-%d %H:%M:%S UTC"));
                } else {
                    eprintln!("  Timestamp raw: {}", ts);
                }
            }
            // Print first 40 bytes hex
            let show = data.len().min(40);
            eprintln!("  First {} bytes: [{}]", show, protocol::hex_dump(&data[..show]));
        }
        Err(e) => eprintln!("  Error: {e}"),
    }

    // Step 6: Read first dive profile (if available)
    eprintln!("\n--- Step 6: Read first dive profile (0x3000 sub 3) ---");
    match protocol::read_dive_profile(&mut conn, 0).await {
        Ok(data) => {
            eprintln!("  Profile ({} bytes)", data.len());
            let show = data.len().min(60);
            eprintln!("  First {} bytes: [{}]", show, protocol::hex_dump(&data[..show]));

            // Look for record markers
            let markers = ["DSTR", "TISS", "DPRS", "AIRS"];
            for marker in &markers {
                let count = data
                    .windows(4)
                    .filter(|w| *w == marker.as_bytes())
                    .count();
                if count > 0 {
                    eprintln!("  {} records: {}", marker, count);
                }
            }
        }
        Err(e) => eprintln!("  Error: {e}"),
    }

    conn.disconnect().await?;
    eprintln!("\nDone.");
    Ok(())
}

// ── Info ──

async fn cmd_info(address: Option<String>) -> Result<()> {
    let adapter = ble::get_adapter().await?;
    let peripheral = find_device(&adapter, address.as_deref()).await?;
    let mut conn = ble::connect(&peripheral, None, None).await?;

    let info = protocol::get_device_info(&mut conn).await?;

    // Read PCB number via ECOP
    let pcb = match protocol::read_pcb_number(&mut conn).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Warning: could not read PCB number: {e}");
            String::from("unknown")
        }
    };

    // Count dives
    let dive_count = match protocol::count_dives(&mut conn).await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Warning: could not count dives: {e}");
            0
        }
    };

    println!("Device Info:");
    println!("  Model:      {} (0x{:02X})", info.model_name, info.model as u8);
    println!("  PCB Number: {}", pcb);
    println!("  Dives:      {}", dive_count);

    conn.disconnect().await?;
    Ok(())
}

// ── Download ──

async fn cmd_download(
    address: Option<String>,
    output: PathBuf,
    format: OutputFormat,
    save_raw: Option<PathBuf>,
) -> Result<()> {
    // Load existing dives from output file (if any) for incremental download
    let mut existing_dives: Vec<DiveLog> = Vec::new();
    let mut existing_numbers: HashSet<u32> = HashSet::new();

    if matches!(format, OutputFormat::Json) && output.exists() {
        match std::fs::read_to_string(&output) {
            Ok(contents) => match serde_json::from_str::<DiveData>(&contents) {
                Ok(data) => {
                    // Dives from a logbook have no number of their own
                    for dive in data.dives.iter().filter(|dive| !dive.is_logbook_only()) {
                        existing_numbers.insert(dive.number);
                    }
                    eprintln!(
                        "Loaded {} existing dive(s) from {}",
                        data.dives.len(),
                        output.display()
                    );
                    existing_dives = data.dives;
                }
                Err(e) => {
                    eprintln!("Warning: could not parse {}: {e}", output.display());
                }
            },
            Err(e) => {
                eprintln!("Warning: could not read {}: {e}", output.display());
            }
        }
    }

    let adapter = ble::get_adapter().await?;
    let peripheral = find_device(&adapter, address.as_deref()).await?;
    let mut conn = ble::connect(&peripheral, None, None).await?;

    let info = protocol::get_device_info(&mut conn).await?;
    eprintln!("Connected to {}", info.model_name);

    // Set datetime
    if let Err(e) = protocol::set_datetime(&mut conn).await {
        eprintln!("Warning: could not set datetime: {e}");
    }

    // Count dives
    let dive_count = protocol::count_dives(&mut conn).await?;
    eprintln!("Found {} dive(s)", dive_count);

    if dive_count == 0 {
        eprintln!("No dives on device.");
        conn.disconnect().await?;
        return Ok(());
    }

    // Download dive headers + profiles, skipping already-downloaded dives
    let mut new_dives = Vec::new();
    let mut skipped = 0u32;

    // An error part-way through must not lose the dives fetched so far
    let result: Result<()> = async {
        for i in 0..dive_count {
            eprint!("\rChecking dive {}/{}...", i + 1, dive_count);

            let header = protocol::read_dive_header(&mut conn, i).await?;

            // Check if we already have this dive
            let dive_number = parser::dive_number_from_header(&header);
            if existing_numbers.contains(&dive_number) {
                eprintln!("\r  Dive #{}: already downloaded, skipping", dive_number);
                skipped += 1;
                continue;
            }

            eprint!("\rDownloading dive {}/{}...", i + 1, dive_count);
            let profile = if parser::is_freedive_header(&header) {
                protocol::read_freedive_data(&mut conn, i).await
            } else {
                protocol::read_dive_profile(&mut conn, i).await
            };
            let profile = match profile {
                Ok(profile) => profile,
                // The watch lists the dive but refuses to hand over its profile
                Err(e) if e.is::<protocol::SdoAbort>() => {
                    eprintln!("\r  Dive #{dive_number}: no profile available, skipping ({e})");
                    continue;
                }
                Err(e) => return Err(e),
            };

            if let Some(ref raw_dir) = save_raw {
                std::fs::create_dir_all(raw_dir)?;
                std::fs::write(raw_dir.join(format!("dive_{i:03}_header.bin")), &header)?;
                std::fs::write(raw_dir.join(format!("dive_{i:03}_profile.bin")), &profile)?;
            }

            match parser::parse_dive_ecop(i as u32, &header, &profile) {
                Ok(dive) => {
                    eprintln!(
                        "\r  Dive #{}: {} | {:.1}m | {}s | {}",
                        dive.number,
                        dive.datetime.format("%Y-%m-%d %H:%M"),
                        dive.max_depth_m,
                        dive.duration_seconds,
                        if dive.dips.is_empty() {
                            format!("{} samples", dive.samples.len())
                        } else {
                            format!("{} dips", dive.dips.len())
                        },
                    );
                    new_dives.push(dive);
                }
                Err(e) => {
                    eprintln!("\r  Dive {i}: parse error: {e}");
                }
            }
        }
        Ok(())
    }
    .await;
    eprintln!();

    match &result {
        Ok(()) => conn.disconnect().await?,
        Err(e) => {
            eprintln!("Download interrupted: {e:#}");
            conn.disconnect().await.ok();
        }
    }

    if skipped > 0 {
        eprintln!("Skipped {} already-downloaded dive(s)", skipped);
    }
    if !new_dives.is_empty() {
        eprintln!("Downloaded {} new dive(s)", new_dives.len());
    }

    // Merge existing + new dives
    let mut all_dives = existing_dives;
    all_dives.append(&mut new_dives);
    all_dives.sort_by_key(|d| (d.datetime, d.number));

    if all_dives.is_empty() {
        eprintln!("No dives could be parsed.");
        return result;
    }

    // Export
    match format {
        OutputFormat::Json => {
            let data = DiveData { dives: all_dives };
            let json = serde_json::to_string_pretty(&data)?;
            std::fs::write(&output, &json)?;
            eprintln!("Dive data saved to {} ({} dives)", output.display(), data.dives.len());
        }
        OutputFormat::Csv => {
            for dive in &all_dives {
                let stem = output
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy();
                let dir = output.parent().unwrap_or(std::path::Path::new("."));
                let csv_path = dir.join(format!("{}_{:03}.csv", stem, dive.number));
                let csv = parser::dive_to_csv(dive);
                std::fs::write(&csv_path, &csv)?;
                eprintln!("  Dive #{} -> {}", dive.number, csv_path.display());
            }
        }
    }

    result
}

// ── Parse (offline) ──

fn cmd_parse(raw_dir: PathBuf, output: PathBuf, format: OutputFormat) -> Result<()> {
    // Count available dives
    let mut dive_count = 0u16;
    while raw_dir.join(format!("dive_{:03}_header.bin", dive_count)).exists() {
        dive_count += 1;
    }

    if dive_count == 0 {
        anyhow::bail!("No dive files found in {}", raw_dir.display());
    }

    eprintln!("Found {} raw dive file(s) in {}", dive_count, raw_dir.display());

    let mut dives = Vec::new();
    for i in 0..dive_count {
        let header = std::fs::read(raw_dir.join(format!("dive_{i:03}_header.bin")))?;
        let profile = std::fs::read(raw_dir.join(format!("dive_{i:03}_profile.bin")))?;

        match parser::parse_dive_ecop(i as u32, &header, &profile) {
            Ok(dive) => {
                eprintln!(
                    "  Dive #{}: {} | {:.1}m | {}min | {} | {:?}",
                    dive.number,
                    dive.datetime.format("%Y-%m-%d %H:%M"),
                    dive.max_depth_m,
                    dive.duration_seconds / 60,
                    if dive.dips.is_empty() {
                        format!("{} samples", dive.samples.len())
                    } else {
                        format!("{} dips", dive.dips.len())
                    },
                    dive.dive_mode,
                );
                dives.push(dive);
            }
            Err(e) => {
                eprintln!("  Dive {i}: parse error: {e}");
            }
        }
    }

    if dives.is_empty() {
        eprintln!("No dives could be parsed.");
        return Ok(());
    }

    eprintln!("Parsed {} dive(s)", dives.len());

    match format {
        OutputFormat::Json => {
            let data = DiveData { dives };
            let json = serde_json::to_string_pretty(&data)?;
            std::fs::write(&output, &json)?;
            eprintln!("Dive data saved to {}", output.display());
        }
        OutputFormat::Csv => {
            for dive in &dives {
                let stem = output
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy();
                let dir = output.parent().unwrap_or(std::path::Path::new("."));
                let csv_path = dir.join(format!("{}_{:03}.csv", stem, dive.number));
                let csv = parser::dive_to_csv(dive);
                std::fs::write(&csv_path, &csv)?;
                eprintln!("  Dive #{} -> {}", dive.number, csv_path.display());
            }
        }
    }

    Ok(())
}

// ── Correlate ──

struct SsiRecord {
    datetime: chrono::NaiveDateTime,
    site: String,
    country: String,
    buddy: String,
    freedive: bool,
    duration_s: u32,
    max_depth_m: f64,
}

/// The number a field starts with, as in "45 min" or "18.2 m (60 ft)".
fn leading_number(field: &str) -> Option<f64> {
    let end = field
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(field.len());
    field[..end].parse().ok()
}

/// Parse a CSV line handling quoted fields with escaped quotes.
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    current.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                current.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
        } else if c == ',' {
            fields.push(current.clone());
            current.clear();
        } else {
            current.push(c);
        }
    }
    fields.push(current);
    fields
}

/// What the SSI export lists among the buddies for the dive computer.
const SSI_DIVE_COMPUTERS: [&str; 3] = ["Sirius", "Mares Sirius", "Mares"];

/// The blocks of the SSI buddy column: the buddies, the dive center, the
/// dive computer. Runs of blanks keep them apart; the computer is left out.
fn buddy_blocks(raw: &str) -> Vec<String> {
    raw.replace('\t', "  ")
        .split("  ")
        .map(|block| block.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|block| !block.is_empty() && !SSI_DIVE_COMPUTERS.contains(&block.as_str()))
        .collect()
}

/// Find the blocks that hold several names. The export glues the buddies of
/// a dive to one another, as in "Serge MASAxel Brosse", but each of them
/// shows on its own, or next to others, elsewhere in the logbook: a block
/// that starts or ends with another one is that name and more names.
///
/// Returns the two parts of every block that splits, themselves blocks to
/// look up in turn.
fn glued_names(blocks: &BTreeSet<String>) -> HashMap<String, (String, String)> {
    let mut names = blocks.clone();
    let mut parts = HashMap::new();
    loop {
        // The rest of a block is a name when it is known as one, or has the
        // two words of a name; "Serge MASSON" is not "Serge MAS" and "SON"
        let name = |rest: &str| names.contains(rest) || rest.contains(' ');
        let split = names.iter().find_map(|block| {
            names.iter().find_map(|other| {
                // Glued names have no blank where they meet: with one, the
                // block is a longer name of its own ("Dive Systems Malta")
                let ahead = block.strip_prefix(other.as_str());
                let behind = block.strip_suffix(other.as_str());
                if let Some(rest) = ahead.filter(|rest| !rest.starts_with(' ') && name(rest)) {
                    Some((block.clone(), other.clone(), rest.to_string()))
                } else {
                    let rest = behind.filter(|rest| !rest.ends_with(' ') && name(rest))?;
                    Some((block.clone(), rest.to_string(), other.clone()))
                }
            })
        });
        let Some((block, first, second)) = split else {
            return parts;
        };
        names.remove(&block);
        names.insert(first.clone());
        names.insert(second.clone());
        parts.insert(block, (first, second));
    }
}

/// The names of a block, in order, given the blocks that split.
fn names_of(block: &str, parts: &HashMap<String, (String, String)>, names: &mut Vec<String>) {
    match parts.get(block) {
        Some((first, second)) => {
            names_of(first, parts, names);
            names_of(second, parts, names);
        }
        None => names.push(block.to_string()),
    }
}

/// Parse SSI CSV export into records.
fn parse_ssi_csv(contents: &str) -> Vec<SsiRecord> {
    let mut lines = contents.lines();

    // Parse header to find column indices
    let header_line = match lines.next() {
        Some(h) => h,
        None => return Vec::new(),
    };
    let headers = parse_csv_line(header_line);

    let col = |name: &str| headers.iter().position(|h| h == name);

    // The export has named the date column both ways
    let date_col = col("Date / Temps")
        .or_else(|| col("Date / Heure"))
        .unwrap_or(3);
    let site_col = col("Site de plongée").unwrap_or(1);
    let country_col = col("Pays").unwrap_or(2);
    let buddy_col = col("Equipier / Instructor / Center").unwrap_or(9);
    let activity_col = col("Type d'activité de plongée").unwrap_or(4);
    let duration_col = col("Durée").unwrap_or(7);
    let depth_col = col("Profondeur").unwrap_or(8);

    let mut records = Vec::new();
    // The blocks of the buddy column, record by record
    let mut blocks = Vec::new();
    for (line_num, line) in lines.enumerate() {
        let fields = parse_csv_line(line);
        let max_col = *[date_col, site_col, country_col, buddy_col]
            .iter()
            .max()
            .unwrap();
        if fields.len() <= max_col {
            eprintln!(
                "Warning: skipping CSV line {} (not enough fields)",
                line_num + 2
            );
            continue;
        }

        let datetime = match chrono::NaiveDateTime::parse_from_str(
            fields[date_col].trim(),
            "%d. %b %Y %H:%M",
        ) {
            Ok(dt) => dt,
            Err(e) => {
                eprintln!(
                    "Warning: skipping CSV line {} (bad date {:?}: {})",
                    line_num + 2,
                    fields[date_col],
                    e
                );
                continue;
            }
        };

        // What only an imported dive needs; a freedive has no duration
        let field = |col: usize| fields.get(col).map_or("", |field| field.trim());
        let minutes = leading_number(field(duration_col)).unwrap_or(0.0);

        blocks.push(buddy_blocks(&fields[buddy_col]));
        records.push(SsiRecord {
            datetime,
            site: fields[site_col].trim().to_string(),
            country: fields[country_col].trim().to_string(),
            buddy: String::new(),
            freedive: field(activity_col).contains("Apnée"),
            duration_s: (minutes * 60.0) as u32,
            max_depth_m: leading_number(field(depth_col)).unwrap_or(0.0),
        });
    }

    // Buddies and dive center, one name after the other. Which blocks hold
    // several names only shows with the whole logbook at hand.
    let parts = glued_names(&blocks.iter().flatten().cloned().collect());
    for (record, blocks) in records.iter_mut().zip(&blocks) {
        let mut names = Vec::new();
        for block in blocks {
            names_of(block, &parts, &mut names);
        }
        record.buddy = names.join(", ");
    }

    records
}

fn cmd_correlate(csv_path: PathBuf, json_path: PathBuf) -> Result<()> {
    use chrono::{Datelike, Timelike};

    // Load dives.json
    let json_contents = std::fs::read_to_string(&json_path)
        .with_context(|| format!("Failed to read {}", json_path.display()))?;
    let mut data: DiveData = serde_json::from_str(&json_contents)
        .with_context(|| format!("Failed to parse {}", json_path.display()))?;

    // Parse SSI CSV
    let csv_contents = std::fs::read_to_string(&csv_path)
        .with_context(|| format!("Failed to read {}", csv_path.display()))?;
    let ssi_records = parse_ssi_csv(&csv_contents);
    eprintln!("Parsed {} SSI record(s) from {}", ssi_records.len(), csv_path.display());

    // Build lookup by (year, month, day, hour, minute)
    let lookup: HashMap<(i32, u32, u32, u32, u32), &SsiRecord> = ssi_records
        .iter()
        .map(|r| {
            let key = (
                r.datetime.date().year(),
                r.datetime.date().month(),
                r.datetime.date().day(),
                r.datetime.time().hour(),
                r.datetime.time().minute(),
            );
            (key, r)
        })
        .collect();

    let mut matched = 0u32;
    let mut unmatched = 0u32;

    // Dives ignored in the viewer are not looked up
    let ignored = data.dives.iter().filter(|dive| dive.ignored).count();
    for dive in data.dives.iter_mut().filter(|dive| !dive.ignored) {
        let key = (
            dive.datetime.date().year(),
            dive.datetime.date().month(),
            dive.datetime.date().day(),
            dive.datetime.time().hour(),
            dive.datetime.time().minute(),
        );

        if let Some(ssi) = lookup.get(&key) {
            if !ssi.site.is_empty() {
                dive.site = Some(ssi.site.clone());
            }
            if !ssi.country.is_empty() {
                dive.country = Some(ssi.country.clone());
            }
            if !ssi.buddy.is_empty() {
                dive.buddy = Some(ssi.buddy.clone());
            } else if dive
                .buddy
                .as_deref()
                .is_some_and(|buddy| SSI_DIVE_COMPUTERS.contains(&buddy))
            {
                // An earlier version took the dive computer for a buddy
                dive.buddy = None;
            }
            matched += 1;
        } else {
            unmatched += 1;
        }
    }

    eprintln!("Matched: {}, Unmatched: {}", matched, unmatched);
    if ignored > 0 {
        eprintln!("Left out {ignored} ignored dive(s)");
    }

    // An SSI entry with no dive at its minute is a dive no dive computer
    // handed over: import it, with what the logbook says about it
    let mut known: HashSet<chrono::NaiveDateTime> =
        data.dives.iter().map(|dive| dive.datetime).collect();
    let mut imported = Vec::new();
    for record in &ssi_records {
        if !known.insert(record.datetime) {
            continue;
        }
        // A dive under way at that time is the same dive, logged with
        // another start: importing it would list it twice
        let seconds = |seconds: u32| chrono::Duration::seconds(seconds.max(60) as i64);
        let under_way = data.dives.iter().find(|dive| {
            !dive.is_logbook_only()
                && dive.datetime < record.datetime + seconds(record.duration_s)
                && record.datetime < dive.datetime + seconds(dive.duration_seconds)
        });
        if let Some(dive) = under_way {
            eprintln!(
                "  Not imported: the SSI entry of {} overlaps the dive of {}",
                record.datetime.format("%Y-%m-%d %H:%M"),
                dive.datetime.format("%H:%M")
            );
            continue;
        }

        let filled = |text: &String| (!text.is_empty()).then(|| text.clone());
        imported.push(DiveLog {
            datetime: record.datetime,
            duration_seconds: record.duration_s,
            max_depth_m: record.max_depth_m,
            dive_mode: if record.freedive {
                DiveMode::Freedive
            } else {
                DiveMode::Air
            },
            site: filled(&record.site),
            country: filled(&record.country),
            buddy: filled(&record.buddy),
            ..Default::default()
        });
    }
    if !imported.is_empty() {
        eprintln!("Imported {} dive(s) from the SSI logbook", imported.len());
        data.dives.append(&mut imported);
        data.dives.sort_by_key(|dive| (dive.datetime, dive.number));
    }

    // Write back
    let json = serde_json::to_string_pretty(&data)?;
    std::fs::write(&json_path, &json)?;
    eprintln!("Updated {}", json_path.display());

    Ok(())
}

// ── Watermark ──

struct VideoMeta {
    capture_time: chrono::NaiveDateTime,
    width: u32,
    height: u32,
    duration_secs: f64,
}

fn probe_video(path: &std::path::Path, start: Option<chrono::NaiveDateTime>) -> Result<VideoMeta> {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .context("Failed to run ffprobe. Is ffmpeg installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ffprobe failed: {stderr}");
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("Failed to parse ffprobe JSON output")?;

    // Extract capture time from video metadata (try multiple tag formats),
    // unless the caller knows better
    let tags = &json["format"]["tags"];
    let capture_time = if let Some(start) = start {
        start
    } else if let Some(comment) = tags["comment"]
        .as_str()
        .or_else(|| tags["Comment"].as_str())
    {
        // Insta360 X3: "2025-08-01 10:04:49 +0000"
        chrono::DateTime::parse_from_str(comment.trim(), "%Y-%m-%d %H:%M:%S %z")
            .with_context(|| format!("Failed to parse comment timestamp: {comment:?}"))?
            .naive_utc()
    } else if let Some(creation_time) = tags["creation_time"]
        .as_str()
        .or_else(|| tags["Creation_time"].as_str())
    {
        // GoPro / many other cameras: "2024-11-17T10:12:57.000000Z"
        chrono::NaiveDateTime::parse_from_str(creation_time.trim(), "%Y-%m-%dT%H:%M:%S%.fZ")
            .with_context(|| {
                format!("Failed to parse creation_time timestamp: {creation_time:?}")
            })?
    } else {
        anyhow::bail!(
            "No capture time found in video metadata. \
             Expected 'comment' (Insta360) or 'creation_time' (GoPro) tag. \
             Give the start time with --start instead."
        );
    };

    // Find video stream for resolution and duration
    let streams = json["streams"].as_array().context("No streams in ffprobe output")?;
    let video_stream = streams
        .iter()
        .find(|s| s["codec_type"].as_str() == Some("video"))
        .context("No video stream found")?;

    let width = video_stream["width"]
        .as_u64()
        .context("No width in video stream")? as u32;
    let height = video_stream["height"]
        .as_u64()
        .context("No height in video stream")? as u32;

    let duration_secs = video_stream["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            json["format"]["duration"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
        })
        .context("No duration found in video metadata")?;

    Ok(VideoMeta {
        capture_time,
        width,
        height,
        duration_secs,
    })
}

fn find_overlapping_dive(
    dives: &[DiveLog],
    video_start: chrono::NaiveDateTime,
    video_duration: f64,
    offset: i64,
) -> Result<&DiveLog> {
    let video_end = video_start + chrono::Duration::milliseconds((video_duration * 1000.0) as i64);

    // What the viewer calls the dive: its number in the logbook, if it has one
    let numbers = logbook_numbers(dives);
    let name = |dive: usize| numbers[dive].map_or("ignored dive".to_string(), |n| format!("#{n}"));

    let mut best: Option<(usize, i64)> = None;

    // A dive from a logbook has no profile to overlay
    for (index, dive) in dives.iter().enumerate() {
        if dive.is_logbook_only() {
            continue;
        }
        let dive_start = dive.datetime + chrono::Duration::seconds(offset);
        let dive_end = dive_start + chrono::Duration::seconds(dive.duration_seconds as i64);

        let overlap_start = video_start.max(dive_start);
        let overlap_end = video_end.min(dive_end);
        let overlap = (overlap_end - overlap_start).num_seconds();

        if overlap > 0 && (best.is_none() || overlap > best.unwrap().1) {
            best = Some((index, overlap));
        }
    }

    match best {
        Some((index, overlap)) => {
            let dive = &dives[index];
            eprintln!(
                "Matched dive {} ({}) — {:.0}s overlap",
                name(index),
                dive.datetime.format("%Y-%m-%d %H:%M"),
                overlap
            );
            Ok(dive)
        }
        None => {
            let video_date = video_start.date();
            eprintln!("Video time range: {} to {}", video_start, video_end);
            let same_day: Vec<_> = dives
                .iter()
                .enumerate()
                .filter(|(_, d)| d.datetime.date() == video_date && !d.is_logbook_only())
                .collect();
            if same_day.is_empty() {
                eprintln!("No dives found on {video_date}.");
            } else {
                eprintln!("Dives on {video_date}:");
                for &(index, dive) in &same_day {
                    let dive_end =
                        dive.datetime + chrono::Duration::seconds(dive.duration_seconds as i64);
                    eprintln!(
                        "  {}: {} to {}",
                        name(index),
                        dive.datetime.format("%H:%M:%S"),
                        dive_end.format("%H:%M:%S")
                    );
                }
            }
            anyhow::bail!(
                "No dive overlaps with the video time range. \
                 Use --offset to adjust (e.g. --offset 30 shifts dive start forward by 30s)."
            )
        }
    }
}

/// Escape a string for use in ffmpeg drawtext filter.
fn escape_drawtext(s: &str) -> String {
    s.replace('\\', r"\\")
        .replace(':', r"\:")
        .replace('\'', r"'\''")
}

fn build_drawtext_filter(
    dive: &DiveLog,
    video_start: chrono::NaiveDateTime,
    video_duration: f64,
    offset: i64,
    video_height: u32,
) -> String {
    let dive_start = dive.datetime + chrono::Duration::seconds(offset);
    let dive_start_offset = (video_start - dive_start).num_seconds();

    // Scale overlay relative to 1080p baseline
    let scale = video_height as f64 / 1080.0;
    let fontsize = (48.0 * scale).round() as u32;
    let borderw = (2.0 * scale).round().max(1.0) as u32;
    let shadow = (2.0 * scale).round().max(1.0) as u32;
    let margin = (20.0 * scale).round() as u32;

    let mut filters = Vec::new();

    for (i, sample) in dive.samples.iter().enumerate() {
        let sample_video_t = sample.time_s as f64 - dive_start_offset as f64;
        let next_video_t = if i + 1 < dive.samples.len() {
            dive.samples[i + 1].time_s as f64 - dive_start_offset as f64
        } else {
            video_duration
        };

        // Skip samples entirely outside the video
        if next_video_t <= 0.0 || sample_video_t >= video_duration {
            continue;
        }

        // Clamp to video boundaries
        let start_t = sample_video_t.max(0.0);
        let end_t = next_video_t.min(video_duration);

        // Format text
        let mut text = format!("-{:.1}m", sample.depth_m);
        if let Some(temp) = sample.temp_c {
            text.push_str(&format!("  {temp:.1}°C"));
        }
        if let Some(pressure) = sample.pressure_bar {
            text.push_str(&format!("  {pressure:.0}bar"));
        }

        let escaped = escape_drawtext(&text);

        filters.push(format!(
            "drawtext=text='{escaped}'\
            :fontcolor=white:fontsize={fontsize}\
            :borderw={borderw}:bordercolor=black\
            :shadowcolor=black@0.5:shadowx={shadow}:shadowy={shadow}\
            :x=W-tw-{margin}:y=H-th-{margin}\
            :enable='between(t,{start_t:.3},{end_t:.3})'"
        ));
    }

    if filters.is_empty() {
        eprintln!("Warning: no dive samples fall within the video time range. Output will have no overlay.");
        return String::new();
    }

    filters.join(",")
}

fn cmd_watermark(
    video: PathBuf,
    json: PathBuf,
    offset: i64,
    start: Option<chrono::NaiveDateTime>,
) -> Result<()> {
    // Load dives
    let json_contents = std::fs::read_to_string(&json)
        .with_context(|| format!("Failed to read {}", json.display()))?;
    let data: DiveData = serde_json::from_str(&json_contents)
        .with_context(|| format!("Failed to parse {}", json.display()))?;

    if data.dives.is_empty() {
        anyhow::bail!("No dives found in {}", json.display());
    }

    // Probe video
    eprintln!("Probing video: {}", video.display());
    let meta = probe_video(&video, start)?;
    if start.is_some() {
        eprintln!(
            "  Start time: {} (from --start)",
            meta.capture_time.format("%Y-%m-%d %H:%M:%S")
        );
    } else {
        eprintln!(
            "  Capture time: {} UTC",
            meta.capture_time.format("%Y-%m-%d %H:%M:%S")
        );
    }
    eprintln!("  Resolution: {}x{}", meta.width, meta.height);
    eprintln!("  Duration: {:.1}s", meta.duration_secs);

    if offset != 0 {
        eprintln!("  Time offset: +{offset}s applied to dive time");
    }

    // Find matching dive
    let dive = find_overlapping_dive(&data.dives, meta.capture_time, meta.duration_secs, offset)?;

    // Build filter
    let filter = build_drawtext_filter(dive, meta.capture_time, meta.duration_secs, offset, meta.height);

    // Build output path: YYYY-MM-DD_HHhMM_Site_Name.ext
    let ext = video.extension().unwrap_or_default().to_string_lossy();
    let dt_str = meta.capture_time.format("%Y-%m-%d_%Hh%M").to_string();
    let output_name = match &dive.site {
        Some(site) if !site.is_empty() => {
            let safe_site = site.replace(' ', "_");
            format!("{dt_str}_{safe_site}.{ext}")
        }
        _ => format!("{dt_str}_dive.{ext}"),
    };
    let output_path = video
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(output_name);

    // The output is named like hand-named clips: never write over the input
    if std::fs::canonicalize(&output_path).ok() == std::fs::canonicalize(&video).ok() {
        anyhow::bail!(
            "The output would overwrite the input ({}). Rename or move the input first.",
            output_path.display()
        );
    }

    if filter.is_empty() {
        eprintln!("No overlay samples — copying video without modification.");
        std::fs::copy(&video, &output_path)?;
        eprintln!("Output: {}", output_path.display());
        return Ok(());
    }

    eprintln!(
        "Rendering overlay ({} drawtext filters, {:.1}KB filter string)...",
        filter.matches("drawtext=").count(),
        filter.len() as f64 / 1024.0
    );

    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-i"]).arg(&video);

    // Use filter_script if the filter string is very large (>100KB)
    let _tempfile;
    if filter.len() > 100 * 1024 {
        let tmp = std::env::temp_dir().join("sirius_dive_filter.txt");
        std::fs::write(&tmp, &filter)?;
        cmd.args(["-filter_script:v"]).arg(&tmp);
        _tempfile = Some(tmp);
    } else {
        cmd.args(["-vf", &filter]);
    }

    cmd.args(["-c:v", "libx264", "-preset", "medium", "-crf", "18", "-c:a", "copy",
              "-map_metadata", "0", "-movflags", "+use_metadata_tags", "-y"])
        .arg(&output_path);

    eprintln!("Running ffmpeg...");
    let status = cmd
        .status()
        .context("Failed to run ffmpeg. Is ffmpeg installed?")?;

    if !status.success() {
        anyhow::bail!("ffmpeg exited with status {status}");
    }

    eprintln!("Output: {}", output_path.display());
    Ok(())
}

// ── FIT export ──

/// UTC offset of this machine's time zone at a local time.
fn local_utc_offset(datetime: chrono::NaiveDateTime) -> chrono::FixedOffset {
    use chrono::TimeZone;

    chrono::Local
        .offset_from_local_datetime(&datetime)
        .earliest()
        // A time skipped by a clock change has no offset of its own
        .unwrap_or_else(|| chrono::Local.offset_from_utc_datetime(&datetime))
}

fn cmd_fit(
    json: PathBuf,
    output: PathBuf,
    date: Option<chrono::NaiveDate>,
    number: Option<u32>,
    utc_offset: Option<chrono::FixedOffset>,
    offset: i64,
    interval: u32,
) -> Result<()> {
    let contents = std::fs::read_to_string(&json)
        .with_context(|| format!("Failed to read {}", json.display()))?;
    let data: DiveData = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse {}", json.display()))?;

    // Dives go by their number in the logbook. Those ignored in the viewer
    // have none and are not exported; those that come from a logbook only
    // have no profile to export.
    let numbers = logbook_numbers(&data.dives);
    let (ignored, dives): (Vec<_>, Vec<_>) = data
        .dives
        .iter()
        .zip(&numbers)
        .filter(|(dive, _)| !dive.is_logbook_only())
        .filter(|(dive, _)| date.is_none_or(|date| dive.datetime.date() == date))
        .filter(|(_, n)| number.is_none_or(|number| **n == Some(number)))
        .partition(|(dive, _)| dive.ignored);
    let dives = dives
        .into_iter()
        .filter_map(|(dive, n)| Some((dive, (*n)?)));
    let dives: Vec<(&DiveLog, u32)> = dives.collect();
    if !ignored.is_empty() {
        eprintln!("Left out {} ignored dive(s)", ignored.len());
    }
    if dives.is_empty() {
        anyhow::bail!("No matching dive in {}", json.display());
    }

    std::fs::create_dir_all(&output)?;
    let mut exported = 0;
    for (dive, number) in dives {
        let zone = utc_offset.unwrap_or_else(|| local_utc_offset(dive.datetime));
        let start = dive.datetime - zone + chrono::Duration::seconds(offset);

        let file = match fit::encode_dive(dive, number, start, zone.local_minus_utc(), interval) {
            Ok(file) => file,
            Err(e) => {
                eprintln!("  Dive #{number}: {e}, skipping");
                continue;
            }
        };
        let path = output.join(format!(
            "{}_dive_{:03}.fit",
            dive.datetime.format("%Y-%m-%d_%Hh%M"),
            number
        ));
        std::fs::write(&path, file)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        eprintln!(
            "  Dive #{number}: {} UTC{zone} -> {}",
            dive.datetime.format("%Y-%m-%d %H:%M"),
            path.display()
        );
        exported += 1;
    }
    eprintln!("Exported {exported} dive(s) to {}", output.display());

    Ok(())
}

// ── Helpers ──

/// Find a Mares device, either by address or by scanning.
async fn find_device(
    adapter: &btleplug::platform::Adapter,
    address: Option<&str>,
) -> Result<btleplug::platform::Peripheral> {
    eprintln!("Scanning for Mares devices...");
    let devices = ble::scan_for_devices(adapter, Duration::from_secs(10)).await?;

    if devices.is_empty() {
        anyhow::bail!("No Mares devices found. Make sure the dive computer is in Bluetooth mode.");
    }

    let dev = if let Some(addr) = address {
        let addr_upper = addr.to_uppercase();
        devices
            .into_iter()
            .find(|d| d.address.to_uppercase() == addr_upper)
            .with_context(|| format!("Device with address {addr} not found"))?
    } else {
        eprintln!("Connecting to first device: {}", devices[0].name);
        devices.into_iter().next().unwrap()
    };

    Ok(dev.peripheral)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own for the files of a test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sirius-dive-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Two dives of the same morning, the first one ignored in the viewer.
    fn write_dives(path: &std::path::Path) {
        let dive = |number: u32, hour: u32, ignored: bool| DiveLog {
            number,
            datetime: chrono::NaiveDate::from_ymd_opt(2025, 10, 26)
                .unwrap()
                .and_hms_opt(hour, 21, 0)
                .unwrap(),
            duration_seconds: 2700,
            samples: vec![Sample {
                depth_m: 5.0,
                ..Default::default()
            }],
            ignored,
            ..Default::default()
        };
        let data = DiveData {
            dives: vec![dive(47, 10, true), dive(48, 12, false)],
        };
        std::fs::write(path, serde_json::to_string_pretty(&data).unwrap()).unwrap();
    }

    fn read_dives(path: &std::path::Path) -> Vec<DiveLog> {
        let data: DiveData = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        data.dives
    }

    #[test]
    fn correlate_leaves_ignored_dives_alone() {
        let dir = scratch("correlate");
        let (json, ssi) = (dir.join("dives.json"), dir.join("ssi.csv"));
        write_dives(&json);
        std::fs::write(
            &ssi,
            concat!(
                "\"plongée #\",\"Site de plongée\",\"Pays\",\"Date / Temps\",\"a\",\"b\",\"c\",\"d\",\"e\",",
                "\"Equipier / Instructor / Center\"\n",
                "\"99\",\"West coast\",\"Croatie\",\"26. Oct 2025 10:21\",\"\",\"\",\"\",\"\",\"\",\"Venus\"\n",
                "\"100\",\"Red Rocks\",\"Croatie\",\"26. Oct 2025 12:21\",\"\",\"\",\"\",\"\",\"\",\"Venus\"\n",
            ),
        )
        .unwrap();

        cmd_correlate(ssi, json.clone()).unwrap();

        let dives = read_dives(&json);
        assert_eq!(
            (dives[0].number, &dives[0].site, dives[0].ignored),
            (47, &None, true)
        );
        assert_eq!(dives[1].site.as_deref(), Some("Red Rocks"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fit_leaves_ignored_dives_out() {
        let dir = scratch("fit");
        let (json, output) = (dir.join("dives.json"), dir.join("fit"));
        write_dives(&json);

        cmd_fit(json.clone(), output.clone(), None, None, None, 0, 1).unwrap();
        let files: Vec<String> = std::fs::read_dir(&output)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        // The first dive of the logbook, the ignored one not being counted
        assert_eq!(files, ["2025-10-26_12h21_dive_001.fit"]);

        // which has no number to ask for it by
        assert!(cmd_fit(json, output, None, Some(47), None, 0, 1).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn correlate_imports_the_dives_only_the_logbook_has() {
        let dir = scratch("import");
        let (json, ssi) = (dir.join("dives.json"), dir.join("ssi.csv"));
        write_dives(&json);
        std::fs::write(
            &ssi,
            concat!(
                "\"plongée #\",\"Site de plongée\",\"Pays\",\"Date / Heure\",",
                "\"Type d'activité de plongée\",\"Plongée de Spécialité\",\"Type de plongée\",",
                "\"Durée\",\"Profondeur\",\"Equipier / Instructor / Center\"\n",
                // The dive of the watch
                "\"100\",\"Red Rocks\",\"Croatie\",\"26. Oct 2025 12:21\",\"Plongée Récréative\",",
                "\"\",\"Exploration\",\"45 min\",\"24 m \t\t(79 ft)\",\"Venus\"\n",
                // The same dive entered again, nine minutes into it
                "\"101\",\"Elsewhere\",\"\",\"26. Oct 2025 12:30\",\"Plongée Récréative\",",
                "\"\",\"\",\"40 min\",\"20 m\",\"\"\n",
                // Two dives from before the watch
                "\"12\",\"La muraillette\",\"France\",\"18. Jun 2022 11:54\",\"Plongée Récréative\",",
                "\"\",\"Exploration\",\"31 min\",\"18.2 m \t\t(60 ft)\",\"Sirius\"\n",
                "\"5\",\"Fosse\",\"France\",\"30. Mar 2022 16:22\",\"Apnée\",",
                "\"\",\"\",\"CWT\",\"7 m\",\"Serge MASAxel Brosse \t\t  Argonaute \t Mares Sirius\"\n",
                // Not a dive of its own: it only tells who Serge MAS is
                "\"102\",\"Red Rocks\",\"\",\"26. Oct 2025 12:21\",\"\",\"\",\"\",\"\",\"\",\"Serge MAS\"\n",
            ),
        )
        .unwrap();

        cmd_correlate(ssi.clone(), json.clone()).unwrap();

        // In the file by date: the two imported dives, which the watch has
        // no number for, then its own
        let dives = read_dives(&json);
        let numbers: Vec<u32> = dives.iter().map(|dive| dive.number).collect();
        assert_eq!(numbers, [0, 0, 47, 48]);
        // In the logbook they come first, the ignored dive not being counted
        assert_eq!(logbook_numbers(&dives), [Some(1), Some(1), None, Some(2)]);

        let freedive = &dives[0];
        assert_eq!(freedive.dive_mode, DiveMode::Freedive);
        assert_eq!((freedive.duration_seconds, freedive.max_depth_m), (0, 7.0));
        // The buddies glued to one another, then the dive center
        assert_eq!(
            freedive.buddy.as_deref(),
            Some("Serge MAS, Axel Brosse, Argonaute")
        );

        let scuba = &dives[1];
        assert_eq!(scuba.datetime.to_string(), "2022-06-18 11:54:00");
        assert_eq!((scuba.duration_seconds, scuba.max_depth_m), (1860, 18.2));
        assert_eq!(scuba.site.as_deref(), Some("La muraillette"));
        assert_eq!(scuba.country.as_deref(), Some("France"));
        // "Sirius" alone in the buddy column is the dive computer, not a buddy
        assert_eq!(scuba.buddy, None);
        assert!(scuba.samples.is_empty() && !scuba.ignored);

        assert_eq!(dives[3].site.as_deref(), Some("Red Rocks"));
        assert_eq!(dives[3].buddy.as_deref(), Some("Serge MAS"));

        // A second run finds them all in place
        cmd_correlate(ssi, json.clone()).unwrap();
        assert_eq!(read_dives(&json).len(), 4);

        // With no profile, the imported dives are not for the FIT export
        let output = dir.join("fit");
        cmd_fit(json, output.clone(), None, None, None, 0, 1).unwrap();
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn buddies_glued_together_are_told_apart() {
        let blocks: BTreeSet<String> = [
            "Serge MAS",
            "Serge MASAxel Brosse",
            "Matthieu Martinez",
            "Matthieu Martinezpaul cochet",
            "paul cochetJeremie RahmMatthieu Martinez",
            // Not "Serge MAS" and more: a name of its own
            "Serge MASSON",
            // nor "Dive Systems" and "Malta"
            "Dive Systems",
            "Dive Systems Malta",
        ]
        .map(String::from)
        .into();
        let parts = glued_names(&blocks);
        let names = |block: &str| {
            let mut names = Vec::new();
            names_of(block, &parts, &mut names);
            names.join(", ")
        };

        assert_eq!(names("Serge MASAxel Brosse"), "Serge MAS, Axel Brosse");
        // A name in lower case, learnt from another dive
        assert_eq!(
            names("Matthieu Martinezpaul cochet"),
            "Matthieu Martinez, paul cochet"
        );
        assert_eq!(
            names("paul cochetJeremie RahmMatthieu Martinez"),
            "paul cochet, Jeremie Rahm, Matthieu Martinez"
        );
        assert_eq!(names("Serge MASSON"), "Serge MASSON");
        assert_eq!(names("Dive Systems Malta"), "Dive Systems Malta");
    }

    #[test]
    fn buddy_column_is_cut_at_the_runs_of_blanks() {
        assert_eq!(
            buddy_blocks("Richard Puntous\t\t\t        LAGUNE PLONGEE   \t Sirius"),
            ["Richard Puntous", "LAGUNE PLONGEE"]
        );
        assert!(buddy_blocks("  \t Mares Sirius").is_empty());
    }

    #[test]
    fn number_at_the_start_of_a_field() {
        assert_eq!(leading_number("45 min"), Some(45.0));
        assert_eq!(leading_number("18.2 m \t(60 ft)"), Some(18.2));
        assert_eq!(leading_number("24 m(79 ft)"), Some(24.0));
        assert_eq!(leading_number("CWT"), None);
    }
}
