//! Elgato's device library, downloaded from Elgato for PCs without Studio.
//!
//! Elgato publishes Elgato Studio as an MSIX package (a ZIP64 archive) on its
//! CDN. Instead of the whole ~120 MB package, TackleCast reads the archive's
//! table of contents with HTTP range requests and fetches only the four files
//! it needs (about 5 MB). Each file must match the archive's CRC-32 and size,
//! and each DLL must carry a valid Authenticode signature from Corsair
//! (Elgato's parent company). Nothing is downloaded until the user asks.

use super::win;
use std::path::{Path, PathBuf};

/// Elgato's Studio version service. `final` currently lags behind `beta`;
/// the newest version across both is used.
const VERSION_URLS: [&str; 2] = [
    "https://gc-updates.elgato.com/windows/estw-update/beta/app-version-check.json.php",
    "https://gc-updates.elgato.com/windows/estw-update/final/app-version-check.json.php",
];
/// The publisher every downloaded DLL must be signed by.
pub const EXPECTED_SIGNER: &str = "Corsair Memory, Inc.";
/// A Studio version TackleCast is known to work with, used when neither the
/// installed Studio nor Elgato's newest package is compatible. Its files must
/// also match these SHA-256 hashes (taken from a verified install).
pub const PINNED_VERSION: &str = "2.0.0.3364";
const PINNED_URL: &str =
    "https://edge.elgato.com/egc/windows/estw/2.0.0/Elgato.Studio_2.0.0.3364_x64.msix";
const PINNED_SHA256: [(&str, &str); 4] = [
    (
        "EGAVDeviceSupport.dll",
        "C0DFC491EEE207792EB5FB1DE29933FD69E24E3A8067831F0DDB8A02CC814356",
    ),
    (
        "RTICE_SDK_x64.dll",
        "D1DA26A68D7D5A0763F97306F1B531750D22A9F3108AE77B7D69312D2F626985",
    ),
    (
        "RTK_IO_x64.dll",
        "272F98EC4ED9275829FC9E5B11FA0B8BE2031360FCBA6C9C8073EF11266B8BE8",
    ),
    (
        "ElgatoDeviceCapabilities.json",
        "D96B7C1D156A0A1EC62D10BFC4A16AEF67588071F2468E216D37F9B163A562FA",
    ),
];

/// The known-good package: (version, MSIX URL).
pub fn pinned_package() -> (String, String) {
    (PINNED_VERSION.to_string(), PINNED_URL.to_string())
}

/// Whether `version` is newer than `other` ("2.0.0.10" > "2.0.0.9").
pub fn is_newer(version: &str, other: &str) -> bool {
    version_key(version) > version_key(other)
}
/// Files TackleCast needs from the package, and whether each is a signed DLL.
const FILES: [(&str, bool); 4] = [
    ("EGAVDeviceSupport.dll", true),
    ("RTICE_SDK_x64.dll", true),
    ("RTK_IO_x64.dll", true),
    ("ElgatoDeviceCapabilities.json", false),
];
const DIR_PREFIX: &str = "download-";
const COMPLETE_MARKER: &str = "complete";
const TIMEOUT_MS: i32 = 20_000;
const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;

/// The newest Elgato Studio package Elgato offers: (version, MSIX URL).
pub fn latest_package() -> Result<(String, String), String> {
    let mut best: Option<(Vec<u32>, String, String)> = None;
    let mut last_error = String::new();
    for url in VERSION_URLS {
        let parsed = win::https_get(url, 1024 * 1024, TIMEOUT_MS).and_then(|bytes| {
            let json: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("Elgato's version list is invalid: {e}"))?;
            let version = json["Automatic"]["Version"]
                .as_str()
                .ok_or("Elgato's version list has no version")?
                .to_string();
            let msix = json["Automatic"]["fileURLs"]["x86_64"]
                .as_str()
                .ok_or("Elgato's version list has no x64 package")?
                .to_string();
            Ok((version, msix))
        });
        match parsed {
            Ok((version, msix)) => {
                let key = version_key(&version);
                if best.as_ref().is_none_or(|(k, _, _)| key > *k) {
                    best = Some((key, version, msix));
                }
            }
            Err(error) => last_error = error,
        }
    }
    best.map(|(_, version, msix)| (version, msix))
        .ok_or_else(|| format!("Couldn't reach Elgato to find its device library ({last_error})"))
}

fn version_key(version: &str) -> Vec<u32> {
    version
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// The newest complete downloaded library under `lib_root`, if any:
/// (version, folder).
pub fn installed(lib_root: &Path) -> Option<(String, PathBuf)> {
    std::fs::read_dir(lib_root)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let version = name.strip_prefix(DIR_PREFIX)?.to_string();
            let dir = entry.path();
            dir.join(COMPLETE_MARKER)
                .is_file()
                .then_some((version_key(&version), version, dir))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, version, dir)| (version, dir))
}

/// Download Elgato's library for `version` from `msix_url` into
/// `lib_root/download-<version>`, verified, and return that folder.
pub fn download(lib_root: &Path, version: &str, msix_url: &str) -> Result<PathBuf, String> {
    if !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(format!("Unexpected Elgato Studio version '{version}'"));
    }
    let dir = lib_root.join(format!("{DIR_PREFIX}{version}"));
    if dir.join(COMPLETE_MARKER).is_file() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let entries = read_directory(msix_url)?;
    for (name, signed) in FILES {
        let entry = entries
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| format!("Elgato's package has no {name}"))?;
        let bytes = fetch_entry(msix_url, entry)?;
        if version == PINNED_VERSION {
            let expected = PINNED_SHA256
                .iter()
                .find(|(file, _)| *file == name)
                .map(|(_, sha)| *sha)
                .unwrap_or_default();
            if !win::sha256_hex(&bytes)?.eq_ignore_ascii_case(expected) {
                return Err(format!(
                    "Refusing Elgato's {name}: it doesn't match the known-good {PINNED_VERSION} file"
                ));
            }
        }
        let target = dir.join(name);
        std::fs::write(&target, &bytes)
            .map_err(|e| format!("Cannot write {}: {e}", target.display()))?;
        if signed {
            let signer = win::authenticode_signer(&target);
            if signer.as_deref() != Ok(EXPECTED_SIGNER) {
                let _ = std::fs::remove_file(&target);
                return Err(format!(
                    "Refusing Elgato's {name}: expected a signature from {EXPECTED_SIGNER}, got {}",
                    match signer {
                        Ok(other) => other,
                        Err(error) => error,
                    }
                ));
            }
        }
    }
    std::fs::write(dir.join(COMPLETE_MARKER), version)
        .map_err(|e| format!("Cannot finish {}: {e}", dir.display()))?;
    Ok(dir)
}

struct ZipEntry {
    name: String,
    method: u16,
    crc32: u32,
    compressed: u64,
    uncompressed: u64,
    local_header: u64,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Read a ZIP64 archive's central directory over HTTP range requests.
fn read_directory(url: &str) -> Result<Vec<ZipEntry>, String> {
    const TAIL: usize = 64 * 1024;
    let bad = || format!("Elgato's package at {url} isn't a readable ZIP64 archive");
    let (tail, total) = win::https_get_range(url, &format!("-{TAIL}"), TAIL, TIMEOUT_MS)?;
    let tail_start = total.checked_sub(tail.len() as u64).ok_or_else(bad)?;
    // ZIP64 end-of-central-directory locator, then the record it points to.
    let locator = (0..tail.len().saturating_sub(20))
        .rev()
        .find(|&i| u32_at(&tail, i) == Some(0x0706_4b50))
        .ok_or_else(bad)?;
    let record = u64_at(&tail, locator + 8)
        .and_then(|offset| offset.checked_sub(tail_start))
        .ok_or_else(bad)? as usize;
    if u32_at(&tail, record) != Some(0x0606_4b50) {
        return Err(bad());
    }
    let dir_size = u64_at(&tail, record + 40).ok_or_else(bad)?;
    let dir_offset = u64_at(&tail, record + 48).ok_or_else(bad)?;
    if dir_size == 0 || dir_size > 16 * 1024 * 1024 {
        return Err(bad());
    }
    let (dir, _) = win::https_get_range(
        url,
        &format!("{dir_offset}-{}", dir_offset + dir_size - 1),
        dir_size as usize,
        TIMEOUT_MS,
    )?;
    parse_central_directory(&dir).ok_or_else(bad)
}

fn parse_central_directory(dir: &[u8]) -> Option<Vec<ZipEntry>> {
    let mut entries = Vec::new();
    let mut at = 0;
    while u32_at(dir, at) == Some(0x0201_4b50) {
        let method = u16_at(dir, at + 10)?;
        let crc32 = u32_at(dir, at + 16)?;
        let mut compressed = u64::from(u32_at(dir, at + 20)?);
        let mut uncompressed = u64::from(u32_at(dir, at + 24)?);
        let name_len = usize::from(u16_at(dir, at + 28)?);
        let extra_len = usize::from(u16_at(dir, at + 30)?);
        let comment_len = usize::from(u16_at(dir, at + 32)?);
        let mut local_header = u64::from(u32_at(dir, at + 42)?);
        let name = String::from_utf8_lossy(dir.get(at + 46..at + 46 + name_len)?).into_owned();
        // ZIP64 extra field: 64-bit values for whichever fields are 0xFFFFFFFF.
        let mut extra = at + 46 + name_len;
        let extra_end = extra + extra_len;
        while extra + 4 <= extra_end {
            let tag = u16_at(dir, extra)?;
            let len = usize::from(u16_at(dir, extra + 2)?);
            if tag == 1 {
                let mut field = extra + 4;
                for value in [&mut uncompressed, &mut compressed, &mut local_header] {
                    if *value == u64::from(u32::MAX) {
                        *value = u64_at(dir, field)?;
                        field += 8;
                    }
                }
            }
            extra += 4 + len;
        }
        entries.push(ZipEntry {
            name,
            method,
            crc32,
            compressed,
            uncompressed,
            local_header,
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Some(entries)
}

/// Download and unpack one archive entry, checking its size and CRC-32.
fn fetch_entry(url: &str, entry: &ZipEntry) -> Result<Vec<u8>, String> {
    if entry.uncompressed > MAX_FILE_BYTES as u64 || entry.compressed > MAX_FILE_BYTES as u64 {
        return Err(format!(
            "{} in Elgato's package is unexpectedly large",
            entry.name
        ));
    }
    // The local header's own name and extra lengths decide where data starts.
    let (header, _) = win::https_get_range(
        url,
        &format!("{}-{}", entry.local_header, entry.local_header + 29),
        30,
        TIMEOUT_MS,
    )?;
    if u32_at(&header, 0) != Some(0x0403_4b50) {
        return Err(format!(
            "{} has a bad header in Elgato's package",
            entry.name
        ));
    }
    let skip = 30
        + u64::from(u16_at(&header, 26).unwrap_or(0))
        + u64::from(u16_at(&header, 28).unwrap_or(0));
    let start = entry.local_header + skip;
    let (data, _) = if entry.compressed == 0 {
        (Vec::new(), 0)
    } else {
        win::https_get_range(
            url,
            &format!("{start}-{}", start + entry.compressed - 1),
            entry.compressed as usize,
            TIMEOUT_MS,
        )?
    };
    let bytes = match entry.method {
        0 => data,
        8 => miniz_oxide::inflate::decompress_to_vec_with_limit(&data, entry.uncompressed as usize)
            .map_err(|e| format!("Cannot unpack {}: {e:?}", entry.name))?,
        other => {
            return Err(format!(
                "{} uses an unsupported compression method ({other})",
                entry.name
            ))
        }
    };
    if bytes.len() as u64 != entry.uncompressed || crc32(&bytes) != entry.crc32 {
        return Err(format!(
            "{} from Elgato's package failed its integrity check",
            entry.name
        ));
    }
    Ok(bytes)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_zip_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn newer_versions_compare_numerically() {
        assert!(version_key("2.0.0.3364") > version_key("1.1.0.1714"));
        assert!(version_key("2.0.0.10000") > version_key("2.0.0.3364"));
    }

    #[test]
    fn reads_zip64_sizes_from_the_central_directory() {
        // One entry whose sizes and offset are in a ZIP64 extra field.
        let name = b"EGAVDeviceSupport.dll";
        let mut dir = Vec::new();
        dir.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        dir.extend_from_slice(&[0; 6]); // versions, flags
        dir.extend_from_slice(&8u16.to_le_bytes()); // deflate
        dir.extend_from_slice(&[0; 4]); // time, date
        dir.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // crc
        dir.extend_from_slice(&u32::MAX.to_le_bytes()); // compressed
        dir.extend_from_slice(&u32::MAX.to_le_bytes()); // uncompressed
        dir.extend_from_slice(&(name.len() as u16).to_le_bytes());
        dir.extend_from_slice(&28u16.to_le_bytes()); // extra length
        dir.extend_from_slice(&[0; 6]); // comment length, disk, internal attributes
        dir.extend_from_slice(&[0; 4]); // external attributes
        dir.extend_from_slice(&u32::MAX.to_le_bytes()); // local header offset
        dir.extend_from_slice(name);
        dir.extend_from_slice(&1u16.to_le_bytes());
        dir.extend_from_slice(&24u16.to_le_bytes());
        dir.extend_from_slice(&8_268_000u64.to_le_bytes()); // uncompressed
        dir.extend_from_slice(&2_521_000u64.to_le_bytes()); // compressed
        dir.extend_from_slice(&63_234_113u64.to_le_bytes()); // offset
        let entries = parse_central_directory(&dir).unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.name, "EGAVDeviceSupport.dll");
        assert_eq!((e.method, e.crc32), (8, 0xDEAD_BEEF));
        assert_eq!(
            (e.uncompressed, e.compressed, e.local_header),
            (8_268_000, 2_521_000, 63_234_113)
        );
    }
}
