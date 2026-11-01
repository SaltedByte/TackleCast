use cpal::traits::{DeviceTrait, HostTrait};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
#[cfg(target_os = "windows")]
use tracing::warn;

#[cfg(target_os = "windows")]
use windows::core::{GUID, HSTRING, VARIANT};
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
        return devices.first().cloned().unwrap_or_default();
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

const IGNORED_KEYWORDS: &[&str] = &[
    "the",
    "and",
    "audio",
    "device",
    "video",
    "capture",
    "usb",
    "digital",
    "microphone",
    "input",
    "interface",
    "endpoint",
    "hdmi",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub index: i32,
    pub name: String,
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
    let keywords = keywords_for_device_name(&video_device_label(video_device_name));
    if keywords.is_empty() {
        return None;
    }

    let mut matches = inputs.iter().filter(|device| {
        let tokens = keywords_for_device_name(&device.name);
        keywords.iter().all(|keyword| tokens.contains(keyword))
    });
    let matched = matches.next()?;
    // A name match cannot distinguish multiple identical cards/endpoints.
    matches.next().is_none().then_some(matched.index)
}

pub fn resolve_audio_input_index(
    input_index: i32,
    video_device_name: &str,
    inputs: &[AudioDevice],
) -> Result<i32, String> {
    if input_index >= 0 {
        return inputs.iter().any(|device| device.index == input_index)
            .then_some(input_index)
            .ok_or_else(|| format!("selected audio input {input_index} is unavailable; select an input in Audio settings"));
    }
    find_audio_input_for_video(video_device_name, inputs).ok_or_else(|| {
        format!(
            "no unique audio input matches video '{}'; select an input in Audio settings",
            video_device_label(video_device_name)
        )
    })
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
    // Linux display labels append a device node, which is not an audio name.
    device_name
        .split(" (/dev/")
        .next()
        .unwrap_or(device_name)
        .to_ascii_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty() && !IGNORED_KEYWORDS.contains(word))
        .map(ToString::to_string)
        .collect()
}

#[derive(Clone, Copy)]
enum Direction {
    Input,
    Output,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_audio_follows_video_without_unrelated_or_default_fallback() {
        let inputs = vec![
            AudioDevice {
                index: 4,
                name: "Elgato 4K X (2- Elgato 4K X)".into(),
            },
            AudioDevice {
                index: 5,
                name: "Microphone (RODECaster Duo Main Multitrack)".into(),
            },
        ];
        assert_eq!(resolve_audio_input_index(-1, "Elgato 4K X", &inputs), Ok(4));
        assert!(resolve_audio_input_index(-1, "OBSBOT Tiny 2", &inputs).is_err());
        assert!(resolve_audio_input_index(-1, "USB Video Capture", &inputs).is_err());
        assert!(resolve_audio_input_index(-1, "", &inputs).is_err());
        assert!(resolve_audio_input_index(-1, "Elgato Cam Link", &inputs).is_err());
        // An explicit independent source is allowed, but a missing selection stays disconnected.
        assert_eq!(
            resolve_audio_input_index(4, "OBSBOT Tiny 2", &inputs),
            Ok(4)
        );
        assert!(resolve_audio_input_index(99, "Elgato 4K X", &inputs).is_err());
    }

    #[test]
    fn auto_audio_rejects_ambiguous_matches_and_preserves_model_tokens() {
        let mut inputs = vec![
            AudioDevice {
                index: 4,
                name: "Elgato 4K X".into(),
            },
            AudioDevice {
                index: 6,
                name: "Elgato 4K S".into(),
            },
        ];
        assert_eq!(find_audio_input_for_video("Elgato 4K X", &inputs), Some(4));
        assert_eq!(
            find_audio_input_for_video("Elgato 4K X (/dev/video0)", &inputs),
            Some(4)
        );
        inputs.push(AudioDevice {
            index: 7,
            name: "Digital Audio (Elgato 4K X)".into(),
        });
        assert_eq!(find_audio_input_for_video("Elgato 4K X", &inputs), None);
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
