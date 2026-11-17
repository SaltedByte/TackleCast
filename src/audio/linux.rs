use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host, StreamConfig, SupportedStreamConfig};
use tracing::info;

use crate::audio::common::ResolvedInput;
use crate::devices::{find_audio_input_for_video, AudioDevice};

pub fn preferred_audio_host() -> Host {
    cpal::default_host()
}

pub fn refresh_recovery_device(previous: &Device, _input: bool) -> Result<Device, String> {
    // Retain the same ALSA logical device rather than resolving a saved index
    // or following a changed system default during recovery.
    Ok(previous.clone())
}

pub fn resolve_input_device(
    host: &Host,
    input_index: i32,
    video_device_name: &str,
    inputs: &[AudioDevice],
) -> Result<ResolvedInput, String> {
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
