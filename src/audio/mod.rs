pub mod common;

#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "windows")]
use windows as platform;

#[cfg(not(target_os = "windows"))]
pub mod linux;
#[cfg(not(target_os = "windows"))]
use linux as platform;

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::devices;
pub use common::{
    convert_channels, AudioDiagnostics, AudioResampler, AudioRingBuffer, AudioStatus,
    AudioStreamInfo, DirectShowAudioSink, ResolvedInput, VolumeController,
};

const RING_BUFFER_CAPACITY: usize = 48_000 * 2 * 2; // ~2 seconds of stereo audio headroom

pub struct AudioPassthrough {
    input_stream: Option<Stream>,
    output_stream: Option<Stream>,
    directshow_sink: Option<DirectShowAudioSink>,
    volume_ctrl: VolumeController,
    diagnostics: Arc<AudioDiagnostics>,
}

impl AudioPassthrough {
    pub fn new() -> Self {
        Self {
            input_stream: None,
            output_stream: None,
            directshow_sink: None,
            volume_ctrl: VolumeController::new(1.0, false),
            diagnostics: Arc::new(AudioDiagnostics::new()),
        }
    }

    pub fn status(&self) -> AudioStatus {
        self.diagnostics.status()
    }

    pub fn directshow_sink(&self) -> Option<DirectShowAudioSink> {
        self.directshow_sink.clone()
    }

    #[allow(dead_code)]
    pub fn diagnostics(&self) -> Arc<AudioDiagnostics> {
        self.diagnostics.clone()
    }

    pub fn start(
        &mut self,
        video_device_name: &str,
        input_index: i32,
        output_index: i32,
        volume: f64,
        muted: bool,
    ) {
        self.stop();
        self.volume_ctrl.set_volume(volume);
        self.volume_ctrl.set_muted(muted);

        let host = platform::preferred_audio_host();
        let inputs = devices::enumerate_audio_inputs();
        let outputs = devices::enumerate_audio_outputs();

        info!("=== Starting Audio Pipeline ===");
        info!("discovered audio inputs: {:?}", inputs);
        info!("discovered audio outputs: {:?}", outputs);

        let video_label = devices::video_device_label(video_device_name);
        let resolved_input =
            match platform::resolve_input_device(&host, input_index, &video_label, &inputs) {
                Ok(resolved) => resolved,
                Err(error) => {
                    let msg = format!("audio input unavailable: {error}");
                    warn!("{msg}");
                    self.diagnostics.set_status(AudioStatus::NoInput);
                    return;
                }
            };

        let output_device = match platform::resolve_output_device(&host, output_index, &outputs) {
            Ok(device) => device,
            Err(error) => {
                let msg = format!("audio output unavailable: {error}");
                warn!("{msg}");
                self.diagnostics.set_status(AudioStatus::NoOutput);
                return;
            }
        };

        let output_name = output_device
            .name()
            .unwrap_or_else(|_| "<unknown output>".to_string());

        let input_device = match resolved_input {
            ResolvedInput::Device(device) => device,
            ResolvedInput::DirectShow(dshow_label) => {
                let output_supported = match output_device.default_output_config() {
                    Ok(cfg) => cfg,
                    Err(error) => {
                        let msg = format!("failed to get default output config: {error}");
                        error!("{msg}");
                        self.diagnostics.set_status(AudioStatus::Error(msg));
                        return;
                    }
                };
                let out_config = StreamConfig {
                    channels: output_supported.channels(),
                    sample_rate: output_supported.sample_rate(),
                    buffer_size: cpal::BufferSize::Default,
                };
                let out_rate = out_config.sample_rate.0;
                let out_channels = out_config.channels;
                let out_format = output_supported.sample_format();

                let out_info = AudioStreamInfo {
                    device_name: output_name,
                    sample_rate: out_rate,
                    channels: out_channels,
                    sample_format: out_format,
                };
                info!("selected audio input:  DirectShow ({dshow_label})");
                info!("selected audio output: {out_info}");
                self.diagnostics.set_output_info(Some(out_info));

                let ring = Arc::new(AudioRingBuffer::new(
                    RING_BUFFER_CAPACITY,
                    self.diagnostics.clone(),
                ));
                let output_stream = match build_output_stream(
                    &output_device,
                    &out_config,
                    out_format,
                    ring.clone(),
                    self.volume_ctrl.clone(),
                    self.diagnostics.clone(),
                ) {
                    Ok(stream) => stream,
                    Err(error) => {
                        let msg = format!("failed to build output audio stream: {error}");
                        error!("{msg}");
                        self.diagnostics.set_status(AudioStatus::Error(msg));
                        return;
                    }
                };
                if let Err(error) = output_stream.play() {
                    let msg = format!("failed to start output audio stream: {error}");
                    error!("{msg}");
                    self.diagnostics.set_status(AudioStatus::Error(msg));
                    return;
                }
                info!("Audio output started successfully (DirectShow input sink ready)");
                self.directshow_sink = Some(DirectShowAudioSink {
                    ring,
                    target_sample_rate: out_rate,
                    target_channels: out_channels,
                    diagnostics: self.diagnostics.clone(),
                });
                self.output_stream = Some(output_stream);
                return;
            }
        };

        let input_name = input_device
            .name()
            .unwrap_or_else(|_| "<unknown input>".to_string());

        let (input_supported, output_supported, in_config, out_config) =
            match platform::resolve_stream_configs(&input_device, &output_device) {
                Ok(configs) => configs,
                Err(error) => {
                    let msg = format!("audio stream config negotiation failed: {error}");
                    error!("{msg}");
                    self.diagnostics.set_status(AudioStatus::Error(msg));
                    return;
                }
            };

        let in_rate = in_config.sample_rate.0;
        let in_channels = in_config.channels;
        let in_format = input_supported.sample_format();

        let out_rate = out_config.sample_rate.0;
        let out_channels = out_config.channels;
        let out_format = output_supported.sample_format();

        let in_info = AudioStreamInfo {
            device_name: input_name.clone(),
            sample_rate: in_rate,
            channels: in_channels,
            sample_format: in_format,
        };
        let out_info = AudioStreamInfo {
            device_name: output_name.clone(),
            sample_rate: out_rate,
            channels: out_channels,
            sample_format: out_format,
        };

        info!("selected audio input:  {}", in_info);
        info!("selected audio output: {}", out_info);

        self.diagnostics.set_input_info(Some(in_info));
        self.diagnostics.set_output_info(Some(out_info));

        let ring = Arc::new(AudioRingBuffer::new(
            RING_BUFFER_CAPACITY,
            self.diagnostics.clone(),
        ));

        let input_stream = match build_input_stream(
            &input_device,
            &in_config,
            in_format,
            out_rate,
            out_channels,
            ring.clone(),
            self.diagnostics.clone(),
        ) {
            Ok(stream) => stream,
            Err(error) => {
                let msg = format!("failed to build input audio stream: {error}");
                error!("{msg}");
                self.diagnostics.set_status(AudioStatus::Error(msg));
                return;
            }
        };

        let output_stream = match build_output_stream(
            &output_device,
            &out_config,
            out_format,
            ring,
            self.volume_ctrl.clone(),
            self.diagnostics.clone(),
        ) {
            Ok(stream) => stream,
            Err(error) => {
                let msg = format!("failed to build output audio stream: {error}");
                error!("{msg}");
                self.diagnostics.set_status(AudioStatus::Error(msg));
                return;
            }
        };

        if let Err(error) = input_stream.play() {
            let msg = format!("failed to start input audio stream: {error}");
            error!("{msg}");
            self.diagnostics.set_status(AudioStatus::Error(msg));
            return;
        }
        info!("Audio input started successfully");

        if let Err(error) = output_stream.play() {
            let msg = format!("failed to start output audio stream: {error}");
            error!("{msg}");
            self.diagnostics.set_status(AudioStatus::Error(msg));
            return;
        }
        info!("Audio output started successfully");

        self.diagnostics.set_status(AudioStatus::Connected);
        self.input_stream = Some(input_stream);
        self.output_stream = Some(output_stream);
    }

    pub fn stop(&mut self) {
        self.input_stream.take();
        self.output_stream.take();
        self.directshow_sink.take();
        self.diagnostics.set_status(AudioStatus::Stopped);
        self.diagnostics.set_input_info(None);
        self.diagnostics.set_output_info(None);
    }

    pub fn set_volume(&mut self, volume: f64) {
        self.volume_ctrl.set_volume(volume);
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.volume_ctrl.set_muted(muted);
    }

    #[allow(dead_code)]
    pub fn is_muted(&self) -> bool {
        self.volume_ctrl.is_muted()
    }

    #[allow(dead_code)]
    pub fn volume(&self) -> f64 {
        self.volume_ctrl.volume() as f64
    }
}

fn build_input_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    target_sample_rate: u32,
    target_channels: u16,
    ring: Arc<AudioRingBuffer>,
    diagnostics: Arc<AudioDiagnostics>,
) -> Result<Stream, cpal::BuildStreamError> {
    match sample_format {
        SampleFormat::F32 => build_input_stream_typed::<f32>(
            device,
            config,
            target_sample_rate,
            target_channels,
            ring,
            diagnostics,
        ),
        SampleFormat::I16 => build_input_stream_typed::<i16>(
            device,
            config,
            target_sample_rate,
            target_channels,
            ring,
            diagnostics,
        ),
        SampleFormat::U16 => build_input_stream_typed::<u16>(
            device,
            config,
            target_sample_rate,
            target_channels,
            ring,
            diagnostics,
        ),
        SampleFormat::I8 => build_input_stream_typed::<i8>(
            device,
            config,
            target_sample_rate,
            target_channels,
            ring,
            diagnostics,
        ),
        SampleFormat::U8 => build_input_stream_typed::<u8>(
            device,
            config,
            target_sample_rate,
            target_channels,
            ring,
            diagnostics,
        ),
        SampleFormat::I32 => build_input_stream_typed::<i32>(
            device,
            config,
            target_sample_rate,
            target_channels,
            ring,
            diagnostics,
        ),
        other => {
            warn!("unsupported input sample format: {:?}", other);
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }
}

fn build_output_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    ring: Arc<AudioRingBuffer>,
    volume_ctrl: VolumeController,
    diagnostics: Arc<AudioDiagnostics>,
) -> Result<Stream, cpal::BuildStreamError> {
    match sample_format {
        SampleFormat::F32 => {
            build_output_stream_typed::<f32>(device, config, ring, volume_ctrl, diagnostics)
        }
        SampleFormat::I16 => {
            build_output_stream_typed::<i16>(device, config, ring, volume_ctrl, diagnostics)
        }
        SampleFormat::U16 => {
            build_output_stream_typed::<u16>(device, config, ring, volume_ctrl, diagnostics)
        }
        SampleFormat::I8 => {
            build_output_stream_typed::<i8>(device, config, ring, volume_ctrl, diagnostics)
        }
        SampleFormat::U8 => {
            build_output_stream_typed::<u8>(device, config, ring, volume_ctrl, diagnostics)
        }
        SampleFormat::I32 => {
            build_output_stream_typed::<i32>(device, config, ring, volume_ctrl, diagnostics)
        }
        other => {
            warn!("unsupported output sample format: {:?}", other);
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }
}

fn build_input_stream_typed<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    target_sample_rate: u32,
    target_channels: u16,
    ring: Arc<AudioRingBuffer>,
    diagnostics: Arc<AudioDiagnostics>,
) -> Result<Stream, cpal::BuildStreamError>
where
    T: Sample + SizedSample,
    f32: FromSample<T>,
{
    let in_channels = config.channels as usize;
    let target_channels = target_channels as usize;
    let in_rate = config.sample_rate.0;

    let mut resampler = AudioResampler::new(in_rate, target_sample_rate, target_channels);

    // Pre-allocated buffers to prevent allocation in callback
    let mut scratch_f32 = Vec::with_capacity(4096);
    let mut channels_converted = Vec::with_capacity(4096);
    let mut resampled = Vec::with_capacity(4096);

    let err_diag = diagnostics;
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            scratch_f32.clear();
            scratch_f32.reserve(data.len());
            for &sample in data {
                scratch_f32.push(f32::from_sample(sample));
            }

            channels_converted.clear();
            convert_channels(
                &scratch_f32,
                in_channels,
                target_channels,
                &mut channels_converted,
            );

            resampled.clear();
            resampler.process(&channels_converted, &mut resampled);

            ring.push_samples(&resampled);
        },
        move |error| {
            error!("audio input stream error: {error}");
            err_diag.set_status(AudioStatus::Error(format!("Input error: {error}")));
        },
        None,
    )
}

fn build_output_stream_typed<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    ring: Arc<AudioRingBuffer>,
    volume_ctrl: VolumeController,
    diagnostics: Arc<AudioDiagnostics>,
) -> Result<Stream, cpal::BuildStreamError>
where
    T: Sample + SizedSample + FromSample<f32>,
{
    let mut scratch = [0.0f32; 2048];
    let err_diag = diagnostics;

    device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            let mut offset = 0;
            let gain = volume_ctrl.gain();

            while offset < data.len() {
                let chunk_len = (data.len() - offset).min(scratch.len());
                let scratch_slice = &mut scratch[..chunk_len];
                let filled = ring.pop_samples(scratch_slice);

                for i in 0..chunk_len {
                    let sample = if i < filled {
                        scratch_slice[i] * gain
                    } else {
                        0.0
                    };
                    // Cubic interpolation can overshoot even with bounded input.
                    data[offset + i] = common::f32_to_sample(sample);
                }
                offset += chunk_len;
            }
        },
        move |error| {
            error!("audio output stream error: {error}");
            err_diag.set_status(AudioStatus::Error(format!("Output error: {error}")));
        },
        None,
    )
}
