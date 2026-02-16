mod ble;
mod parser;
mod protocol;
mod tui;
mod types;

use std::collections::{HashMap, HashSet};
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
        csv: PathBuf,

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

        /// Time offset in seconds applied to video capture time (positive = shift video time forward, negative = shift back)
        #[arg(short, long, default_value = "0", allow_hyphen_values = true)]
        offset: i64,
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
        Commands::View { input } => tui::run(input),
        Commands::Correlate { csv, json } => cmd_correlate(csv, json),
        Commands::Watermark {
            video,
            json,
            offset,
        } => cmd_watermark(video, json, offset),
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
                    for dive in &data.dives {
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
        let profile = protocol::read_dive_profile(&mut conn, i).await?;

        if let Some(ref raw_dir) = save_raw {
            std::fs::create_dir_all(raw_dir)?;
            std::fs::write(raw_dir.join(format!("dive_{i:03}_header.bin")), &header)?;
            std::fs::write(raw_dir.join(format!("dive_{i:03}_profile.bin")), &profile)?;
        }

        match parser::parse_dive_ecop(i as u32, &header, &profile) {
            Ok(dive) => {
                eprintln!(
                    "\r  Dive #{}: {} | {:.1}m | {}s | {} samples",
                    dive.number,
                    dive.datetime.format("%Y-%m-%d %H:%M"),
                    dive.max_depth_m,
                    dive.duration_seconds,
                    dive.samples.len(),
                );
                new_dives.push(dive);
            }
            Err(e) => {
                eprintln!("\r  Dive {i}: parse error: {e}");
            }
        }
    }
    eprintln!();

    conn.disconnect().await?;

    if skipped > 0 {
        eprintln!("Skipped {} already-downloaded dive(s)", skipped);
    }
    if !new_dives.is_empty() {
        eprintln!("Downloaded {} new dive(s)", new_dives.len());
    }

    // Merge existing + new dives
    let mut all_dives = existing_dives;
    all_dives.append(&mut new_dives);
    all_dives.sort_by_key(|d| d.number);

    if all_dives.is_empty() {
        eprintln!("No dives could be parsed.");
        return Ok(());
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

    Ok(())
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
                    "  Dive #{}: {} | {:.1}m | {}min | {} samples | {:?}",
                    dive.number,
                    dive.datetime.format("%Y-%m-%d %H:%M"),
                    dive.max_depth_m,
                    dive.duration_seconds / 60,
                    dive.samples.len(),
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

/// Clean buddy field: normalize whitespace, strip trailing "Sirius"/"Mares", trim.
fn clean_buddy(raw: &str) -> String {
    let normalized: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = normalized
        .trim_end_matches("Sirius")
        .trim_end_matches("Mares")
        .trim();
    trimmed.to_string()
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

    let date_col = col("Date / Temps").unwrap_or(3);
    let site_col = col("Site de plongée").unwrap_or(1);
    let country_col = col("Pays").unwrap_or(2);
    let buddy_col = col("Equipier / Instructor / Center").unwrap_or(9);

    let mut records = Vec::new();
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

        records.push(SsiRecord {
            datetime,
            site: fields[site_col].trim().to_string(),
            country: fields[country_col].trim().to_string(),
            buddy: clean_buddy(&fields[buddy_col]),
        });
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

    for dive in &mut data.dives {
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
            }
            matched += 1;
        } else {
            unmatched += 1;
        }
    }

    eprintln!("Matched: {}, Unmatched: {}", matched, unmatched);

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

fn probe_video(path: &std::path::Path) -> Result<VideoMeta> {
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

    // Extract capture time from format.tags.comment
    let comment = json["format"]["tags"]["comment"]
        .as_str()
        .or_else(|| json["format"]["tags"]["Comment"].as_str())
        .context("No 'comment' tag found in video metadata. Cannot determine capture time.")?;

    let capture_time = chrono::DateTime::parse_from_str(comment.trim(), "%Y-%m-%d %H:%M:%S %z")
        .with_context(|| format!("Failed to parse comment timestamp: {comment:?}"))?
        .naive_utc();

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
    let video_start = video_start + chrono::Duration::seconds(offset);
    let video_end = video_start + chrono::Duration::milliseconds((video_duration * 1000.0) as i64);

    let mut best: Option<(&DiveLog, i64)> = None;

    for dive in dives {
        let dive_end = dive.datetime + chrono::Duration::seconds(dive.duration_seconds as i64);

        let overlap_start = video_start.max(dive.datetime);
        let overlap_end = video_end.min(dive_end);
        let overlap = (overlap_end - overlap_start).num_seconds();

        if overlap > 0 && (best.is_none() || overlap > best.unwrap().1) {
            best = Some((dive, overlap));
        }
    }

    match best {
        Some((dive, overlap)) => {
            eprintln!(
                "Matched dive #{} ({}) — {:.0}s overlap",
                dive.number,
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
                .filter(|d| d.datetime.date() == video_date)
                .collect();
            if same_day.is_empty() {
                eprintln!("No dives found on {video_date}.");
            } else {
                eprintln!("Dives on {video_date}:");
                for dive in &same_day {
                    let dive_end =
                        dive.datetime + chrono::Duration::seconds(dive.duration_seconds as i64);
                    eprintln!(
                        "  #{}: {} to {}",
                        dive.number,
                        dive.datetime.format("%H:%M:%S"),
                        dive_end.format("%H:%M:%S")
                    );
                }
            }
            anyhow::bail!(
                "No dive overlaps with the video time range. \
                 Use --offset to adjust (e.g. --offset 60 shifts video time forward by 60s)."
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
) -> String {
    let video_start = video_start + chrono::Duration::seconds(offset);
    let dive_start_offset = (video_start - dive.datetime).num_seconds();

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
            :fontcolor=white:fontsize=48\
            :borderw=2:bordercolor=black\
            :shadowcolor=black@0.5:shadowx=2:shadowy=2\
            :x=W-tw-20:y=H-th-20\
            :enable='between(t,{start_t:.3},{end_t:.3})'"
        ));
    }

    if filters.is_empty() {
        eprintln!("Warning: no dive samples fall within the video time range. Output will have no overlay.");
        return String::new();
    }

    filters.join(",")
}

fn cmd_watermark(video: PathBuf, json: PathBuf, offset: i64) -> Result<()> {
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
    let meta = probe_video(&video)?;
    eprintln!(
        "  Capture time: {} UTC",
        meta.capture_time.format("%Y-%m-%d %H:%M:%S")
    );
    eprintln!("  Resolution: {}x{}", meta.width, meta.height);
    eprintln!("  Duration: {:.1}s", meta.duration_secs);

    if offset != 0 {
        eprintln!("  Time offset: {offset}s");
    }

    // Find matching dive
    let dive = find_overlapping_dive(&data.dives, meta.capture_time, meta.duration_secs, offset)?;

    // Build filter
    let filter = build_drawtext_filter(dive, meta.capture_time, meta.duration_secs, offset);

    // Build output path: YYYY-MM-DD_HHhMM_Site_Name.ext
    let ext = video.extension().unwrap_or_default().to_string_lossy();
    let dt_str = dive.datetime.format("%Y-%m-%d_%Hh%M").to_string();
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
