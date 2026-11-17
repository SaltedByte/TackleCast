use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host, StreamConfig, SupportedStreamConfig};
use tracing::info;

use crate::audio::common::ResolvedInput;
use crate::devices::{find_audio_input_for_video, AudioDevice, DIRECTSHOW_AUDIO_INDEX};

pub fn preferred_audio_host() -> Host {
    if let Ok(host) = cpal::host_from_id(cpal::HostId::Wasapi) {
        return host;
    }
    cpal::default_host()
}

/// Refresh WASAPI's cached clients using the same endpoint ID, including after
/// unplug/replug. CPAL's WASAPI equality compares IMMDevice::GetId, not names.
pub fn refresh_recovery_device(previous: &Device, input: bool) -> Result<Device, String> {
    let host = preferred_audio_host();
    let devices = if input {
        host.input_devices()
    } else {
        host.output_devices()
    }
    .map_err(|error| format!("Could not enumerate the previous audio endpoint: {error}"))?;
    let cpal::platform::DeviceInner::Wasapi(previous) = previous.as_inner();
    devices
        .into_iter()
        .find(|device| {
            let cpal::platform::DeviceInner::Wasapi(candidate) = device.as_inner();
            previous == candidate
        })
        .ok_or_else(|| {
            "The previous audio endpoint is disconnected; no substitute device was selected".into()
        })
}

/// Preserve explicit inputs (including opt-in DirectShow); automatic input
/// requires a unique name match. Never substitute a system microphone.
pub fn resolve_input_device(
    host: &Host,
    input_index: i32,
    video_device_name: &str,
    inputs: &[AudioDevice],
) -> Result<ResolvedInput, String> {
    if input_index == DIRECTSHOW_AUDIO_INDEX {
        if video_device_name.is_empty() {
            return Err("no video capture device selected for DirectShow audio".to_string());
        }
        info!(
            "selected DirectShow audio input from video device '{}'",
            video_device_name
        );
        return Ok(ResolvedInput::DirectShow(video_device_name.to_string()));
    }

    // 1. Try explicit index
    if input_index >= 0 {
        if let Some(device) = resolve_device_by_index(host, input_index, Direction::Input) {
            let name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
            info!(
                "resolved audio input by saved index {}: '{}'",
                input_index, name
            );
            return Ok(ResolvedInput::Device(device));
        }
        return Err(format!(
            "Selected audio input #{input_index} is unavailable. Choose an input in Audio."
        ));
    }

    // 2. Try auto-detect based on video device name
    if let Some(index) = find_audio_input_for_video(video_device_name, inputs) {
        if let Some(device) = resolve_device_by_index(host, index, Direction::Input) {
            let name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
            info!(
                "auto-detected capture card audio input {} ('{}') matching video '{}'",
                index, name, video_device_name
            );
            return Ok(ResolvedInput::Device(device));
        }
    }

    Err("No unique matching capture audio input. Choose an input in Audio.".into())
}

/// Resolves the audio output device (speakers, headphones, etc.).
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
        return Err(format!(
            "Selected audio output #{output_index} is unavailable. Choose an output in Audio."
        ));
    }

    host.default_output_device()
        .ok_or_else(|| "no default audio output device found on the system".to_string())
}

/// Query native stream configurations for input and output devices.
/// In Windows WASAPI shared mode, the output device strictly requires its default mix format.
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

    // Use default buffer sizes to let WASAPI shared mode use its native periodicity
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directshow_requires_explicit_selection_and_video_device() {
        let host = preferred_audio_host();
        let video = "Synthetic capture device";
        assert!(matches!(
            resolve_input_device(&host, DIRECTSHOW_AUDIO_INDEX, video, &[]),
            Ok(ResolvedInput::DirectShow(name)) if name == video
        ));
        assert!(resolve_input_device(&host, DIRECTSHOW_AUDIO_INDEX, "", &[]).is_err());
    }

    #[test]
    fn unmatched_automatic_input_requires_selection_with_or_without_video() {
        let host = preferred_audio_host();
        for video in ["", "Synthetic capture device"] {
            let resolved = resolve_input_device(&host, -1, video, &[]);
            assert!(resolved.is_err());
        }
    }
}
