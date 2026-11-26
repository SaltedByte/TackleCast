use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tempfile::{NamedTempFile, TempDir};

use super::{current_version, SemVer};

const STAGING_ROOT: &str = ".update-staging";
const STAGING_PREFIX: &str = "update-";
const LOCK_FILE: &str = ".update.lock";
const JOURNAL: &str = "transaction.json";
const COMPLETE: &str = "complete";
const PARENT_EXIT_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) fn is_portable(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join("build-info.json")).is_ok_and(|meta| is_regular_file(&meta))
}

pub(super) fn read_version(dir: &Path) -> io::Result<SemVer> {
    #[derive(Deserialize)]
    struct BuildInfo {
        version: String,
    }
    const MAX_BUILD_INFO: u64 = 64 * 1024;
    if !is_portable(dir) {
        return Err(io::Error::other("Missing portable build metadata"));
    }
    let mut bytes = Vec::new();
    File::open(dir.join("build-info.json"))?
        .take(MAX_BUILD_INFO + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BUILD_INFO {
        return Err(io::Error::other("Portable build metadata is too large"));
    }
    let info: BuildInfo =
        serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes))?;
    SemVer::parse(&info.version)
        .ok_or_else(|| io::Error::other("Portable build version is invalid"))
}

pub(super) fn is_regular_file(meta: &Metadata) -> bool {
    meta.is_file() && !is_link(meta)
}

fn is_link(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        meta.is_symlink()
    }
}

fn metadata_if_exists(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn directory_exists(path: &Path) -> io::Result<bool> {
    match metadata_if_exists(path)? {
        Some(meta) if meta.is_dir() && !is_link(&meta) => Ok(true),
        Some(_) => Err(io::Error::other(format!(
            "Expected a real directory at {}",
            path.display()
        ))),
        None => Ok(false),
    }
}

fn ensure_directory(path: &Path) -> io::Result<()> {
    if !directory_exists(path)? {
        match fs::create_dir(path) {
            Ok(()) => sync_directory(path.parent().expect("update directory has a parent"))?,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists && directory_exists(path)? => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Walks each destination parent from a known root without following links or junctions.
pub(super) fn check_parents(root: &Path, relative: &Path, create: bool) -> io::Result<()> {
    let mut path = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for part in parent.components() {
            path.push(part);
            if create {
                ensure_directory(&path)?;
            } else if !directory_exists(&path)? {
                break;
            }
        }
    }
    Ok(())
}

/// Restricts names to portable path components and reserves user data and updater state.
pub(super) fn validate_path(path: &Path) -> io::Result<()> {
    let invalid = || io::Error::other(format!("Invalid update path: {}", path.display()));
    for (depth, part) in path.components().enumerate() {
        let Component::Normal(name) = part else {
            return Err(invalid());
        };
        let name = name.to_str().ok_or_else(invalid)?;
        let base = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
        if depth >= 32
            || !name.is_ascii()
            || name.ends_with(['.', ' '])
            || name
                .chars()
                .any(|ch| ch.is_control() || "\\/:*?\"<>|~".contains(ch))
            || matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].iter().any(|prefix| {
                base.strip_prefix(prefix).is_some_and(|suffix| {
                    suffix.len() == 1 && suffix.as_bytes()[0].is_ascii_digit()
                })
            })
        {
            return Err(invalid());
        }
    }
    let first = path
        .iter()
        .next()
        .and_then(|name| name.to_str())
        .ok_or_else(invalid)?
        .to_ascii_lowercase();
    if first.starts_with(".update")
        || matches!(
            first.as_str(),
            "tacklecast_settings.json" | "logs" | "recordings" | "nvoffruc.dll"
        )
    {
        return Err(io::Error::other(format!(
            "Update contains protected path {}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn open_install_lock(root: &Path) -> io::Result<Option<File>> {
    if !is_portable(root) {
        return Ok(None);
    }
    let path = root.join(LOCK_FILE);
    if let Some(meta) = metadata_if_exists(&path)? {
        if !is_regular_file(&meta) {
            return Err(io::Error::other("Update lock must be a regular file"));
        }
        return OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .or_else(|err| match err.kind() {
                io::ErrorKind::PermissionDenied => File::open(&path),
                _ => Err(err),
            })
            .map(Some);
    }
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => Ok(Some(file)),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => open_install_lock(root),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

fn lock_install(file: &File, shared: bool) -> io::Result<()> {
    let result = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    match result {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Close every instance of TackleCast in this folder before applying or recovering an update",
        )),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

#[derive(Clone, Debug)]
struct Workspace {
    root: PathBuf,
    work: PathBuf,
}

impl Workspace {
    fn payload(&self) -> PathBuf {
        self.work.join("payload")
    }

    fn journal(&self) -> PathBuf {
        self.work.join(JOURNAL)
    }

    fn complete(&self) -> io::Result<bool> {
        Ok(metadata_if_exists(&self.work.join(COMPLETE))?
            .is_some_and(|meta| is_regular_file(&meta)))
    }

    fn pending(&self) -> io::Result<bool> {
        Ok(!self.complete()? && metadata_if_exists(&self.journal())?.is_some())
    }

    fn from_exe(exe: &Path) -> io::Result<Self> {
        let invalid = || io::Error::other("Run the updater helper from its staging folder");
        let payload = exe.parent().ok_or_else(invalid)?;
        let work = payload.parent().ok_or_else(invalid)?;
        let storage = work.parent().ok_or_else(invalid)?;
        let root = storage.parent().ok_or_else(invalid)?;
        if storage.file_name().is_none_or(|name| name != STAGING_ROOT)
            || payload.file_name().is_none_or(|name| name != "payload")
            || !is_workspace_name(work)
            || exe != binary_path(payload)
            || !is_portable(root)
            || !directory_exists(storage)?
            || !directory_exists(work)?
            || !directory_exists(payload)?
        {
            return Err(invalid());
        }
        Ok(Self {
            root: root.into(),
            work: work.into(),
        })
    }
}

fn is_workspace_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(STAGING_PREFIX))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}

pub(super) fn create_staging(root: &Path) -> io::Result<TempDir> {
    let storage = root.join(STAGING_ROOT);
    ensure_directory(&storage)?;
    let work = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(&storage)?;
    sync_directory(&storage)?;
    ensure_directory(&work.path().join("payload"))?;
    Ok(work)
}

fn workspaces(root: &Path) -> io::Result<Vec<Workspace>> {
    let storage = root.join(STAGING_ROOT);
    if !metadata_if_exists(&storage)?.is_some_and(|meta| meta.is_dir() && !is_link(&meta)) {
        return Ok(Vec::new());
    }
    let mut workspaces = Vec::new();
    for entry in fs::read_dir(&storage)? {
        let work = entry?.path();
        if is_workspace_name(&work)
            && metadata_if_exists(&work)?.is_some_and(|meta| meta.is_dir() && !is_link(&meta))
        {
            workspaces.push(Workspace {
                root: root.into(),
                work,
            });
        }
    }
    Ok(workspaces)
}

/// Keeps the application covered by a shared lock throughout its lifetime.
pub(super) fn startup(root: &Path) -> io::Result<Option<File>> {
    let Some(lock) = open_install_lock(root)? else {
        return Ok(None);
    };
    loop {
        lock_install(&lock, true)?;
        let mut pending = false;
        for workspace in workspaces(root)? {
            if workspace.complete()? {
                clean_completed(&workspace);
            } else if workspace.pending()? {
                pending = true;
            }
        }
        if !pending {
            return Ok(Some(lock));
        }
        lock.unlock()?;
        lock_install(&lock, false)?;
        for workspace in workspaces(root)? {
            if workspace.pending()? {
                recover(&workspace)?;
            }
        }
        lock.unlock()?;
    }
}

fn clean_completed(workspace: &Workspace) {
    let Some(name) = workspace.work.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    // Rename before deleting anything so a partial Windows cleanup cannot
    // leave behind a journal that looks like an interrupted transaction.
    let discarded = workspace.work.with_file_name(format!(".completed-{name}"));
    if fs::rename(&workspace.work, &discarded).is_ok() {
        let _ = fs::remove_dir_all(discarded);
    }
}

pub(super) fn binary_path(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    const BINARY: &str = "TackleCast.exe";
    #[cfg(not(windows))]
    const BINARY: &str = "tacklecast";
    dir.join(BINARY)
}

pub(super) fn spawn_helper(work: TempDir) -> io::Result<()> {
    let payload = work.path().join("payload");
    let mut command = Command::new(binary_path(&payload));
    command
        .args(["--apply-update", &std::process::id().to_string()])
        .current_dir(&payload);
    #[cfg(target_os = "linux")]
    command.env("LD_LIBRARY_PATH", payload.join("lib"));
    command.spawn()?;
    let _ = work.keep();
    Ok(())
}

pub(super) fn run_helper(args: &[OsString]) -> Option<i32> {
    let mode = args.first()?;
    if mode != "--apply-update" && mode != "--recover-update" {
        return None;
    }
    let result = std::env::current_exe().and_then(|exe| {
        let workspace = Workspace::from_exe(&exe)?;
        let result = if mode == "--recover-update" && args.len() == 1 {
            with_exclusive_lock(&workspace, || recover(&workspace))
        } else if mode == "--apply-update" && args.len() == 2 {
            let pid = args[1]
                .to_str()
                .and_then(|arg| arg.parse().ok())
                .filter(|&pid| pid != 0 && pid != std::process::id())
                .ok_or_else(|| io::Error::other("Missing valid parent process ID"))?;
            wait_for_parent(pid, PARENT_EXIT_TIMEOUT)
                .and_then(|()| {
                    with_exclusive_lock(&workspace, || {
                        if Some(read_version(&workspace.payload())?)
                            != SemVer::parse(current_version())
                        {
                            return Err(io::Error::other(
                                "Staged executable version does not match the package",
                            ));
                        }
                        apply(&workspace)
                    })
                })
                .and_then(|()| relaunch(&workspace.root))
        } else {
            Err(io::Error::other("Invalid updater arguments"))
        };
        if let Err(err) = &result {
            if let Ok(mut log) = File::create_new(workspace.work.join("update-error.log")) {
                let _ = writeln!(log, "{err}");
            }
        }
        result
    });
    match result {
        Ok(()) => Some(0),
        Err(err) => {
            eprintln!("TackleCast updater: {err}");
            Some(1)
        }
    }
}

fn with_exclusive_lock(
    workspace: &Workspace,
    f: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let lock = open_install_lock(&workspace.root)?
        .ok_or_else(|| io::Error::other("Cannot lock this portable install for update"))?;
    lock_install(&lock, false)?;
    f()
}

fn wait_for_parent(pid: u32, timeout: Duration) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows::core::HRESULT;
        use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        // SAFETY: A successfully opened process handle is closed after waiting.
        unsafe {
            match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                Ok(handle) => {
                    let state = WaitForSingleObject(handle, timeout.as_millis() as u32);
                    let _ = CloseHandle(handle);
                    if state == WAIT_OBJECT_0 {
                        return Ok(());
                    }
                }
                Err(err) if err.code() == HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) => {
                    return Ok(())
                }
                Err(err) => return Err(io::Error::other(err)),
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let process = PathBuf::from(format!("/proc/{pid}"));
        if !Path::new("/proc/self").is_dir() {
            return Err(io::Error::other(
                "Cannot determine if the parent process has exited",
            ));
        }
        let deadline = Instant::now() + timeout;
        while process.try_exists()? {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "TackleCast did not shut down in time",
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, timeout);
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Could not confirm that TackleCast shut down",
        ))
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Replacement {
    path: PathBuf,
    existed: bool,
}

fn apply(workspace: &Workspace) -> io::Result<()> {
    if workspace.complete()? || workspace.pending()? {
        return Err(io::Error::other(
            "This update has already been applied or needs recovery",
        ));
    }
    for other in workspaces(&workspace.root)? {
        if other.pending()? {
            return Err(io::Error::other(
                "Start TackleCast to recover the previous update first",
            ));
        }
    }
    if read_version(&workspace.payload())? <= read_version(&workspace.root)? {
        return Err(io::Error::other(
            "The staged version is no longer newer than the installed version",
        ));
    }
    let entries = prepare(workspace)?;
    let result = (|| {
        for entry in &entries {
            copy_entry(&workspace.payload(), &workspace.root, &entry.path)?;
        }
        write_atomic(&workspace.work.join(COMPLETE), b"")
    })();
    if let Err(err) = result {
        if !workspace.complete()? {
            return match recover(workspace) {
                Ok(()) => Err(err),
                Err(recovery) => Err(io::Error::other(format!(
                    "{err}. Recovery also failed: {recovery}"
                ))),
            };
        }
        return Err(err);
    }
    Ok(())
}

/// Saves every original and persists the journal before changing the install.
fn prepare(workspace: &Workspace) -> io::Result<Vec<Replacement>> {
    let payload = workspace.payload();
    let mut paths = Vec::new();
    collect_paths(&payload, Path::new(""), &mut paths)?;
    let executable = binary_path(Path::new(""));
    if !fs::symlink_metadata(payload.join(&executable)).is_ok_and(|meta| is_regular_file(&meta)) {
        return Err(io::Error::other(
            "Update is missing the application executable",
        ));
    }
    paths.sort_by(|a, b| (a == &executable, a).cmp(&(b == &executable, b)));
    let backup = workspace.work.join("backup");
    ensure_directory(&backup)?;
    let mut entries = Vec::new();
    for path in paths {
        check_parents(&workspace.root, &path, false)?;
        let existed = if let Some(meta) = metadata_if_exists(&workspace.root.join(&path))? {
            if !(is_regular_file(&meta) || cfg!(unix) && meta.is_symlink()) {
                return Err(io::Error::other(format!(
                    "Cannot replace {}",
                    path.display()
                )));
            }
            #[cfg(windows)]
            if meta.permissions().readonly() {
                return Err(io::Error::other(format!(
                    "Cannot replace read-only file {}",
                    path.display()
                )));
            }
            copy_entry(&workspace.root, &backup, &path)?;
            true
        } else {
            false
        };
        entries.push(Replacement { path, existed });
    }
    let journal = serde_json::to_vec(&entries)?;
    write_atomic(&workspace.journal(), &journal)?;
    Ok(entries)
}

fn collect_paths(root: &Path, relative: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let relative = relative.join(entry.file_name());
        validate_path(&relative)?;
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_dir() && !is_link(&meta) {
            collect_paths(root, &relative, paths)?;
        } else if is_regular_file(&meta) || (cfg!(unix) && meta.is_symlink()) {
            paths.push(relative);
        } else {
            return Err(io::Error::other(format!(
                "Unsupported update file {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

fn recover(workspace: &Workspace) -> io::Result<()> {
    if !workspace.pending()? {
        return Ok(());
    }
    let journal = workspace.journal();
    if !is_regular_file(&fs::symlink_metadata(&journal)?) {
        return Err(io::Error::other("Invalid update recovery journal"));
    }
    let entries: Vec<Replacement> = serde_json::from_reader(File::open(&journal)?)?;
    let backup = workspace.work.join("backup");
    if !directory_exists(&backup)? {
        return Err(io::Error::other("Update backup directory is missing"));
    }
    for entry in entries {
        validate_path(&entry.path)?;
        check_parents(&workspace.root, &entry.path, false)?;
        if entry.existed {
            check_parents(&backup, &entry.path, false)?;
            if !same_entry(&backup.join(&entry.path), &workspace.root.join(&entry.path))? {
                copy_entry(&backup, &workspace.root, &entry.path)?;
            }
        } else {
            let dest = workspace.root.join(&entry.path);
            if metadata_if_exists(&dest)?.is_some() {
                fs::remove_file(&dest)?;
                sync_directory(dest.parent().expect("update file has a parent"))?;
            }
        }
    }
    write_atomic(&workspace.work.join(COMPLETE), b"")
}

fn same_entry(first: &Path, second: &Path) -> io::Result<bool> {
    let first_meta = fs::symlink_metadata(first)?;
    let Some(second_meta) = metadata_if_exists(second)? else {
        return Ok(false);
    };
    if first_meta.is_symlink() && second_meta.is_symlink() {
        return Ok(fs::read_link(first)? == fs::read_link(second)?);
    }
    if !is_regular_file(&first_meta)
        || !is_regular_file(&second_meta)
        || first_meta.len() != second_meta.len()
        || first_meta.permissions() != second_meta.permissions()
    {
        return Ok(false);
    }
    let mut first = BufReader::new(File::open(first)?);
    let mut second = BufReader::new(File::open(second)?);
    loop {
        let left = first.fill_buf()?;
        let right = second.fill_buf()?;
        let count = left.len().min(right.len());
        if count == 0 {
            return Ok(left.is_empty() && right.is_empty());
        }
        if left[..count] != right[..count] {
            return Ok(false);
        }
        first.consume(count);
        second.consume(count);
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().expect("update file has a parent");
    let mut temp = NamedTempFile::new_in(dir)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    sync_directory(dir)
}

fn copy_entry(source: &Path, dest: &Path, relative: &Path) -> io::Result<()> {
    check_parents(source, relative, false)?;
    check_parents(dest, relative, true)?;
    let source = source.join(relative);
    let dest = dest.join(relative);
    let dir = dest.parent().expect("update file has a parent");
    let meta = fs::symlink_metadata(&source)?;
    #[cfg(unix)]
    if meta.is_symlink() {
        let temp = tempfile::tempdir_in(dir)?;
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(fs::read_link(&source)?, &link)?;
        fs::rename(link, &dest)?;
        return sync_directory(dir);
    }
    if !is_regular_file(&meta) {
        return Err(io::Error::other(format!(
            "Expected a regular file at {}",
            source.display()
        )));
    }
    let mut temp = NamedTempFile::new_in(dir)?;
    io::copy(&mut File::open(&source)?, temp.as_file_mut())?;
    temp.as_file().set_permissions(meta.permissions())?;
    temp.as_file().sync_all()?;
    temp.persist(&dest)?;
    sync_directory(dir)
}

fn sync_directory(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn relaunch(root: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let launcher = root.join("run.sh");
        if fs::symlink_metadata(&launcher).is_ok_and(|meta| is_regular_file(&meta)) {
            return Command::new(launcher)
                .current_dir(root)
                .env_remove("LD_LIBRARY_PATH")
                .spawn()
                .map(|_| ());
        }
        Command::new(binary_path(root))
            .current_dir(root)
            .env("LD_LIBRARY_PATH", root.join("lib"))
            .spawn()
            .map(|_| ())
    }
    #[cfg(not(target_os = "linux"))]
    Command::new(binary_path(root))
        .current_dir(root)
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_install() -> (TempDir, TempDir, Workspace) {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("build-info.json"),
            br#"{"version":"3.0.0"}"#,
        )
        .unwrap();
        fs::write(binary_path(root.path()), b"old application").unwrap();
        let stage = create_staging(root.path()).unwrap();
        let workspace = Workspace::from_exe(&binary_path(&stage.path().join("payload"))).unwrap();
        fs::write(
            workspace.payload().join("build-info.json"),
            br#"{"version":"3.0.1"}"#,
        )
        .unwrap();
        fs::write(binary_path(&workspace.payload()), b"new application").unwrap();
        (root, stage, workspace)
    }

    #[test]
    fn overlay_updates_the_app_and_keeps_user_data() {
        let (root, _stage, workspace) = test_install();
        fs::create_dir(root.path().join("logs")).unwrap();
        for file in [
            "tacklecast_settings.json",
            "logs/session.log",
            "NvOFFRUC.dll",
            "notes.txt",
        ] {
            fs::write(root.path().join(file), b"user data").unwrap();
        }
        apply(&workspace).unwrap();
        assert!(workspace.complete().unwrap());
        assert_eq!(
            fs::read(binary_path(root.path())).unwrap(),
            b"new application"
        );
        for file in [
            "tacklecast_settings.json",
            "logs/session.log",
            "NvOFFRUC.dll",
            "notes.txt",
        ] {
            assert_eq!(fs::read(root.path().join(file)).unwrap(), b"user data");
        }
        let _lock = startup(root.path()).unwrap().unwrap();
        assert!(!workspace.work.exists());
        assert_eq!(
            fs::read(binary_path(root.path())).unwrap(),
            b"new application"
        );
    }

    #[test]
    fn startup_recovers_after_partial_replacement() {
        let (root, _stage, workspace) = test_install();
        let payload = workspace.payload();
        fs::write(payload.join("new.dll"), b"new file").unwrap();
        let entries = prepare(&workspace).unwrap();
        assert!(workspace.pending().unwrap());
        for entry in entries {
            copy_entry(&payload, root.path(), &entry.path).unwrap();
        }
        // No completion marker simulates an interruption after the writes.
        assert_eq!(
            fs::read(binary_path(root.path())).unwrap(),
            b"new application"
        );
        let _lock = startup(root.path()).unwrap().unwrap();
        assert_eq!(
            fs::read(binary_path(root.path())).unwrap(),
            b"old application"
        );
        assert_eq!(read_version(root.path()).unwrap().to_string(), "3.0.0");
        assert!(!root.path().join("new.dll").exists());
        assert!(!workspace.work.exists());
    }

    #[test]
    fn failed_recovery_keeps_the_journal_and_backup_for_retry() {
        let (root, _stage, workspace) = test_install();
        let entries = prepare(&workspace).unwrap();
        for entry in entries {
            copy_entry(&workspace.payload(), root.path(), &entry.path).unwrap();
        }
        let executable = binary_path(root.path());
        fs::remove_file(&executable).unwrap();
        fs::create_dir(&executable).unwrap();
        assert!(startup(root.path()).is_err());
        assert!(workspace.pending().unwrap());
        assert_eq!(
            fs::read(binary_path(&workspace.work.join("backup"))).unwrap(),
            b"old application"
        );
        fs::remove_dir(&executable).unwrap();
        let _lock = startup(root.path()).unwrap().unwrap();
        assert_eq!(fs::read(&executable).unwrap(), b"old application");
    }

    #[test]
    fn another_instance_blocks_apply_without_discarding_staging() {
        let (root, _stage, workspace) = test_install();
        let first = startup(root.path()).unwrap().unwrap();
        let second = startup(root.path()).unwrap().unwrap();
        assert!(workspace.payload().exists());
        drop(first);
        let error = with_exclusive_lock(&workspace, || apply(&workspace)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(workspace.payload().exists());
        assert!(!workspace.pending().unwrap());
        drop(second);
        with_exclusive_lock(&workspace, || apply(&workspace)).unwrap();
        assert!(workspace.complete().unwrap());
    }

    #[test]
    fn path_validation_reserves_user_data_and_update_state() {
        for path in [
            "TackleCast_Settings.json",
            "logs/test.log",
            "recordings/clip",
            "NvOFFRUC.dll",
            ".update.lock",
            ".update-staging/test",
            "lib/NUL.txt",
            "lib/trailing.",
            "lib/FILE~1.TXT",
        ] {
            assert!(validate_path(Path::new(path)).is_err(), "{path}");
        }
        assert!(validate_path(Path::new("lib/avcodec-62.dll")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn overlay_refuses_a_symlinked_install_parent_before_replacement() {
        let (root, _stage, workspace) = test_install();
        let external = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(external.path(), root.path().join("lib")).unwrap();
        fs::create_dir(workspace.payload().join("lib")).unwrap();
        fs::write(workspace.payload().join("lib/dependency.so"), b"dependency").unwrap();
        assert!(apply(&workspace).is_err());
        assert!(!workspace.pending().unwrap());
        assert_eq!(
            fs::read(binary_path(root.path())).unwrap(),
            b"old application"
        );
        assert_eq!(fs::read_dir(external.path()).unwrap().count(), 0);
    }

    #[test]
    fn refuses_an_update_that_is_no_longer_newer() {
        let (root, _stage, workspace) = test_install();
        fs::write(
            root.path().join("build-info.json"),
            br#"{"version":"3.1.0"}"#,
        )
        .unwrap();
        assert!(apply(&workspace).is_err());
        assert!(!workspace.pending().unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parent_timeout_prevents_applying() {
        let error = wait_for_parent(std::process::id(), Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
