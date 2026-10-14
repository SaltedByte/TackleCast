use cpal::traits::{DeviceTrait, HostTrait};
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
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};

#[allow(dead_code)]
const IGNORED_KEYWORDS: &[&str] = &["pro", "the", "and"];

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
                    devices.push(name);
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
                    devices.push(format!("{name} ({dev_node})"));
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

#[allow(dead_code)]
pub fn find_audio_input_for_video(video_device_name: &str, inputs: &[AudioDevice]) -> Option<i32> {
    let keywords = keywords_for_device_name(video_device_name);
    if keywords.is_empty() {
        return None;
    }

    let mut best_index = None;
    let mut best_score = 0;
    for device in inputs {
        let haystack = device.name.to_ascii_lowercase();
        let score = keywords
            .iter()
            .filter(|keyword| haystack.contains(keyword.as_str()))
            .count();
        if score > best_score {
            best_score = score;
            best_index = Some(device.index);
        }
    }

    let threshold = if keywords.len() == 1 { 1 } else { 2 };
    (best_score >= threshold).then_some(best_index?).or(None)
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

#[allow(dead_code)]
fn keywords_for_device_name(device_name: &str) -> Vec<String> {
    device_name
        .to_ascii_lowercase()
        .replace('-', " ")
        .split_whitespace()
        .filter(|word| word.len() >= 3 && !IGNORED_KEYWORDS.contains(word))
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
