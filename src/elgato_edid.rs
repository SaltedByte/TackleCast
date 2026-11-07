//! Switch the EDID of an Elgato 4K X from the settings menu.
//!
//! Elgato Studio doesn't toggle a preset on the 4K X: it writes a whole EDID
//! file into the card's flash, and the card disconnects and reconnects while it
//! does (about 20 s). TackleCast drives the same code path through Studio's own
//! `EGAVDeviceSupport.dll`, so Studio must be installed (and closed while
//! switching). The DLL only loads in a helper process; see `elgato_edid/windows.rs`.
//!
//! Reads and writes run on background threads. The UI polls [`view`] each frame
//! and the main loop collects a finished write with [`take_finished_write`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(all(target_os = "windows", feature = "elgato-edid"))]
#[path = "elgato_edid/egavds.rs"]
mod egavds;
#[cfg(all(target_os = "windows", feature = "elgato-edid"))]
#[path = "elgato_edid/windows.rs"]
mod platform;
#[cfg(all(target_os = "windows", feature = "elgato-edid"))]
#[path = "elgato_edid/win.rs"]
mod win;

const ELGATO_VID: &str = "vid_0fd9";
/// The 4K X enumerates with a different product ID per USB link speed.
const PIDS_4KX: &[&str] = &["pid_009b", "pid_009c", "pid_009d"];
const DSHOW_PNP_PREFIX: &str = "@device_pnp_";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Profile {
    pub name: String,
    /// "default", "custom", "user file" or "pass-through monitor".
    pub kind: String,
    /// The EDID file has been downloaded, so the profile can be written.
    pub available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Status {
    /// The card's EDID source setting, e.g. "Internal".
    pub source: Option<String>,
    pub active: Profile,
    pub profiles: Vec<Profile>,
    /// Elgato Studio is running, so writes are refused.
    pub studio_running: bool,
}

pub enum View {
    Loading,
    Error(String),
    Ready(Arc<Status>),
    Writing { target: String, elapsed: Duration },
}

/// The outcome of a write, for the main loop.
pub struct FinishedWrite {
    pub target: String,
    pub result: Result<(), String>,
}

#[derive(Clone)]
enum Entry {
    Loading,
    Done(Result<Arc<Status>, String>),
}

struct WriteJob {
    device: String,
    target: String,
    started: Instant,
    result: Option<Result<Status, String>>,
}

#[derive(Default)]
struct State {
    cache: HashMap<String, Entry>,
    write: Option<WriteJob>,
    /// Result of the last write per device, shown in the menu until the next one.
    notices: HashMap<String, Result<String, String>>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn lock_state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The KS device interface path inside a DirectShow device ID, when the device
/// is an Elgato 4K X. DirectShow IDs look like
/// `@device_pnp_\\?\usb#vid_0fd9&pid_009b&mi_00#...#{65e8773d-...}\global`
/// (TackleCast stores them with `:` replaced by `_`).
pub fn interface_path(device_id: &str) -> Option<String> {
    let path = device_id.strip_prefix(DSHOW_PNP_PREFIX)?;
    let lower = path.to_ascii_lowercase();
    let is_4kx = lower.starts_with(r"\\?\usb#")
        && lower.contains(ELGATO_VID)
        && lower.contains("&mi_00")
        && PIDS_4KX.iter().any(|pid| lower.contains(pid));
    is_4kx.then(|| path.to_string())
}

pub fn is_supported_device(device_id: &str) -> bool {
    cfg!(all(target_os = "windows", feature = "elgato-edid")) && interface_path(device_id).is_some()
}

/// What the menu should show for this device, starting a read if needed.
/// `None` means the device has no Elgato EDID support.
pub fn view(device_id: &str) -> Option<View> {
    if !is_supported_device(device_id) {
        return None;
    }
    let mut state = lock_state();
    if let Some(job) = state.write.as_ref().filter(|job| job.result.is_none()) {
        if job.device == device_id {
            return Some(View::Writing {
                target: job.target.clone(),
                elapsed: job.started.elapsed(),
            });
        }
    }
    match state.cache.get(device_id).cloned() {
        Some(Entry::Loading) => Some(View::Loading),
        Some(Entry::Done(Ok(status))) => Some(View::Ready(status)),
        Some(Entry::Done(Err(message))) => Some(View::Error(message)),
        None => {
            state.cache.insert(device_id.to_string(), Entry::Loading);
            let device = device_id.to_string();
            std::thread::spawn(move || {
                let result = query(&device).map(Arc::new);
                lock_state().cache.insert(device, Entry::Done(result));
            });
            Some(View::Loading)
        }
    }
}

/// Forget the cached status so the next [`view`] reads the card again.
pub fn refresh(device_id: &str) {
    let mut state = lock_state();
    if !matches!(state.cache.get(device_id), Some(Entry::Loading)) {
        state.cache.remove(device_id);
    }
}

/// The message from the last write to this device, if any.
pub fn notice(device_id: &str) -> Option<Result<String, String>> {
    lock_state().notices.get(device_id).cloned()
}

/// Start writing `profile` to the card. Returns `false` if another write is
/// already running. The caller must release the card (stop capture) first.
pub fn start_write(device_id: &str, profile: &str) -> bool {
    if !is_supported_device(device_id) {
        return false;
    }
    let mut state = lock_state();
    if state.write.is_some() {
        return false;
    }
    state.notices.remove(device_id);
    state.write = Some(WriteJob {
        device: device_id.to_string(),
        target: profile.to_string(),
        started: Instant::now(),
        result: None,
    });
    let (device, target) = (device_id.to_string(), profile.to_string());
    std::thread::spawn(move || {
        let result = write(&device, &target);
        if let Some(job) = lock_state().write.as_mut() {
            job.result = Some(result);
        }
    });
    true
}

/// The profile being written and for how long, while a write runs.
pub fn write_in_progress() -> Option<(String, Duration)> {
    lock_state()
        .write
        .as_ref()
        .filter(|job| job.result.is_none())
        .map(|job| (job.target.clone(), job.started.elapsed()))
}

/// Collect a write that has finished, updating the cached status.
pub fn take_finished_write() -> Option<FinishedWrite> {
    let mut state = lock_state();
    // Only a job whose thread has stored its result is finished.
    state.write.as_ref()?.result.as_ref()?;
    let job = state.write.take()?;
    let result = job
        .result
        .unwrap_or_else(|| Err("The write did not report a result".into()));
    let notice = match &result {
        Ok(status) => Ok(format!("Switched to \"{}\".", status.active.name)),
        Err(message) => Err(message.clone()),
    };
    match &result {
        Ok(status) => {
            state.cache.insert(
                job.device.clone(),
                Entry::Done(Ok(Arc::new(status.clone()))),
            );
        }
        Err(_) => {
            state.cache.remove(&job.device);
        }
    }
    state.notices.insert(job.device.clone(), notice);
    Some(FinishedWrite {
        target: job.target,
        result: result.map(|_| ()),
    })
}

#[cfg(all(target_os = "windows", feature = "elgato-edid"))]
pub use platform::handle_helper_args;

#[cfg(not(all(target_os = "windows", feature = "elgato-edid")))]
pub fn handle_helper_args() -> bool {
    false
}

#[cfg(all(target_os = "windows", feature = "elgato-edid"))]
fn query(device_id: &str) -> Result<Status, String> {
    platform::query(&interface_path(device_id).ok_or("Not an Elgato 4K X")?)
}

#[cfg(all(target_os = "windows", feature = "elgato-edid"))]
fn write(device_id: &str, profile: &str) -> Result<Status, String> {
    platform::write(
        &interface_path(device_id).ok_or("Not an Elgato 4K X")?,
        profile,
    )
}

#[cfg(not(all(target_os = "windows", feature = "elgato-edid")))]
fn query(_device_id: &str) -> Result<Status, String> {
    Err("Elgato EDID switching is not available in this build".into())
}

#[cfg(not(all(target_os = "windows", feature = "elgato-edid")))]
fn write(_device_id: &str, _profile: &str) -> Result<Status, String> {
    Err("Elgato EDID switching is not available in this build".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_4k_x_in_every_usb_mode() {
        for pid in ["009B", "009c", "009D"] {
            let id = format!(
                r"@device_pnp_\\?\usb#vid_0fd9&pid_{pid}&mi_00#7&2ef9b516&0&0000#{{65e8773d-8f56-11d0-a3b9-00a0c9223196}}\global"
            );
            assert_eq!(
                interface_path(&id).as_deref(),
                id.strip_prefix(DSHOW_PNP_PREFIX)
            );
        }
    }

    #[test]
    fn ignores_other_devices_and_interfaces() {
        for id in [
            // Another Elgato card.
            r"@device_pnp_\\?\usb#vid_0fd9&pid_0066&mi_00#7&1&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global",
            // The 4K X's audio interface, not its video one.
            r"@device_pnp_\\?\usb#vid_0fd9&pid_009b&mi_02#7&1&0&0002#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global",
            // A different vendor with a matching product ID.
            r"@device_pnp_\\?\usb#vid_1234&pid_009b&mi_00#7&1&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global",
            // Software devices and friendly-name fallbacks.
            r"@device_sw_{860BB310-5D01-11D0-BD3B-00A0C911CE86}\{...}",
            "Game Capture 4K X",
            "/dev/video0",
        ] {
            assert_eq!(interface_path(id), None, "{id}");
        }
    }
}
