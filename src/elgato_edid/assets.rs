//! Elgato's asset manifest and the 4K X's EDID files, kept current in
//! TackleCast's own data folder.
//!
//! The manifest comes from the same public update service Elgato Studio uses,
//! checked at most once a day. Each EDID file the 4K X needs is taken from, in
//! order: our verified copy, Studio's cache, or Elgato's CDN. Every file is
//! checked against the manifest's SHA-256 before it is kept. When Elgato can't
//! be reached, the last saved manifest (or Studio's) is used.

use super::win;
use std::path::Path;
use std::time::{Duration, SystemTime};

pub const MANIFEST_URL: &str =
    "https://gc-updates.elgato.com/windows/estw-update/final/device-update-check.json.php";
pub const MANIFEST_FILE: &str = "device-asset-manifest.json";
const CHECKED_STAMP: &str = "manifest-checked";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const TIMEOUT_MS: i32 = 8_000;
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_EDID_BYTES: usize = 64 * 1024;

pub struct Assets {
    pub manifest: Vec<u8>,
    /// Where the manifest came from, for the self-test.
    pub source: String,
    /// EDID files the 4K X needs, and how many are present and verified.
    pub edids_needed: usize,
    pub edids_ok: usize,
    /// Something the user should know, e.g. that Elgato couldn't be reached.
    pub note: Option<String>,
}

struct EdidEntry {
    filename: String,
    url: String,
    sha256: String,
}

/// Bring the manifest and the 4K X's EDID files up to date. `force_check`
/// ignores the once-a-day limit.
pub fn sync(
    data_dir: &Path,
    edid_dir: &Path,
    studio_tmp: &Path,
    force_check: bool,
) -> Result<Assets, String> {
    std::fs::create_dir_all(edid_dir)
        .map_err(|e| format!("Cannot create {}: {e}", edid_dir.display()))?;
    let ours = data_dir.join(MANIFEST_FILE);
    let stamp = data_dir.join(CHECKED_STAMP);
    let mut note = None;
    let mut online = true;
    let mut source = "saved copy from Elgato".to_string();

    if force_check || age(&stamp).is_none_or(|age| age > CHECK_INTERVAL) {
        match win::https_get(MANIFEST_URL, MAX_MANIFEST_BYTES, TIMEOUT_MS)
            .and_then(|bytes| validate_manifest(&bytes).map(|_| bytes))
        {
            Ok(bytes) => {
                write_if_changed(&ours, &bytes)?;
                let _ = std::fs::write(&stamp, b"");
                source = "downloaded from Elgato just now".to_string();
            }
            Err(error) => {
                online = false;
                note = Some(format!(
                    "Couldn't check Elgato for an updated EDID list, so the saved one is used ({error})."
                ));
            }
        }
    }

    // Studio's copy is also Elgato's; use it when ours is missing or older.
    let studio_manifest = studio_tmp.join(MANIFEST_FILE);
    let studio_newer = match (modified(&studio_manifest), modified(&ours)) {
        (Some(_), None) => true,
        (Some(studio), Some(ours)) => studio > ours,
        _ => false,
    };
    if studio_newer {
        if let Ok(bytes) = std::fs::read(&studio_manifest) {
            if validate_manifest(&bytes).is_ok() {
                write_if_changed(&ours, &bytes)?;
                source = "Elgato Studio's copy".to_string();
            }
        }
    }
    let manifest = std::fs::read(&ours).map_err(|_| {
        "Elgato's EDID list isn't on this PC and couldn't be downloaded. Check the internet \
         connection, or open Elgato Studio once."
            .to_string()
    })?;

    let entries = edid_entries(&manifest)?;
    let mut ok = 0;
    for entry in &entries {
        let dst = edid_dir.join(&entry.filename);
        if sha_matches(&dst, &entry.sha256) {
            ok += 1;
            continue;
        }
        let studio_copy = studio_tmp.join("EDIDFiles").join(&entry.filename);
        if sha_matches(&studio_copy, &entry.sha256) {
            if let Ok(bytes) = std::fs::read(&studio_copy) {
                write_if_changed(&dst, &bytes)?;
                ok += 1;
                continue;
            }
        }
        if !online {
            continue;
        }
        match win::https_get(&entry.url, MAX_EDID_BYTES, TIMEOUT_MS) {
            Ok(bytes) if win::sha256_hex(&bytes)?.eq_ignore_ascii_case(&entry.sha256) => {
                write_if_changed(&dst, &bytes)?;
                ok += 1;
            }
            Ok(_) => {
                note = Some(format!(
                    "Elgato's download of {} didn't match its checksum, so it wasn't kept.",
                    entry.filename
                ));
            }
            Err(error) => {
                online = false;
                note = Some(format!(
                    "Couldn't download {} from Elgato ({error}).",
                    entry.filename
                ));
            }
        }
    }
    Ok(Assets {
        manifest,
        source,
        edids_needed: entries.len(),
        edids_ok: ok,
        note,
    })
}

fn validate_manifest(bytes: &[u8]) -> Result<(), String> {
    let entries = edid_entries(bytes)?;
    if entries.is_empty() {
        return Err("Elgato's EDID list has no entries for the 4K X".into());
    }
    Ok(())
}

/// The EDID files a 4K X can use: its own entries plus Elgato's shared capture
/// EDIDs. Entries with an unsafe file name or a non-Elgato URL are skipped.
fn edid_entries(manifest: &[u8]) -> Result<Vec<EdidEntry>, String> {
    let json: serde_json::Value = serde_json::from_slice(manifest)
        .map_err(|e| format!("Elgato's EDID list is invalid: {e}"))?;
    let devices = json["hardwareDevices"]
        .as_array()
        .ok_or("Elgato's EDID list has no device entries")?;
    let pids = super::pids_from_manifest(manifest);
    let mut out: Vec<EdidEntry> = Vec::new();
    for device in devices {
        let id = &device["id"];
        let is_4kx = id["vendorId"].as_u64() == Some(u64::from(super::ELGATO_VENDOR_ID))
            && id["productId"]
                .as_u64()
                .is_some_and(|pid| pids.iter().any(|p| u64::from(*p) == pid));
        let is_shared_capture =
            device["isShared"].as_bool() == Some(true) && device["category"] == "capture";
        if !is_4kx && !is_shared_capture {
            continue;
        }
        for edid in device["edidFiles"].as_array().into_iter().flatten() {
            let (Some(filename), Some(url), Some(sha256)) = (
                edid["filename"].as_str(),
                edid["downloadUrl"].as_str(),
                edid["sha256"].as_str(),
            ) else {
                continue;
            };
            let safe_name = filename.to_ascii_lowercase().ends_with(".bin")
                && !filename.contains(['/', '\\', ':'])
                && !filename.contains("..");
            let host = url
                .strip_prefix("https://")
                .and_then(|rest| rest.split('/').next())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !safe_name || !win::ALLOWED_HOSTS.contains(&host.as_str()) || sha256.len() != 64 {
                continue;
            }
            if !out
                .iter()
                .any(|e| e.filename.eq_ignore_ascii_case(filename))
            {
                out.push(EdidEntry {
                    filename: filename.to_string(),
                    url: url.to_string(),
                    sha256: sha256.to_string(),
                });
            }
        }
    }
    Ok(out)
}

fn sha_matches(path: &Path, sha256: &str) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| win::sha256_hex(&bytes).ok())
        .is_some_and(|actual| actual.eq_ignore_ascii_case(sha256))
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn age(path: &Path) -> Option<Duration> {
    modified(path).and_then(|time| SystemTime::now().duration_since(time).ok())
}

fn write_if_changed(dst: &Path, bytes: &[u8]) -> Result<(), String> {
    if std::fs::read(dst).ok().as_deref() != Some(bytes) {
        std::fs::write(dst, bytes).map_err(|e| format!("Cannot write {}: {e}", dst.display()))?;
    }
    Ok(())
}
