use cpal::traits::{DeviceTrait, HostTrait};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
#[cfg(target_os = "windows")]
use tracing::warn;

#[cfg(target_os = "windows")]
use windows::core::{GUID, HSTRING};
#[cfg(target_os = "windows")]
use windows::Win32::Media::DirectShow::ICreateDevEnum;
#[cfg(target_os = "windows")]
use windows::Win32::Media::MediaFoundation::{
    CLSID_SystemDeviceEnum, CLSID_VideoInputDeviceCategory,
};
#[cfg(target_os = "windows")]
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
#[cfg(target_os = "windows")]
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CreateBindCtx, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
#[cfg(target_os = "windows")]
use windows::Win32::System::Variant::VARIANT;

static VIDEO_LABELS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

pub fn video_device_label(id: &str) -> String {
    VIDEO_LABELS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .get(id)
        .cloned()
        .unwrap_or_else(|| id.to_string())
}

pub fn restore_video_device(saved: &str, devices: &[String]) -> String {
    if saved.is_empty() {
        return String::new();
    }
    if devices.iter().any(|device| device == saved) {
        return saved.to_string();
    }
    let matches: Vec<_> = devices
        .iter()
        .filter(|id| video_device_label(id) == saved)
        .collect();
    if matches.len() == 1 {
        matches[0].clone()
    } else {
        saved.to_string()
    }
}

// These describe endpoints rather than a particular capture product. Require
// matching product words (and model numbers), never a shared "USB Device".
const IGNORED_KEYWORDS: &[&str] = &[
    "pro",
    "the",
    "and",
    "usb",
    "hd",
    "hdmi",
    "device",
    "devices",
    "audio",
    "video",
    "capture",
    "input",
    "output",
    "microphone",
    "mic",
    "digital",
    "interface",
    "endpoint",
];

pub const DIRECTSHOW_AUDIO_INDEX: i32 = -2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub index: i32,
    pub name: String,
}

// An unavailable explicit choice must not alias a newly enumerated endpoint.
pub const UNAVAILABLE_AUDIO_INDEX: i32 = i32::MAX;

pub fn remember_audio_choices(
    settings: &mut crate::settings::Settings,
    inputs: &[AudioDevice],
    outputs: &[AudioDevice],
) {
    for (index, name, devices) in [
        (settings.audio_input, &mut settings.audio_input_name, inputs),
        (
            settings.audio_output,
            &mut settings.audio_output_name,
            outputs,
        ),
    ] {
        if index < 0 {
            *name = None;
        } else if name.is_none() {
            *name = devices
                .iter()
                .find(|device| device.index == index)
                .map(|device| device.name.clone());
        }
    }
}

pub fn refresh_audio_choices(
    settings: &mut crate::settings::Settings,
    inputs: &[AudioDevice],
    outputs: &[AudioDevice],
) {
    reconcile_audio_choices(settings, inputs, outputs, false);
}

pub fn validate_audio_choices(
    settings: &mut crate::settings::Settings,
    inputs: &[AudioDevice],
    outputs: &[AudioDevice],
) {
    reconcile_audio_choices(settings, inputs, outputs, true);
}

fn reconcile_audio_choices(
    settings: &mut crate::settings::Settings,
    inputs: &[AudioDevice],
    outputs: &[AudioDevice],
    trust_current_choice: bool,
) {
    remember_audio_choices(settings, inputs, outputs);
    for (index, name, devices) in [
        (
            &mut settings.audio_input,
            &settings.audio_input_name,
            inputs,
        ),
        (
            &mut settings.audio_output,
            &settings.audio_output_name,
            outputs,
        ),
    ] {
        if *index < 0 {
            continue;
        }
        let Some(name) = name else {
            *index = UNAVAILABLE_AUDIO_INDEX;
            continue;
        };
        if trust_current_choice
            && devices
                .iter()
                .any(|device| device.index == *index && &device.name == name)
        {
            continue;
        }
        let mut matches = devices.iter().filter(|device| &device.name == name);
        let first = matches.next();
        *index = match (first, matches.next()) {
            (Some(device), None) => device.index,
            _ => UNAVAILABLE_AUDIO_INDEX,
        };
    }
}

pub fn enumerate_video_devices() -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        match enumerate_video_devices_dshow() {
            Ok(devices) => devices,
            Err(error) => {
                warn!("DirectShow video device enumeration failed: {error}");
                Vec::new()
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        enumerate_video_devices_v4l2()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "windows")]
fn enumerate_video_devices_dshow() -> Result<Vec<String>, String> {
    unsafe {
        // COM may already be initialized on this thread; ignore errors from re-init.
        // Use STA to be compatible with winit's OleInitialize.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let dev_enum: ICreateDevEnum =
            CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("CoCreateInstance for SystemDeviceEnum failed: {e}"))?;

        let mut enumerator = None;
        dev_enum
            .CreateClassEnumerator(
                &CLSID_VideoInputDeviceCategory as *const GUID,
                &mut enumerator,
                0,
            )
            .map_err(|e| format!("CreateClassEnumerator failed: {e}"))?;

        let Some(enumerator) = enumerator else {
            // No video capture devices on this system
            return Ok(Vec::new());
        };

        let mut devices = Vec::new();
        loop {
            let mut moniker = [None];
            let hr = enumerator.Next(&mut moniker, None);
            if hr.is_err() {
                break;
            }
            let Some(moniker) = moniker[0].take() else {
                break;
            };

            let bag: Result<IPropertyBag, _> = moniker.BindToStorage(None, None);
            let Ok(bag) = bag else {
                continue;
            };

            let mut var = VARIANT::default();
            let name_prop = HSTRING::from("FriendlyName");
            if bag.Read(&name_prop, &mut var, None).is_ok() {
                let name = format!("{}", var);
                let name = name.trim().to_string();
                if !name.is_empty() {
                    let context = CreateBindCtx(0).ok();
                    let id = context
                        .and_then(|context| moniker.GetDisplayName(&context, None).ok())
                        .and_then(|value| {
                            let id = value.to_string().ok();
                            CoTaskMemFree(Some(value.0.cast()));
                            id.map(|value| value.replace(':', "_"))
                        })
                        .unwrap_or_else(|| name.clone());
                    VIDEO_LABELS
                        .get_or_init(Default::default)
                        .lock()
                        .unwrap()
                        .insert(id.clone(), name);
                    devices.push(id);
                }
            }
        }

        Ok(devices)
    }
}

#[cfg(target_os = "linux")]
fn enumerate_video_devices_v4l2() -> Vec<String> {
    let mut devices = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") {
        let mut v4l2_entries: Vec<_> = entries.filter_map(Result::ok).collect();
        v4l2_entries.sort_by_key(|e| e.file_name());
        for entry in v4l2_entries {
            let name_file = entry.path().join("name");
            if let Ok(name) = std::fs::read_to_string(name_file) {
                let name = name.trim().to_string();
                if !name.is_empty() {
                    let dev_node = format!("/dev/{}", entry.file_name().to_string_lossy());
                    let mut id = dev_node.clone();
                    if let Ok(links) = std::fs::read_dir("/dev/v4l/by-id") {
                        let mut links: Vec<_> = links.filter_map(Result::ok).collect();
                        links.sort_by_key(|entry| entry.file_name());
                        if let Some(link) = links.into_iter().find(|entry| {
                            std::fs::canonicalize(entry.path()).ok().as_deref()
                                == Some(std::path::Path::new(&dev_node))
                        }) {
                            id = link.path().to_string_lossy().into_owned();
                        }
                    }
                    VIDEO_LABELS
                        .get_or_init(Default::default)
                        .lock()
                        .unwrap()
                        .insert(id.clone(), format!("{name} ({dev_node})"));
                    devices.push(id);
                }
            }
        }
    }
    devices
}

pub fn enumerate_audio_inputs() -> Vec<AudioDevice> {
    enumerate_audio_devices(Direction::Input)
}

pub fn enumerate_audio_outputs() -> Vec<AudioDevice> {
    enumerate_audio_devices(Direction::Output)
}

pub fn find_audio_input_for_video(video_device_name: &str, inputs: &[AudioDevice]) -> Option<i32> {
    matching_audio_input(&video_device_label(video_device_name), inputs)
}

fn matching_audio_input(video_label: &str, inputs: &[AudioDevice]) -> Option<i32> {
    let mut matches = inputs
        .iter()
        .filter(|input| names_match(video_label, &input.name));
    let matched = matches.next()?;
    matches.next().is_none().then_some(matched.index)
}

fn names_match(video: &str, audio: &str) -> bool {
    let video = keywords_for_device_name(video);
    let audio = keywords_for_device_name(audio);
    // Complete meaningful token sets avoid substring/model collisions such as
    // ShadowCast 3 vs ShadowCast 30, or two different products from one brand.
    video
        .iter()
        .any(|word| word.chars().any(char::is_alphabetic))
        && video == audio
}

#[derive(Debug, PartialEq, Eq)]
pub struct CaptureSelection {
    pub video_device: String,
    pub audio_input: Option<i32>,
}

impl CaptureSelection {
    pub fn message(&self) -> Option<&'static str> {
        if self.video_device.is_empty() {
            Some("No unique capture video/audio pair found. Press Escape and select a video device in Video and a capture input in Audio.")
        } else if self.audio_input.is_none() {
            Some("No unique audio input matches this video device. Select its capture input in Audio.")
        } else {
            None
        }
    }
}

/// Resolve automatic choices without replacing the saved automatic/explicit
/// intent. Playback outputs are deliberately outside capture-input pairing.
pub fn select_capture_devices(
    video: &str,
    audio: i32,
    videos: &[String],
    inputs: &[AudioDevice],
) -> CaptureSelection {
    let named: Vec<_> = videos
        .iter()
        .map(|id| (id.clone(), video_device_label(id)))
        .collect();
    select_named_capture_devices(video, audio, &named, inputs)
}

fn select_named_capture_devices(
    video: &str,
    audio: i32,
    videos: &[(String, String)],
    inputs: &[AudioDevice],
) -> CaptureSelection {
    if !video.is_empty() {
        let label = videos
            .iter()
            .find(|(id, _)| id == video)
            .map(|(_, label)| label.as_str())
            .unwrap_or(video);
        return CaptureSelection {
            video_device: video.to_string(),
            audio_input: if audio == -1 {
                matching_audio_input(label, inputs)
            } else {
                Some(audio)
            },
        };
    }
    for (id, label) in videos {
        let matched = if audio == -1 {
            matching_audio_input(label, inputs)
        } else {
            inputs
                .iter()
                .find(|input| input.index == audio && names_match(label, &input.name))
                .map(|input| input.index)
        };
        if let Some(index) = matched {
            let input = inputs.iter().find(|input| input.index == index).unwrap();
            // Two identically named video endpoints cannot be disambiguated by
            // their audio name. Leave this case for explicit selection.
            if videos
                .iter()
                .filter(|(_, name)| names_match(name, &input.name))
                .count()
                == 1
            {
                return CaptureSelection {
                    video_device: id.clone(),
                    audio_input: Some(index),
                };
            }
        }
    }
    CaptureSelection {
        video_device: String::new(),
        audio_input: (audio != -1).then_some(audio),
    }
}

fn enumerate_audio_devices(direction: Direction) -> Vec<AudioDevice> {
    let host = preferred_audio_host();
    let Ok(devices) = host.devices() else {
        return Vec::new();
    };

    devices
        .enumerate()
        .filter_map(|(index, device)| {
            let name = device.name().ok()?;
            let supported = match direction {
                Direction::Input => device
                    .supported_input_configs()
                    .ok()
                    .and_then(|mut configs| configs.next())
                    .is_some(),
                Direction::Output => device
                    .supported_output_configs()
                    .ok()
                    .and_then(|mut configs| configs.next())
                    .is_some(),
            };

            supported.then_some(AudioDevice {
                index: index as i32,
                name,
            })
        })
        .collect()
}

fn preferred_audio_host() -> cpal::Host {
    #[cfg(target_os = "windows")]
    {
        if let Ok(host) = cpal::host_from_id(cpal::HostId::Wasapi) {
            return host;
        }
    }

    cpal::default_host()
}

fn keywords_for_device_name(device_name: &str) -> Vec<String> {
    // Linux labels append their node for display/disambiguation; it is not a
    // product name and must not participate in audio pairing.
    let device_name = device_name.split(" (/dev/").next().unwrap_or(device_name);
    let mut words: Vec<_> = device_name
        .to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty() && !IGNORED_KEYWORDS.contains(word))
        .map(ToString::to_string)
        .collect();
    words.sort();
    words.dedup();
    words
}

#[derive(Clone, Copy)]
enum Direction {
    Input,
    Output,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> (Vec<(String, String)>, Vec<AudioDevice>) {
        (
            vec![
                ("@device_pnp_webcam".into(), "Desk Webcam".into()),
                ("@device_pnp_shadowcast".into(), "ShadowCast 3".into()),
            ],
            vec![
                AudioDevice {
                    index: 2,
                    name: "Headset Microphone".into(),
                },
                AudioDevice {
                    index: 7,
                    name: "HD (ShadowCast 3)".into(),
                },
            ],
        )
    }

    #[test]
    fn automatic_pair_skips_unmatched_first_webcam_and_headset() {
        let (videos, inputs) = candidates();
        assert_eq!(
            select_named_capture_devices("", -1, &videos, &inputs),
            CaptureSelection {
                video_device: videos[1].0.clone(),
                audio_input: Some(7)
            }
        );
        // An unresolved automatic value must stay automatic, never first video.
        assert_eq!(
            restore_video_device("", &[videos[0].0.clone(), videos[1].0.clone()]),
            ""
        );
    }

    #[test]
    fn pairing_uses_friendly_names_instead_of_moniker_identifiers() {
        let (videos, inputs) = candidates();
        {
            let mut labels = VIDEO_LABELS.get_or_init(Default::default).lock().unwrap();
            for (id, label) in &videos {
                labels.insert(id.clone(), label.clone());
            }
        }
        let ids: Vec<_> = videos.iter().map(|(id, _)| id.clone()).collect();
        assert_eq!(
            select_capture_devices("", -1, &ids, &inputs).video_device,
            videos[1].0
        );
        assert_eq!(find_audio_input_for_video(&videos[1].0, &inputs), Some(7));
        assert_eq!(
            select_named_capture_devices(&videos[1].0, -1, &videos, &inputs).audio_input,
            Some(7)
        );
        assert_eq!(matching_audio_input(&videos[1].0, &inputs), None);
    }

    #[test]
    fn unmatched_and_generic_names_require_explicit_selection() {
        let (videos, _) = candidates();
        let inputs = vec![AudioDevice {
            index: 4,
            name: "Microphone (USB HD Device)".into(),
        }];
        let selected = select_named_capture_devices("", -1, &videos, &inputs);
        assert!(selected.video_device.is_empty());
        assert_eq!(selected.audio_input, None);
        assert!(selected.message().unwrap().contains("select"));
        assert!(!names_match("USB HD Device", "USB HD Device"));
        assert!(!names_match("ShadowCast 3", "ShadowCast 30"));
        assert!(!names_match("ShadowCast 3", "ShadowCast 2"));
        assert!(names_match(
            "HD (ShadowCast-3)",
            "Digital Audio Interface (ShadowCast 3)"
        ));
        assert!(names_match(
            "ShadowCast 3 (/dev/video0)",
            "HD (ShadowCast 3)"
        ));
    }

    #[test]
    fn ambiguous_audio_or_video_names_are_not_selected_arbitrarily() {
        let (mut videos, mut inputs) = candidates();
        inputs.push(AudioDevice {
            index: 8,
            name: "ShadowCast 3".into(),
        });
        assert!(select_named_capture_devices("", -1, &videos, &inputs)
            .video_device
            .is_empty());
        assert_eq!(
            select_named_capture_devices(&videos[1].0, -1, &videos, &inputs).audio_input,
            None
        );
        inputs.pop();
        videos.push(("@another_shadowcast".into(), "ShadowCast 3".into()));
        assert!(select_named_capture_devices("", -1, &videos, &inputs)
            .video_device
            .is_empty());
        // Explicit video identity disambiguates identical friendly names.
        assert_eq!(
            select_named_capture_devices(&videos[2].0, -1, &videos, &inputs).audio_input,
            Some(7)
        );
    }

    #[test]
    fn explicit_choices_are_preserved_even_when_unavailable_or_unmatched() {
        let (videos, inputs) = candidates();
        assert_eq!(
            select_named_capture_devices(&videos[0].0, 2, &videos, &inputs),
            CaptureSelection {
                video_device: videos[0].0.clone(),
                audio_input: Some(2)
            }
        );
        assert_eq!(
            select_named_capture_devices("unplugged-video", 999, &videos, &inputs),
            CaptureSelection {
                video_device: "unplugged-video".into(),
                audio_input: Some(999)
            }
        );
        assert_eq!(
            select_named_capture_devices(&videos[1].0, DIRECTSHOW_AUDIO_INDEX, &videos, &inputs)
                .audio_input,
            Some(DIRECTSHOW_AUDIO_INDEX)
        );
        let unmatched_video = select_named_capture_devices(&videos[0].0, -1, &videos, &inputs);
        assert_eq!(unmatched_video.video_device, videos[0].0);
        assert_eq!(unmatched_video.audio_input, None);
        // An explicit audio input can guide automatic video, but is never replaced.
        assert_eq!(
            select_named_capture_devices("", 7, &videos, &inputs).video_device,
            videos[1].0
        );
        let unmatched_audio = select_named_capture_devices("", 2, &videos, &inputs);
        assert_eq!(unmatched_audio.audio_input, Some(2));
        assert!(unmatched_audio.video_device.is_empty());
    }

    #[test]
    fn first_unambiguous_meaningful_pair_wins_and_output_is_untouched() {
        let (mut videos, mut inputs) = candidates();
        videos.push(("later".into(), "Cam Link 4K".into()));
        inputs.push(AudioDevice {
            index: 12,
            name: "Audio (Cam Link 4K)".into(),
        });
        let settings = crate::settings::Settings {
            audio_output: 42,
            ..Default::default()
        };
        let selected = select_named_capture_devices(
            &settings.video_device,
            settings.audio_input,
            &videos,
            &inputs,
        );
        assert_eq!(selected.video_device, videos[1].0);
        assert_eq!(settings.audio_output, 42);
        assert_eq!(settings.audio_input, -1);
        assert!(settings.video_device.is_empty());
    }

    #[test]
    fn auto_detect_matches_keyword_overlap() {
        let inputs = vec![
            AudioDevice {
                index: 2,
                name: "Microphone (USB Audio Device)".to_string(),
            },
            AudioDevice {
                index: 7,
                name: "ShadowCast Capture Audio".to_string(),
            },
        ];

        assert_eq!(
            find_audio_input_for_video("ShadowCast Capture", &inputs),
            Some(7)
        );
    }
}
