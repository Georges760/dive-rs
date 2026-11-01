//! Apeks DSX support. Experimental: written from libdivecomputer and
//! DiverLog+, tried on one device so far.

pub mod parser;
pub mod protocol;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use btleplug::platform::Peripheral;
use chrono::NaiveDate;

use crate::ble::{self, BleConnection};
use crate::merge;
use crate::types::{DiveData, DiveLog};

use parser::{FlashMap, LogbookEntry};

/// Where the access code handed out at pairing is kept, one file per device.
fn access_code_path(device_name: &str) -> Result<PathBuf> {
    device_file(device_name, "key")
}

/// Where the date the dives of a device start from is kept.
fn since_path(device_name: &str) -> Result<PathBuf> {
    device_file(device_name, "since")
}

fn device_file(device_name: &str, extension: &str) -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base
        .join("dive-rs")
        .join(format!("dsx-{device_name}.{extension}")))
}

/// Read a stored access code (32 hex digits), if there is one.
fn load_access_code(path: &Path) -> Result<Option<[u8; 16]>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
    };
    let text = text.trim();

    let mut code = [0u8; 16];
    if text.len() != 32 || !text.is_ascii() {
        bail!("{} does not hold a 16-byte access code", path.display());
    }
    for (i, byte) in code.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("Invalid access code in {}", path.display()))?;
    }
    Ok(Some(code))
}

fn save_access_code(path: &Path, code: &[u8; 16]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text: String = code.iter().map(|byte| format!("{byte:02x}")).collect();
    std::fs::write(path, text + "\n").with_context(|| format!("Failed to write {}", path.display()))
}

/// Read the date stored for a device, if there is one.
fn load_since(path: &Path) -> Result<Option<NaiveDate>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
    };
    let date = text
        .trim()
        .parse()
        .with_context(|| format!("{} does not hold a date", path.display()))?;
    Ok(Some(date))
}

fn save_since(path: &Path, date: NaiveDate) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{date}\n"))
        .with_context(|| format!("Failed to write {}", path.display()))
}

fn parse_passcode(text: &str) -> Result<[u8; 6]> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|digit| digit - b'0')
        .collect();
    if digits.len() != 6 || text.len() != 6 {
        bail!("The passcode must be 6 digits");
    }
    Ok(digits.try_into().unwrap())
}

async fn connect(peripheral: &Peripheral) -> Result<BleConnection> {
    let conn = ble::connect(
        peripheral,
        Some(protocol::WRITE_UUID),
        Some(protocol::NOTIFY_UUID),
    )
    .await?;
    // DiverLog+ waits about this long after enabling notifications
    tokio::time::sleep(Duration::from_millis(800)).await;
    Ok(conn)
}

/// Connect, present the stored access code, wake the device up.
async fn open(peripheral: &Peripheral, name: &str) -> Result<(BleConnection, protocol::DeviceId)> {
    let code = load_access_code(&access_code_path(name)?)?.unwrap_or([0; 16]);
    let mut conn = connect(peripheral).await?;

    let session = async {
        if !protocol::request_access(&mut conn, &code).await? {
            bail!("The DSX refused access. Pair first with `dive-rs dsx pair`.");
        }
        let id = protocol::wakeup(&mut conn).await?;
        if id.model != protocol::MODEL_DSX {
            bail!("Not a DSX: model code 0x{:04X}", id.model);
        }
        Ok(id)
    }
    .await;

    match session {
        Ok(id) => Ok((conn, id)),
        Err(e) => {
            conn.disconnect().await.ok();
            Err(e)
        }
    }
}

/// Pair with the DSX: exchange a passcode for an access code and store it.
pub async fn cmd_pair(peripheral: &Peripheral, name: &str, passcode: Option<String>) -> Result<()> {
    let path = access_code_path(name)?;
    let stored = load_access_code(&path)?;
    let passcode = match passcode {
        Some(text) => parse_passcode(&text)?,
        None => protocol::generate_passcode(),
    };

    let mut conn = connect(peripheral).await?;
    let result = async {
        if let Some(code) = stored {
            if protocol::request_access(&mut conn, &code).await? {
                eprintln!("Already paired: the stored access code is accepted.");
                return Ok(());
            }
            eprintln!("The stored access code is no longer accepted, pairing again.");
        }

        // The device wants an access request before the passcode exchange
        if protocol::request_access(&mut conn, &[0; 16]).await? {
            eprintln!("This DSX asks for no passcode.");
            return save_access_code(&path, &[0; 16]);
        }

        let digits: String = passcode.iter().map(|digit| digit.to_string()).collect();
        eprintln!("Sending passcode {digits}...");
        let code = protocol::request_access_code(&mut conn, &passcode).await?;
        save_access_code(&path, &code)?;
        eprintln!("Access code saved to {}", path.display());

        if !protocol::request_access(&mut conn, &code).await? {
            bail!("The DSX rejected the access code it just handed out");
        }
        eprintln!("Paired.");
        Ok(())
    }
    .await;

    conn.disconnect().await.ok();
    result
}

/// Show what the DSX reports about itself and its logbook.
pub async fn cmd_info(peripheral: &Peripheral, name: &str) -> Result<()> {
    let (mut conn, id) = open(peripheral, name).await?;

    let result = async {
        let hw_cal = protocol::read_hw_cal(&mut conn).await?;
        let map = FlashMap::parse(&protocol::read_flash_map(&mut conn).await?)?;

        println!("Device Info:");
        println!("  Model:      Apeks DSX (0x{:04X})", id.model);
        println!("  Firmware:   {}", id.firmware);
        match parser::serial_number(&hw_cal) {
            Some(serial) => println!("  Serial:     {serial:06}"),
            None => println!("  Serial:     unknown"),
        }
        println!("  ID:         {}", crate::protocol::hex_dump(&id.raw));
        println!("  Dives:      {}", map.logbook_entries()?.len());
        println!(
            "  Logbook:    0x{:08X} .. 0x{:08X}",
            map.logbook_first, map.logbook_last
        );
        println!(
            "  Profiles:   0x{:08X} .. 0x{:08X}",
            map.profile_first, map.profile_last
        );
        Ok(())
    }
    .await;

    conn.disconnect().await.ok();
    result
}

/// Download the dives that are not yet in `output`.
pub async fn cmd_download(
    peripheral: &Peripheral,
    name: &str,
    output: PathBuf,
    save_raw: Option<PathBuf>,
    since: Option<NaiveDate>,
) -> Result<()> {
    // A used DSX still holds the dives of its former owner: the date stays
    // with the device, so that they do not come back on the next download
    let since_file = since_path(name)?;
    if let Some(date) = since {
        save_since(&since_file, date)?;
    }
    let since = match since {
        Some(date) => Some(date),
        None => load_since(&since_file)?,
    };

    let mut existing_dives: Vec<DiveLog> = Vec::new();
    if output.exists() {
        let contents = std::fs::read_to_string(&output)
            .with_context(|| format!("Failed to read {}", output.display()))?;
        let data: DiveData = serde_json::from_str(&contents)
            .with_context(|| format!("Failed to parse {}", output.display()))?;
        eprintln!(
            "Loaded {} existing dive(s) from {}",
            data.dives.len(),
            output.display()
        );
        existing_dives = data.dives;
    }
    let existing: HashSet<u32> = existing_dives
        .iter()
        .filter_map(|dive| dive.dsx_number)
        .collect();

    let (mut conn, id) = open(peripheral, name).await?;
    eprintln!("Connected to Apeks DSX, firmware {}", id.firmware);

    let mut new_dives = Vec::new();
    let mut skipped = 0u32;
    let mut left_out = 0u32;

    // An error part-way through must not lose the dives fetched so far
    let result: Result<()> = async {
        let hw_cal = protocol::read_hw_cal(&mut conn).await?;
        let flash_map = protocol::read_flash_map(&mut conn).await?;
        if let Some(ref raw_dir) = save_raw {
            std::fs::create_dir_all(raw_dir)?;
            std::fs::write(raw_dir.join("hw_cal.bin"), &hw_cal)?;
            std::fs::write(raw_dir.join("flash_map.bin"), &flash_map)?;
        }

        let entries = FlashMap::parse(&flash_map)?.logbook_entries()?;
        eprintln!("Found {} logbook entr(ies)", entries.len());

        for (i, &address) in entries.iter().enumerate() {
            eprint!("\rChecking dive {}/{}...", i + 1, entries.len());
            let raw_entry = protocol::read_flash(&mut conn, address, parser::LOGBOOK_ENTRY).await?;

            let entry = match LogbookEntry::parse(&raw_entry) {
                // Entries are read newest first: nothing older than an unused slot
                Ok(None) => break,
                Ok(Some(entry)) => entry,
                Err(e) => {
                    eprintln!("\r  Logbook entry at 0x{address:08X}: {e}");
                    continue;
                }
            };

            if since.is_some_and(|date| entry.datetime.date() < date) {
                left_out += 1;
                continue;
            }

            if existing.contains(&entry.number) {
                skipped += 1;
                continue;
            }

            eprint!("\rDownloading dive {}/{}...", i + 1, entries.len());
            let ranges = match entry.profile_ranges() {
                Ok(ranges) => ranges,
                Err(e) => {
                    eprintln!("\r  Dive #{}: {e}, skipping", entry.number);
                    continue;
                }
            };
            let mut profile = Vec::new();
            for (start, size) in ranges {
                if size > 0 {
                    profile.extend(protocol::read_flash(&mut conn, start, size).await?);
                }
            }

            if let Some(ref raw_dir) = save_raw {
                let stem = format!(
                    "dive_{:03}_{}",
                    entry.number,
                    entry.datetime.format("%Y%m%d_%H%M")
                );
                std::fs::write(raw_dir.join(format!("{stem}_logbook.bin")), &raw_entry)?;
                std::fs::write(raw_dir.join(format!("{stem}_profile.bin")), &profile)?;
            }

            match parser::parse_dive(&entry, &profile) {
                Ok(dive) => {
                    eprintln!(
                        "\r  Dive #{}: {} | {:.1}m | {}s | {} samples",
                        entry.number,
                        dive.datetime.format("%Y-%m-%d %H:%M"),
                        dive.max_depth_m,
                        dive.duration_seconds,
                        dive.samples.len(),
                    );
                    new_dives.push(dive);
                }
                Err(e) => eprintln!("\r  Dive #{}: parse error: {e}", entry.number),
            }
        }
        Ok(())
    }
    .await;
    eprintln!();

    if let Err(e) = &result {
        eprintln!("Download interrupted: {e:#}");
    }
    conn.disconnect().await.ok();

    if skipped > 0 {
        eprintln!("Skipped {skipped} already-downloaded dive(s)");
    }
    if let (true, Some(date)) = (left_out > 0, since) {
        eprintln!("Left out {left_out} dive(s) from before {date}");
    }
    if new_dives.is_empty() {
        eprintln!("No new dives.");
        return result;
    }
    eprintln!("Downloaded {} new dive(s)", new_dives.len());

    let mut all_dives = existing_dives;
    let merged = merge::add_dives(&mut all_dives, new_dives);
    if merged > 0 {
        eprintln!("Merged {merged} into the entry another computer had for the same dive");
    }

    let data = DiveData { dives: all_dives };
    std::fs::write(&output, serde_json::to_string_pretty(&data)?)?;
    eprintln!(
        "Dive data saved to {} ({} dives)",
        output.display(),
        data.dives.len()
    );

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_date_is_remembered() {
        let dir = std::env::temp_dir().join(format!("dive-rs-since-{}", std::process::id()));
        let path = dir.join("dsx-GA000001.since");

        assert_eq!(load_since(&path).unwrap(), None);
        let date = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        save_since(&path, date).unwrap();
        assert_eq!(load_since(&path).unwrap(), Some(date));

        std::fs::write(&path, "soon\n").unwrap();
        assert!(load_since(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
