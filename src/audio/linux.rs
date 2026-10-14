use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host, StreamConfig, SupportedStreamConfig};
use tracing::{info, warn};

use crate::devices::AudioDevice;

const CAPTURE_KEYWORDS: &[&str] = &[
    "capture",
    "hdmi",
    "usb audio",
    "shadowcast",
    "cam link",
    "camlink",
    "elgato",
    "avermedia",
    "mirabox",
    "video",
    "game",
    "ps4",
    "playstation",
    "interface",
    "endpoint",
    "pipewire",
    "alsa",
];

const IGNORED_KEYWORDS: &[&str] = &["pro", "the", "and", "audio", "device"];

pub fn preferred_audio_host() -> Host {
    cpal::default_host()
}

pub fn resolve_input_device(
    host: &Host,
    input_index: i32,
    video_device_name: &str,
    inputs: &[AudioDevice],
) -> Result<Device, String> {
    if input_index >= 0 {
        if let Some(device) = resolve_device_by_index(host, input_index, Direction::Input) {
            let name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
            info!(
                "resolved audio input by saved index {}: '{}'",
                input_index, name
            );
            return Ok(device);
        }
        warn!(
            "saved audio input index {} is no longer available; searching for capture device",
            input_index
        );
    }

    if let Some(index) = find_audio_input_for_video(video_device_name, inputs) {
        if let Some(device) = resolve_device_by_index(host, index, Direction::Input) {
            let name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
            info!(
                "auto-detected capture card audio input {} ('{}') matching video '{}'",
                index, name, video_device_name
            );
            return Ok(device);
        }
    }

    for device_info in inputs {
        let name_lower = device_info.name.to_ascii_lowercase();
        if CAPTURE_KEYWORDS.iter().any(|&kw| name_lower.contains(kw)) {
            if let Some(device) = resolve_device_by_index(host, device_info.index, Direction::Input)
            {
                info!(
                    "matched generic capture card audio input {} ('{}')",
                    device_info.index, device_info.name
                );
                return Ok(device);
            }
        }
    }

    warn!("no capture-card audio device auto-detected; falling back to default system input");
    host.default_input_device()
        .ok_or_else(|| "no default audio input device found on the system".to_string())
}

pub fn resolve_output_device(
    host: &Host,
    output_index: i32,
    _outputs: &[AudioDevice],
) -> Result<Device, String> {
    if output_index >= 0 {
        if let Some(device) = resolve_device_by_index(host, output_index, Direction::Output) {
            let name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
            info!(
                "resolved audio output by index {}: '{}'",
                output_index, name
            );
            return Ok(device);
        }
        warn!(
            "saved audio output index {} is no longer available; falling back to default output",
            output_index
        );
    }

    host.default_output_device()
        .ok_or_else(|| "no default audio output device found on the system".to_string())
}

pub fn resolve_stream_configs(
    input_device: &Device,
    output_device: &Device,
) -> Result<
    (
        SupportedStreamConfig,
        SupportedStreamConfig,
        StreamConfig,
        StreamConfig,
    ),
    String,
> {
    let input_supported = input_device
        .default_input_config()
        .map_err(|e| format!("failed to get default input config: {e}"))?;

    let output_supported = output_device
        .default_output_config()
        .map_err(|e| format!("failed to get default output config: {e}"))?;

    let input_stream_config = StreamConfig {
        channels: input_supported.channels(),
        sample_rate: input_supported.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };

    let output_stream_config = StreamConfig {
        channels: output_supported.channels(),
        sample_rate: output_supported.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };

    Ok((
        input_supported,
        output_supported,
        input_stream_config,
        output_stream_config,
    ))
}

#[derive(Clone, Copy)]
pub enum Direction {
    Input,
    Output,
}

pub fn resolve_device_by_index(host: &Host, index: i32, direction: Direction) -> Option<Device> {
    let Ok(devices) = host.devices() else {
        return None;
    };

    devices.enumerate().find_map(|(device_index, device)| {
        (device_index as i32 == index && device_supports_direction(&device, direction))
            .then_some(device)
    })
}

pub fn device_supports_direction(device: &Device, direction: Direction) -> bool {
    match direction {
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
    }
}

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
    if best_score >= threshold {
        best_index
    } else {
        None
    }
}

fn keywords_for_device_name(device_name: &str) -> Vec<String> {
    device_name
        .to_ascii_lowercase()
        .replace(['-', '_', '/', '\\', '(', ')'], " ")
        .split_whitespace()
        .filter(|word| word.len() >= 3 && !IGNORED_KEYWORDS.contains(word))
        .map(ToString::to_string)
        .collect()
}
