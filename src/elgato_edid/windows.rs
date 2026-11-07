//! Windows side of the Elgato 4K X controls.
//!
//! Elgato's `EGAVDeviceSupport.dll` only ever loads in a short-lived child
//! process (`TackleCast.exe --elgato-edid ...`). Loading it changes the
//! process-wide DLL search path, brings Elgato's own USB libraries and CRT with
//! it, and prints to the console. None of that should touch the viewer, which
//! loads FFmpeg, CUDA and the MF bridge DLLs itself.

use super::assets;
use super::egavds::{EdidInfo, EdidType, Library, Paths};
use super::win;
use super::{validate_edid, Job, Profile, SavedEdid, SettingChange, Status};
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HELPER_FLAG: &str = "--elgato-edid";
const STUDIO_FAMILY: &str = "Elgato.Studio_g54w8ztgkx496";
/// Studio's device library and the files it loads at runtime.
const STUDIO_LIB_FILES: &[&str] = &[
    "EGAVDeviceSupport.dll",
    "RTICE_SDK_x64.dll",
    "RTK_IO_x64.dll",
    "ElgatoDeviceCapabilities.json",
];
/// Processes that hold the card open and would fight over it during a change.
const CONFLICTING_PROCESSES: &[&str] = &["elgato.studio.exe", "4kcaptureutility.exe"];
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const STATUS_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Parent side
// ---------------------------------------------------------------------------

/// Read the card's active EDID, its EDID choices and its settings.
pub fn query(device_path: &str) -> Result<Status, String> {
    run_helper(&["status", device_path], Some(STATUS_TIMEOUT))
}

/// Make a change on the card. This never kills the helper: interrupting an
/// EDID write while the card is re-flashing is the one thing that could leave
/// it in a bad state, so the parent simply waits for the helper to finish.
pub fn run_job(device_path: &str, job: &Job) -> Result<Status, String> {
    match job {
        Job::Profile(name) => run_helper(&["profile", device_path, name], None),
        Job::File(path) => {
            let path = path
                .to_str()
                .ok_or("The EDID file's path isn't valid text")?;
            run_helper(&["file", device_path, path], None)
        }
        Job::Setting(change) => {
            let json = serde_json::to_string(change).map_err(|e| e.to_string())?;
            run_helper(&["setting", device_path, &json], None)
        }
    }
}

fn run_helper(args: &[&str], timeout: Option<Duration>) -> Result<Status, String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = Command::new(executable)
        .arg(HELPER_FLAG)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Cannot start the Elgato helper: {e}"))?;
    let output = child
        .stdout
        .take()
        .ok_or("Elgato helper output is unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        output
            .take(4 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if timeout.is_none_or(|limit| started.elapsed() < limit) => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Ok(None) => {
                // Only read-only queries have a timeout.
                let _ = child.kill();
                let _ = child.wait();
                return Err(
                    "Reading the card timed out. Check that Elgato Studio is closed, then refresh."
                        .into(),
                );
            }
            Err(error) => return Err(format!("Cannot wait for the Elgato helper: {error}")),
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| "Elgato helper reader failed")?
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    // The result is the last line. Anything before it is stray library output.
    let Some(line) = text.lines().rev().find(|line| !line.trim().is_empty()) else {
        return Err(if status.success() {
            "The Elgato helper returned nothing".into()
        } else {
            format!("The Elgato library crashed ({status}). Check that Elgato Studio is closed, then refresh.")
        });
    };
    serde_json::from_str::<Result<Status, String>>(line)
        .map_err(|e| format!("Invalid Elgato helper result: {e}"))?
}

/// Show the Windows "Open" dialog for an EDID file. Blocks until it closes.
pub fn pick_file(owner: isize) -> Option<PathBuf> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    let filter: Vec<u16> = "EDID files (*.bin)\0*.bin\0All files (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title = win::wide("Load EDID file");
    let mut file = vec![0u16; 4096];
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: HWND(owner as *mut std::ffi::c_void),
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(file.as_mut_ptr()),
        nMaxFile: file.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    if !unsafe { GetOpenFileNameW(&mut dialog) }.as_bool() {
        return None;
    }
    let end = file.iter().position(|&c| c == 0).unwrap_or(file.len());
    Some(PathBuf::from(String::from_utf16_lossy(&file[..end])))
}

// ---------------------------------------------------------------------------
// Helper process
// ---------------------------------------------------------------------------

/// Handle `--elgato-edid` before the app initializes anything. Returns `true`
/// when this process was the helper and should exit.
pub fn handle_helper_args() -> bool {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some(HELPER_FLAG) {
        return false;
    }
    // A fault inside the vendor library belongs in the parent's error message,
    // not a blocking crash dialog.
    unsafe {
        use windows::Win32::System::Diagnostics::Debug::{
            SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX,
        };
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    }
    let command = args.next().unwrap_or_default();
    if command == "selftest" {
        std::process::exit(selftest());
    }
    let device = args.next().unwrap_or_default();
    let argument = args.next();
    let request = match (command.as_str(), argument) {
        _ if device.is_empty() => Err("Missing capture device".to_string()),
        ("status", _) => Ok(None),
        ("profile", Some(name)) => Ok(Some(Job::Profile(name))),
        ("file", Some(path)) => Ok(Some(Job::File(PathBuf::from(path)))),
        ("setting", Some(json)) => serde_json::from_str::<SettingChange>(&json)
            .map(|change| Some(Job::Setting(change)))
            .map_err(|e| format!("Invalid setting: {e}")),
        (other, _) => Err(format!("Invalid helper command '{other}'")),
    };
    let result = request.and_then(|job| run(&device, job));
    if let Ok(json) = serde_json::to_string(&result) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "\n{json}");
        let _ = out.flush();
    }
    true
}

/// Find the installed Elgato Studio and copy its device library next to our
/// data. Returns Studio's install folder and our paths.
fn prepare() -> Result<(PathBuf, Paths), String> {
    let studio_dir = win::package_path_for_family(STUDIO_FAMILY)
        .filter(|dir| dir.join("EGAVDeviceSupport.dll").is_file())
        .ok_or(
            "Elgato Studio is not installed. TackleCast uses Studio's device library for the \
             card's settings, so install Studio.",
        )?;
    let data_dir = data_dir()?;
    let paths = Paths {
        lib_dir: data_dir
            .join("lib")
            .join(studio_dir.file_name().unwrap_or_default()),
        edid_dir: data_dir.join("EDIDFiles"),
        data_dir,
    };
    copy_studio_libs(&studio_dir, &paths.lib_dir)?;
    Ok((studio_dir, paths))
}

fn studio_temp_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Packages")
        .join(STUDIO_FAMILY)
        .join("TempState")
}

fn run(device_path: &str, job: Option<Job>) -> Result<Status, String> {
    let (_, paths) = prepare()?;
    let studio_running = studio_running();
    if job.is_some() && studio_running {
        return Err(
            "Elgato Studio is running and holds the card. Close it, then try again.".into(),
        );
    }
    // Writes skip the network: the status read that opened the menu already
    // brought the EDID files up to date.
    let assets = if job.is_none() {
        assets::sync(&paths.data_dir, &paths.edid_dir, &studio_temp_dir(), false)?
    } else {
        let manifest = std::fs::read(paths.data_dir.join(assets::MANIFEST_FILE))
            .map_err(|_| "Elgato's EDID list isn't on this PC yet; refresh the menu first")?;
        assets::Assets {
            manifest,
            source: String::new(),
            edids_needed: 0,
            edids_ok: 0,
            note: None,
        }
    };
    run_on_card(device_path, job, &paths, &assets.manifest, studio_running).map(|mut status| {
        status.asset_note = assets.note;
        status
    })
}

fn run_on_card(
    device_path: &str,
    job: Option<Job>,
    paths: &Paths,
    manifest: &[u8],
    studio_running: bool,
) -> Result<Status, String> {
    let lib = Library::init(paths, Some(manifest))?;
    let profiles = {
        let dev = lib.open(device_path)?;
        if !dev.supports_edid() {
            return Err("This card does not support EDID changes".into());
        }
        let mut all = vec![dev.default_edid()?];
        all.extend(dev.custom_edids()?);
        let file = match job {
            None => return status(&dev, &all, paths, studio_running),
            Some(Job::Setting(change)) => {
                dev.apply(&change)?;
                return status(&dev, &all, paths, false);
            }
            Some(Job::Profile(name)) => {
                let entry = all
                    .iter()
                    .find(|e| e.name == name)
                    .ok_or_else(|| format!("No EDID profile named \"{name}\" for this card"))?;
                let current = dev.current_edid().ok();
                if current.as_ref().is_some_and(|c| same_edid(c, entry)) {
                    return status(&dev, &all, paths, false);
                }
                verified_profile_file(entry)?
            }
            Some(Job::File(path)) => store_user_edid(paths, &path)?,
        };
        dev.set_edid(&file)?;
        all
    };
    // The card re-enumerated during the write. Re-open it and report.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_error = String::new();
    while Instant::now() < deadline {
        match lib.open(device_path) {
            Ok(dev) => return status(&dev, &profiles, paths, false),
            Err(error) => last_error = error,
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(format!(
        "The write finished, but the card could not be re-read within 30 s: {last_error}"
    ))
}

fn status(
    dev: &super::egavds::Device<'_>,
    all: &[EdidInfo],
    paths: &Paths,
    studio_running: bool,
) -> Result<Status, String> {
    let current = dev.current_edid()?;
    let profile = |e: &EdidInfo| Profile {
        name: if e.name.is_empty() {
            "(unnamed)".into()
        } else {
            e.name.clone()
        },
        kind: e.edid_type.label().into(),
        available: !e.download_required && Path::new(&e.file_path).is_file(),
    };
    Ok(Status {
        active: profile(&current),
        profiles: all
            .iter()
            .filter(|e| matches!(e.edid_type, EdidType::Default | EdidType::Custom))
            .map(profile)
            .collect(),
        saved_files: saved_edids(paths),
        settings: dev.settings(),
        signal: dev.signal(),
        asset_note: None,
        studio_running,
    })
}

fn same_edid(a: &EdidInfo, b: &EdidInfo) -> bool {
    !a.name.is_empty() && a.name == b.name && a.edid_type == b.edid_type
}

/// The EDID file for an Elgato profile, after checking it is the one Elgato's
/// manifest lists.
fn verified_profile_file(entry: &EdidInfo) -> Result<PathBuf, String> {
    let file = PathBuf::from(&entry.file_path);
    if entry.file_path.is_empty() || !file.is_file() {
        return Err(format!(
            "The EDID file for \"{}\" isn't on this PC. Open Elgato Studio once so it downloads it.",
            entry.name
        ));
    }
    if entry.sha256.is_empty() {
        return Err(format!(
            "Elgato's EDID list has no checksum for \"{}\"; refusing to write it",
            entry.name
        ));
    }
    let data = std::fs::read(&file).map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
    let actual = win::sha256_hex(&data)?;
    if !actual.eq_ignore_ascii_case(&entry.sha256) {
        return Err(format!(
            "Refusing to write \"{}\": the file doesn't match Elgato's checksum. Open Elgato \
             Studio once to download it again.",
            entry.name
        ));
    }
    Ok(file)
}

/// Validate an EDID file and keep a copy under TackleCast's data folder, with
/// a plain-ASCII name the Elgato library accepts. Returns the copy's path.
fn store_user_edid(paths: &Paths, source: &Path) -> Result<PathBuf, String> {
    let bytes =
        std::fs::read(source).map_err(|e| format!("Cannot read {}: {e}", source.display()))?;
    validate_edid(&bytes)?;
    let dir = user_edid_dir(paths);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let stem: String = source
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_. ".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = if stem.trim().is_empty() {
        "edid".to_string()
    } else {
        stem
    };
    let mut target = dir.join(format!("{stem}.bin"));
    if std::fs::read(&target).is_ok_and(|existing| existing != bytes) {
        let hash = win::sha256_hex(&bytes)?;
        target = dir.join(format!("{stem}-{}.bin", &hash[..8]));
    }
    if std::fs::read(&target).ok().as_deref() != Some(&bytes[..]) {
        std::fs::write(&target, &bytes)
            .map_err(|e| format!("Cannot write {}: {e}", target.display()))?;
    }
    Ok(target)
}

fn user_edid_dir(paths: &Paths) -> PathBuf {
    paths.data_dir.join("user-edids")
}

/// Valid EDID files the menu can offer: those loaded through TackleCast before
/// and those Elgato Studio saved (imported files and monitor copies).
fn saved_edids(paths: &Paths) -> Vec<SavedEdid> {
    let studio = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Packages")
        .join(STUDIO_FAMILY)
        .join("LocalState")
        .join("EDID");
    let mut out = Vec::new();
    for (dir, suffix) in [(user_edid_dir(paths), ""), (studio, " (Elgato Studio)")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("bin")))
            .filter(|p| std::fs::read(p).is_ok_and(|bytes| validate_edid(&bytes).is_ok()))
            .collect();
        files.sort();
        out.extend(files.into_iter().map(|path| SavedEdid {
            name: format!(
                "{}{suffix}",
                path.file_stem().unwrap_or_default().to_string_lossy()
            ),
            path,
        }));
    }
    out
}

fn data_dir() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(|dir| PathBuf::from(dir).join("TackleCast").join("elgato-edid"))
        .ok_or_else(|| "LOCALAPPDATA is not set".into())
}

fn studio_running() -> bool {
    let running = win::running_process_names();
    CONFLICTING_PROCESSES
        .iter()
        .any(|p| running.iter().any(|r| r == p))
}

fn copy_studio_libs(studio_dir: &Path, lib_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(lib_dir)
        .map_err(|e| format!("Cannot create {}: {e}", lib_dir.display()))?;
    for name in STUDIO_LIB_FILES {
        let src = studio_dir.join(name);
        if src.is_file() {
            copy_if_changed(&src, &lib_dir.join(name))?;
        } else if *name == "EGAVDeviceSupport.dll" {
            return Err(format!("{} not found", src.display()));
        }
    }
    Ok(())
}

/// `TackleCast.exe --elgato-edid selftest`: check, without touching any card,
/// that the installed Elgato Studio's library still has every function
/// TackleCast calls and that Elgato's EDID list and files are available.
/// Prints a report (pipe it, e.g. `| Out-String`, since TackleCast is a GUI
/// app), saves it as `selftest.txt`, and returns the exit code.
fn selftest() -> i32 {
    let mut lines = Vec::new();
    let mut ok = true;
    let mut check = |passed: bool, text: String| {
        ok &= passed;
        lines.push(format!("{} {text}", if passed { "OK  " } else { "FAIL" }));
    };
    match prepare() {
        Err(error) => check(false, error),
        Ok((studio_dir, paths)) => {
            check(
                true,
                format!(
                    "Elgato Studio found: {}",
                    studio_dir.file_name().unwrap_or_default().to_string_lossy()
                ),
            );
            match super::egavds::missing_exports(&paths.lib_dir) {
                Ok(missing) if missing.is_empty() => check(
                    true,
                    "EGAVDeviceSupport.dll has every function TackleCast uses".into(),
                ),
                Ok(missing) => check(
                    false,
                    format!(
                        "EGAVDeviceSupport.dll is missing {} function(s) TackleCast uses: {}",
                        missing.len(),
                        missing.join(", ")
                    ),
                ),
                Err(error) => check(false, error),
            }
            match assets::sync(&paths.data_dir, &paths.edid_dir, &studio_temp_dir(), true) {
                Ok(found) => {
                    check(true, format!("Elgato's EDID list: {}", found.source));
                    check(
                        found.edids_needed > 0 && found.edids_ok == found.edids_needed,
                        format!(
                            "4K X EDID files present and verified: {} of {}",
                            found.edids_ok, found.edids_needed
                        ),
                    );
                    if let Some(note) = found.note {
                        lines.push(format!("NOTE {note}"));
                    }
                }
                Err(error) => check(false, error),
            }
            let report = lines.join("\r\n") + "\r\n";
            let _ = std::fs::write(paths.data_dir.join("selftest.txt"), &report);
        }
    }
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    for line in &lines {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(
        out,
        "{}",
        if ok {
            "Self-test passed."
        } else {
            "Self-test FAILED."
        }
    );
    let _ = out.flush();
    if ok {
        0
    } else {
        1
    }
}

fn copy_if_changed(src: &Path, dst: &Path) -> Result<(), String> {
    let Ok(new) = std::fs::read(src) else {
        return Ok(());
    };
    if std::fs::read(dst).ok().as_deref() != Some(&new[..]) {
        std::fs::write(dst, &new).map_err(|e| format!("Cannot write {}: {e}", dst.display()))?;
    }
    Ok(())
}
