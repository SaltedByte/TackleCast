//! Elgato 4K X card settings and EDID switching from the settings menu.
//!
//! Elgato Studio doesn't toggle a preset on the 4K X: it writes a whole EDID
//! file into the card's flash, and the card disconnects and reconnects while it
//! does (about 20 s). TackleCast drives the same code path, and the card's other
//! settings (HDMI color range, EDID mode, audio input, HDR tone mapping),
//! through Studio's own `EGAVDeviceSupport.dll`. Studio must therefore be
//! installed, and closed while anything is changed. The DLL only loads in a
//! helper process; see `elgato_edid/windows.rs`.
//!
//! Reads and changes run on background threads. The UI polls [`view`] each
//! frame and the main loop collects a finished change with [`take_finished_job`].

use std::collections::HashMap;
use std::path::PathBuf;
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

/// `EGAVDS_COLOR_RANGE` values. Studio shows Bypass as "Auto".
pub const COLOR_RANGES: [(i32, &str); 3] = [
    (0, "Auto"),
    (1, "Expand (limited to full)"),
    (2, "Shrink (full to limited)"),
];
/// `EGAVDS_EDID_MODE` values: which EDID the card presents to the console.
pub const EDID_MODES: [(i32, &str); 3] = [
    (0, "Internal (the card's EDID)"),
    (1, "Display (pass-through monitor's EDID)"),
    (2, "Merged (card and monitor)"),
];
pub const EDID_MODE_INTERNAL: i32 = 0;
/// `EGAVDS_AUDIO_INPUT` values (0 is "invalid").
pub const AUDIO_INPUTS: [(i32, &str); 2] = [(1, "HDMI audio"), (2, "Analog line in")];
/// `EGAVDS_INPUT_COLOR_SPACE` values, for display only.
pub const INPUT_COLOR_SPACES: [(i32, &str); 4] = [
    (0, "RGB 4:4:4"),
    (1, "YUV 4:2:2"),
    (2, "YUV 4:4:4"),
    (3, "YUV 4:2:0"),
];

pub fn label_for(options: &[(i32, &'static str)], value: i32) -> String {
    options
        .iter()
        .find(|(v, _)| *v == value)
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| format!("Unknown ({value})"))
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Profile {
    pub name: String,
    /// "default", "custom", "user file" or "pass-through monitor".
    pub kind: String,
    /// The EDID file has been downloaded, so the profile can be written.
    pub available: bool,
}

/// An EDID file on this PC that can be written to the card: one Elgato Studio
/// saved, or one loaded through TackleCast before.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SavedEdid {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Choice {
    pub value: i32,
    pub default: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Gain {
    pub value: i64,
    pub min: i64,
    pub max: i64,
    pub default: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Toggle {
    pub value: bool,
    pub default: Option<bool>,
}

/// The card's adjustable settings. `None` means the card doesn't support it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CardSettings {
    pub color_range: Option<Choice>,
    pub edid_mode: Option<Choice>,
    pub audio_input: Option<Choice>,
    pub line_in_gain: Option<Gain>,
    pub hdr_tonemapping: Option<Toggle>,
    /// The HDMI input currently carries HDR video.
    pub video_hdr: Option<bool>,
    pub input_color_space: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SettingChange {
    ColorRange(i32),
    EdidMode(i32),
    AudioInput(i32),
    LineInGain(i64),
    HdrTonemapping(bool),
}

impl SettingChange {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ColorRange(_) => "HDMI color range",
            Self::EdidMode(_) => "Input EDID mode",
            Self::AudioInput(_) => "Audio input",
            Self::LineInGain(_) => "Line-in gain",
            Self::HdrTonemapping(_) => "HDR tone mapping",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub active: Profile,
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub saved_files: Vec<SavedEdid>,
    #[serde(default)]
    pub settings: CardSettings,
    /// Elgato Studio is running, so changes are refused.
    pub studio_running: bool,
}

/// A change to make on the card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    /// Write one of Elgato's EDID profiles, by name.
    Profile(String),
    /// Write an EDID file from this PC.
    File(PathBuf),
    /// Change a setting. Takes a second or two; the card stays connected.
    Setting(SettingChange),
}

impl Job {
    /// EDID writes re-flash the card, which disconnects it.
    pub fn pauses_capture(&self) -> bool {
        !matches!(self, Job::Setting(_))
    }

    pub fn describe(&self) -> String {
        match self {
            Job::Profile(name) => format!("\"{name}\""),
            Job::File(path) => format!(
                "\"{}\"",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Job::Setting(change) => change.label().to_string(),
        }
    }
}

#[derive(Clone)]
pub struct Busy {
    pub label: String,
    pub elapsed: Duration,
    pub pauses_capture: bool,
}

pub enum View {
    Loading,
    Error(String),
    Ready(Arc<Status>),
    Busy(Busy),
}

/// The outcome of a change, for the main loop.
pub struct FinishedJob {
    pub label: String,
    pub pauses_capture: bool,
    pub result: Result<(), String>,
}

#[derive(Clone)]
enum Entry {
    Loading,
    Done(Result<Arc<Status>, String>),
}

struct RunningJob {
    device: String,
    job: Job,
    started: Instant,
    result: Option<Result<Status, String>>,
}

#[derive(Default)]
struct State {
    cache: HashMap<String, Entry>,
    job: Option<RunningJob>,
    /// Result of the last change per device, shown in the menu until the next one.
    notices: HashMap<String, Result<String, String>>,
    /// A file chosen in the EDID file picker, waiting for the menu to take it.
    picked: Option<PathBuf>,
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

/// Check that `bytes` is a well-formed EDID before it can reach the card: one
/// to four 128-byte blocks, the fixed base-block header, an extension count
/// that fits the file, and a valid checksum on every block. Blocks after the
/// declared ones must be padding (all `0x00` or all `0xFF`): Elgato Studio
/// saves monitor EDIDs padded to 512 bytes.
pub fn validate_edid(bytes: &[u8]) -> Result<(), String> {
    const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if bytes.is_empty() || !bytes.len().is_multiple_of(128) || bytes.len() > 512 {
        return Err(format!(
            "An EDID file is 128, 256, 384 or 512 bytes; this one is {} bytes",
            bytes.len()
        ));
    }
    if bytes[..8] != HEADER {
        return Err("This file doesn't start with an EDID header".into());
    }
    let blocks = bytes.len() / 128;
    let declared = bytes[126] as usize + 1;
    if declared > blocks {
        return Err(format!(
            "The EDID says it has {} extension blocks, but the file holds {}",
            bytes[126],
            blocks - 1
        ));
    }
    for (index, block) in bytes.chunks(128).enumerate() {
        if index >= declared {
            if !block.iter().all(|&b| b == 0x00) && !block.iter().all(|&b| b == 0xFF) {
                return Err(format!(
                    "The file has data in block {index}, after the EDID's {declared} blocks"
                ));
            }
        } else if block.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)) != 0 {
            return Err(format!("EDID block {index} has a bad checksum"));
        }
    }
    Ok(())
}

/// What the menu should show for this device, starting a read if needed.
/// `None` means the device isn't an Elgato 4K X.
pub fn view(device_id: &str) -> Option<View> {
    if !is_supported_device(device_id) {
        return None;
    }
    let mut state = lock_state();
    if let Some(job) = state.job.as_ref().filter(|job| job.result.is_none()) {
        if job.device == device_id {
            return Some(View::Busy(busy(job)));
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

fn busy(job: &RunningJob) -> Busy {
    Busy {
        label: job.job.describe(),
        elapsed: job.started.elapsed(),
        pauses_capture: job.job.pauses_capture(),
    }
}

/// Forget the cached status so the next [`view`] reads the card again.
pub fn refresh(device_id: &str) {
    let mut state = lock_state();
    if !matches!(state.cache.get(device_id), Some(Entry::Loading)) {
        state.cache.remove(device_id);
    }
}

/// The message from the last change to this device, if any.
pub fn notice(device_id: &str) -> Option<Result<String, String>> {
    lock_state().notices.get(device_id).cloned()
}

/// Start a change on the card. Returns `false` if another change is running.
/// For a job that [pauses capture](Job::pauses_capture), the caller must
/// release the card first.
pub fn start_job(device_id: &str, job: Job) -> bool {
    if !is_supported_device(device_id) {
        return false;
    }
    let mut state = lock_state();
    if state.job.is_some() {
        return false;
    }
    state.notices.remove(device_id);
    state.job = Some(RunningJob {
        device: device_id.to_string(),
        job: job.clone(),
        started: Instant::now(),
        result: None,
    });
    let device = device_id.to_string();
    std::thread::spawn(move || {
        let result = run(&device, &job);
        if let Some(running) = lock_state().job.as_mut() {
            running.result = Some(result);
        }
    });
    true
}

/// The change in progress, if any.
pub fn job_in_progress() -> Option<Busy> {
    lock_state()
        .job
        .as_ref()
        .filter(|job| job.result.is_none())
        .map(busy)
}

/// An EDID write is running, so capture must stay stopped.
pub fn capture_paused() -> bool {
    job_in_progress().is_some_and(|busy| busy.pauses_capture)
}

/// Collect a change that has finished, updating the cached status.
pub fn take_finished_job() -> Option<FinishedJob> {
    let mut state = lock_state();
    // Only a job whose thread has stored its result is finished.
    state.job.as_ref()?.result.as_ref()?;
    let running = state.job.take()?;
    let result = running
        .result
        .unwrap_or_else(|| Err("The change did not report a result".into()));
    let notice = match (&result, &running.job) {
        (Ok(status), Job::Profile(_) | Job::File(_)) => {
            Ok(format!("Switched to \"{}\".", status.active.name))
        }
        (Ok(_), Job::Setting(change)) => Ok(format!("{} updated.", change.label())),
        (Err(message), _) => Err(message.clone()),
    };
    match &result {
        Ok(status) => {
            state.cache.insert(
                running.device.clone(),
                Entry::Done(Ok(Arc::new(status.clone()))),
            );
        }
        Err(_) => {
            state.cache.remove(&running.device);
        }
    }
    state.notices.insert(running.device.clone(), notice);
    Some(FinishedJob {
        label: running.job.describe(),
        pauses_capture: running.job.pauses_capture(),
        result: result.map(|_| ()),
    })
}

/// Open the Windows file picker for an EDID file on a background thread. The
/// menu collects the choice with [`take_picked_file`]. `owner` is the window's
/// HWND, so the picker stays in front of it.
pub fn pick_edid_file(owner: isize) {
    std::thread::spawn(move || {
        if let Some(path) = pick_file(owner) {
            lock_state().picked = Some(path);
        }
    });
}

pub fn take_picked_file() -> Option<PathBuf> {
    lock_state().picked.take()
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
fn run(device_id: &str, job: &Job) -> Result<Status, String> {
    platform::run_job(&interface_path(device_id).ok_or("Not an Elgato 4K X")?, job)
}

#[cfg(all(target_os = "windows", feature = "elgato-edid"))]
fn pick_file(owner: isize) -> Option<PathBuf> {
    platform::pick_file(owner)
}

#[cfg(not(all(target_os = "windows", feature = "elgato-edid")))]
fn query(_device_id: &str) -> Result<Status, String> {
    Err("Elgato card settings are not available in this build".into())
}

#[cfg(not(all(target_os = "windows", feature = "elgato-edid")))]
fn run(_device_id: &str, _job: &Job) -> Result<Status, String> {
    Err("Elgato card settings are not available in this build".into())
}

#[cfg(not(all(target_os = "windows", feature = "elgato-edid")))]
fn pick_file(_owner: isize) -> Option<PathBuf> {
    None
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

    /// A minimal EDID with `extensions` empty extension blocks and valid checksums.
    fn edid(extensions: u8) -> Vec<u8> {
        let mut bytes = vec![0u8; 128 * (extensions as usize + 1)];
        bytes[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        bytes[126] = extensions;
        for block in bytes.chunks_mut(128) {
            let sum = block[..127].iter().fold(0u8, |s, b| s.wrapping_add(*b));
            block[127] = 0u8.wrapping_sub(sum);
        }
        bytes
    }

    #[test]
    fn accepts_well_formed_edids() {
        for extensions in 0..=3 {
            assert_eq!(validate_edid(&edid(extensions)), Ok(()));
        }
    }

    #[test]
    fn rejects_malformed_edids() {
        assert!(validate_edid(&[]).is_err());
        assert!(validate_edid(&edid(0)[..100]).is_err(), "wrong size");
        assert!(
            validate_edid(&[edid(3), edid(0)].concat()).is_err(),
            "too large"
        );

        let mut header = edid(1);
        header[1] = 0;
        assert!(validate_edid(&header).is_err(), "bad header");

        let mut count = edid(1);
        count[126] = 2;
        count[127] = count[127].wrapping_sub(1);
        assert!(validate_edid(&count).is_err(), "extension count mismatch");

        let mut checksum = edid(1);
        checksum[200] ^= 1;
        assert!(validate_edid(&checksum).is_err(), "bad extension checksum");

        let mut trailing = [edid(1), vec![0u8; 128]].concat();
        trailing[300] = 7;
        assert!(validate_edid(&trailing).is_err(), "data after the EDID");
    }

    #[test]
    fn accepts_edids_padded_to_512_bytes() {
        assert_eq!(validate_edid(&[edid(2), vec![0u8; 128]].concat()), Ok(()));
        assert_eq!(validate_edid(&[edid(0), vec![0xFF; 384]].concat()), Ok(()));
    }

    #[test]
    fn only_edid_writes_pause_capture() {
        assert!(Job::Profile("Game Capture 4K X".into()).pauses_capture());
        assert!(Job::File("custom.bin".into()).pauses_capture());
        assert!(!Job::Setting(SettingChange::ColorRange(1)).pauses_capture());
    }

    #[test]
    fn setting_change_round_trips_through_the_helper_protocol() {
        let change = SettingChange::LineInGain(-12);
        let json = serde_json::to_string(&change).unwrap();
        assert_eq!(
            serde_json::from_str::<SettingChange>(&json).unwrap(),
            change
        );
    }
}
