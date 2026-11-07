//! Windows side of Elgato EDID switching.
//!
//! Elgato's `EGAVDeviceSupport.dll` only ever loads in a short-lived child
//! process (`TackleCast.exe --elgato-edid ...`). Loading it changes the
//! process-wide DLL search path, brings Elgato's own USB libraries and CRT with
//! it, and prints to the console. None of that should touch the viewer, which
//! loads FFmpeg, CUDA and the MF bridge DLLs itself.

use super::egavds::{self, EdidInfo, EdidType, Library, Paths};
use super::win;
use super::{Profile, Status};
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
/// Processes that hold the card open and would fight over it during a write.
const CONFLICTING_PROCESSES: &[&str] = &["elgato.studio.exe", "4kcaptureutility.exe"];
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const STATUS_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Parent side
// ---------------------------------------------------------------------------

/// Read the card's active EDID and its profile list.
pub fn query(device_path: &str) -> Result<Status, String> {
    run_helper(&["status", device_path], Some(STATUS_TIMEOUT))
}

/// Write a profile to the card. This never kills the helper: interrupting an
/// EDID write while the card is re-flashing is the one thing that could leave
/// it in a bad state, so the parent simply waits for the helper to finish.
pub fn write(device_path: &str, profile: &str) -> Result<Status, String> {
    run_helper(&["set", device_path, profile], None)
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
        .map_err(|e| format!("Cannot start the Elgato EDID helper: {e}"))?;
    let output = child
        .stdout
        .take()
        .ok_or("Elgato EDID helper output is unavailable")?;
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
                    "Reading the card's EDID timed out. Check that Elgato Studio is closed, then refresh."
                        .into(),
                );
            }
            Err(error) => return Err(format!("Cannot wait for the Elgato EDID helper: {error}")),
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| "Elgato EDID helper reader failed")?
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    // The result is the last line. Anything before it is stray library output.
    let Some(line) = text.lines().rev().find(|line| !line.trim().is_empty()) else {
        return Err(if status.success() {
            "The Elgato EDID helper returned nothing".into()
        } else {
            format!("The Elgato library crashed ({status}). Check that Elgato Studio is closed, then refresh.")
        });
    };
    serde_json::from_str::<Result<Status, String>>(line)
        .map_err(|e| format!("Invalid Elgato EDID helper result: {e}"))?
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
    let device = args.next().unwrap_or_default();
    let result = match command.as_str() {
        _ if device.is_empty() => Err("Missing capture device".to_string()),
        "status" => run(&device, None),
        "set" => match args.next() {
            Some(profile) => run(&device, Some(&profile)),
            None => Err("Missing EDID profile".to_string()),
        },
        other => Err(format!("Unknown EDID helper command '{other}'")),
    };
    if let Ok(json) = serde_json::to_string(&result) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "\n{json}");
        let _ = out.flush();
    }
    true
}

fn run(device_path: &str, target: Option<&str>) -> Result<Status, String> {
    let studio_dir = win::package_path_for_family(STUDIO_FAMILY)
        .filter(|dir| dir.join("EGAVDeviceSupport.dll").is_file())
        .ok_or(
            "Elgato Studio is not installed. TackleCast uses Studio's device library to change \
             the EDID, so install Studio and open it once.",
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

    let studio_running = studio_running();
    if target.is_some() && studio_running {
        return Err(
            "Elgato Studio is running and holds the card. Close it, then try again.".into(),
        );
    }
    let manifest = sync_from_studio(&paths)?.ok_or(
        "Elgato's EDID list isn't on this PC yet. Open Elgato Studio once so it downloads it.",
    )?;

    let lib = Library::init(&paths, Some(&manifest))?;
    let profiles = {
        let dev = lib.open(device_path)?;
        if !dev.supports_edid() {
            return Err("This card does not support EDID changes".into());
        }
        let mut all = vec![dev.default_edid()?];
        all.extend(dev.custom_edids()?);
        let Some(name) = target else {
            return status(&dev, &all, studio_running);
        };
        let entry = all
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| format!("No EDID profile named \"{name}\" for this card"))?
            .clone();
        let current = dev.current_edid().ok();
        if current.as_ref().is_some_and(|c| same_edid(c, &entry)) {
            return status(&dev, &all, false);
        }
        let file = verified_file(&entry)?;
        dev.set_edid(&file)?;
        all
    };
    // The card re-enumerated during the write. Re-open it and confirm.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_error = String::new();
    while Instant::now() < deadline {
        match lib.open(device_path) {
            Ok(dev) => {
                let result = status(&dev, &profiles, false)?;
                return if result.active.name == target.unwrap_or_default() {
                    Ok(result)
                } else {
                    Err(format!(
                        "The write finished, but the card reports \"{}\" instead of \"{}\"",
                        result.active.name,
                        target.unwrap_or_default()
                    ))
                };
            }
            Err(error) => last_error = error,
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(format!(
        "The write finished, but the card could not be re-read within 30 s: {last_error}"
    ))
}

fn status(
    dev: &egavds::Device<'_>,
    all: &[EdidInfo],
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
        source: dev
            .edid_mode()
            .map(|m| egavds::edid_mode_name(m).to_string()),
        active: profile(&current),
        profiles: all
            .iter()
            .filter(|e| matches!(e.edid_type, EdidType::Default | EdidType::Custom))
            .map(profile)
            .collect(),
        studio_running,
    })
}

fn same_edid(a: &EdidInfo, b: &EdidInfo) -> bool {
    !a.name.is_empty() && a.name == b.name && a.edid_type == b.edid_type
}

/// The EDID file on disk, after checking it is the one Elgato's manifest lists.
fn verified_file(entry: &EdidInfo) -> Result<PathBuf, String> {
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

/// Copy Studio's downloaded EDID files and asset manifest into our own folder
/// (Studio keeps them in TempState, which Windows may clean up) and return the
/// manifest JSON.
fn sync_from_studio(paths: &Paths) -> Result<Option<Vec<u8>>, String> {
    std::fs::create_dir_all(&paths.edid_dir)
        .map_err(|e| format!("Cannot create {}: {e}", paths.edid_dir.display()))?;
    let studio_tmp = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Packages")
        .join(STUDIO_FAMILY)
        .join("TempState");
    let ours = paths.data_dir.join("device-asset-manifest.json");
    copy_if_changed(&studio_tmp.join("device-asset-manifest.json"), &ours)?;
    if let Ok(entries) = std::fs::read_dir(studio_tmp.join("EDIDFiles")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("bin"))
            {
                copy_if_changed(&path, &paths.edid_dir.join(entry.file_name()))?;
            }
        }
    }
    Ok(std::fs::read(&ours).ok())
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
