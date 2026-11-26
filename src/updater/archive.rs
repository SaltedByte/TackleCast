#[cfg(unix)]
use std::collections::BTreeMap;
use std::ffi::OsString;
#[cfg(unix)]
use std::fs;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use tar::Archive;
use zip::ZipArchive;

use super::{install, ArchiveKind};

const MAX_ENTRIES: usize = 10_000;
const MAX_EXTRACTED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
// Allow headers, padding and the end marker in addition to the extracted data.
const MAX_TAR_BYTES: u64 = MAX_EXTRACTED_BYTES + 2 * 1024 * MAX_ENTRIES as u64;

pub(super) fn extract(kind: ArchiveKind, archive: &Path, dest: &Path) -> io::Result<()> {
    match kind {
        ArchiveKind::Zip => extract_zip(archive, dest),
        ArchiveKind::TarGz => extract_tar_gz(archive, dest),
    }
}

fn archive_path(raw: &Path) -> io::Result<PathBuf> {
    let mut path = PathBuf::new();
    for part in raw.components() {
        match part {
            Component::Normal(name) => path.push(name),
            Component::CurDir => {}
            _ => {
                return Err(io::Error::other(format!(
                    "Unsafe archive path {}",
                    raw.display()
                )))
            }
        }
    }
    Ok(path)
}

// Windows archives are flat. Linux releases have a common wrapper directory.
fn wrapper(paths: &[PathBuf]) -> Option<OsString> {
    let first = paths.first()?.iter().next()?;
    let all_wrapped = paths.iter().all(|path| path.iter().next() == Some(first));
    let has_children = paths.iter().any(|path| path.iter().nth(1).is_some());
    (all_wrapped && has_children).then(|| first.to_os_string())
}

fn destination<'a>(
    path: &'a Path,
    wrapper: Option<&OsString>,
    is_dir: bool,
) -> io::Result<Option<&'a Path>> {
    let relative = wrapper
        .and_then(|name| path.strip_prefix(name).ok())
        .unwrap_or(path);
    // Windows packages may contain an empty logs directory.
    if relative.as_os_str().is_empty()
        || (is_dir && relative.as_os_str().eq_ignore_ascii_case("logs"))
    {
        return Ok(None);
    }
    install::validate_path(relative)?;
    Ok(Some(relative))
}

#[derive(Default)]
struct ArchiveBudget {
    entries: usize,
    bytes: u64,
}

impl ArchiveBudget {
    fn add(&mut self, size: u64) -> io::Result<()> {
        self.entries += 1;
        self.bytes = self.bytes.saturating_add(size);
        if self.entries > MAX_ENTRIES || self.bytes > MAX_EXTRACTED_BYTES {
            return Err(io::Error::other(
                "Update archive exceeds the extraction limits",
            ));
        }
        Ok(())
    }
}

fn copy_file(reader: impl Read, size: u64, dest: &Path, mode: Option<u32>) -> io::Result<()> {
    let mut file = File::create_new(dest)?;
    if io::copy(&mut reader.take(size + 1), &mut file)? != size {
        return Err(io::Error::other(
            "Archive file size does not match its header",
        ));
    }
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

fn create_dir(root: &Path, relative: &Path) -> io::Result<()> {
    // Checking a synthetic child also checks the archive directory itself.
    install::check_parents(root, &relative.join("entry"), true)
}

fn extract_zip(path: &Path, root: &Path) -> io::Result<()> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    let mut budget = ArchiveBudget::default();
    let mut names = Vec::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        budget.add(entry.size())?;
        let raw = Path::new(entry.name());
        names.push(archive_path(raw)?);
    }
    let wrapper = wrapper(&names);
    for (index, name) in names.iter().enumerate() {
        let entry = archive.by_index(index)?;
        if entry.is_symlink() {
            return Err(io::Error::other(
                "ZIP update archives cannot contain symlinks",
            ));
        }
        let Some(relative) = destination(name, wrapper.as_ref(), entry.is_dir())? else {
            if !entry.is_dir() {
                return Err(io::Error::other("Expected an archive wrapper directory"));
            }
            continue;
        };
        if entry.is_dir() {
            create_dir(root, relative)?;
        } else {
            install::check_parents(root, relative, true)?;
            let (size, mode) = (entry.size(), entry.unix_mode());
            copy_file(entry, size, &root.join(relative), mode)?;
        }
    }
    Ok(())
}

fn open_tar(path: &Path) -> io::Result<Archive<impl Read>> {
    Ok(Archive::new(
        GzDecoder::new(File::open(path)?).take(MAX_TAR_BYTES),
    ))
}

fn extract_tar_gz(path: &Path, root: &Path) -> io::Result<()> {
    let mut budget = ArchiveBudget::default();
    let mut names = Vec::new();
    for entry in open_tar(path)?.entries()?.raw(true) {
        let entry = entry?;
        budget.add(entry.size())?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
            return Err(io::Error::other("Unsupported tar update entry"));
        }
        names.push(archive_path(&entry.path()?)?);
    }
    let wrapper = wrapper(&names);

    #[cfg(unix)]
    let mut links = BTreeMap::new();
    for (entry, name) in open_tar(path)?.entries()?.raw(true).zip(names.iter()) {
        let entry = entry?;
        let kind = entry.header().entry_type();
        let Some(relative) = destination(name, wrapper.as_ref(), kind.is_dir())? else {
            if !kind.is_dir() {
                return Err(io::Error::other("Expected an archive wrapper directory"));
            }
            continue;
        };
        if kind.is_dir() {
            create_dir(root, relative)?;
        } else if kind.is_file() {
            install::check_parents(root, relative, true)?;
            let (size, mode) = (entry.size(), entry.header().mode()?);
            copy_file(entry, size, &root.join(relative), Some(mode))?;
        } else {
            #[cfg(unix)]
            {
                let target = entry
                    .link_name()?
                    .ok_or_else(|| io::Error::other("Missing symlink target"))?;
                let mut parts = target.components();
                let (Some(Component::Normal(target)), None) = (parts.next(), parts.next()) else {
                    return Err(io::Error::other(
                        "Update symlink must point to a file in the same directory",
                    ));
                };
                let resolved = relative.with_file_name(target);
                install::validate_path(&resolved)?;
                if links.insert(relative.to_path_buf(), resolved).is_some() {
                    return Err(io::Error::other("Duplicate update symlink"));
                }
            }
            #[cfg(not(unix))]
            return Err(io::Error::other(
                "This platform does not support update symlinks",
            ));
        }
    }
    #[cfg(unix)]
    for (link, target) in &links {
        let mut resolved = target;
        for _ in 0..32 {
            let Some(next) = links.get(resolved) else {
                break;
            };
            resolved = next;
        }
        if links.contains_key(resolved)
            || !fs::symlink_metadata(root.join(resolved))
                .is_ok_and(|meta| install::is_regular_file(&meta))
        {
            return Err(io::Error::other(
                "Update symlink must resolve to an extracted file",
            ));
        }
        install::check_parents(root, link, false)?;
        std::os::unix::fs::symlink(
            target.file_name().expect("validated link target"),
            root.join(link),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_limits_count_entries_and_unpacked_bytes() {
        let mut budget = ArchiveBudget {
            entries: MAX_ENTRIES - 1,
            bytes: MAX_EXTRACTED_BYTES - 1,
        };
        assert!(budget.add(1).is_ok());
        assert!(budget.add(0).is_err());
        assert!(ArchiveBudget::default()
            .add(MAX_EXTRACTED_BYTES + 1)
            .is_err());
    }
}
