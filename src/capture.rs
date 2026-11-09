use crate::color::ColorInfo;
use crate::frame_rate::FrameRate;
use crate::settings::{ColorRange, ColorSpace, VideoFormat};
use std::f32::consts::PI;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use ffmpeg::{
    codec, device, format, media,
    software::scaling::{flag::Flags as ScaleFlags, Context as ScaleContext},
    threading,
    util::frame::video::Video,
    Dictionary,
};
use ffmpeg_next as ffmpeg;
use tracing::{info, warn};
#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
use winit::event_loop::EventLoopProxy;

use crate::triple_buffer::Producer;
use crate::AppEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Nv12,
    Yuvj422p,
}

#[derive(Debug, Clone)]
pub enum CaptureFrame {
    /// Frame data lives in CPU memory (software decode or GPU decode fallback).
    Cpu {
        width: u32,
        height: u32,
        format: PixelFormat,
        color_info: ColorInfo,
        y_data: Vec<u8>,
        u_data: Vec<u8>,
        v_data: Vec<u8>,
    },
    /// Frame data lives in a shared DX12/CUDA GPU buffer (zero-copy path).
    /// The renderer knows where the actual buffers are; this just carries
    /// the dimensions and which double-buffer set to read from.
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    Gpu {
        width: u32,
        height: u32,
        buffer_index: usize,
        color_info: ColorInfo,
    },
}

impl CaptureFrame {
    pub fn color_info(&self) -> ColorInfo {
        match self {
            Self::Cpu { color_info, .. } => *color_info,
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            Self::Gpu { color_info, .. } => *color_info,
        }
    }
    pub fn set_color_info(&mut self, info: ColorInfo) {
        match self {
            Self::Cpu { color_info, .. } => *color_info = info,
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            Self::Gpu { color_info, .. } => *color_info = info,
        }
    }
    /// Create an empty frame for triple buffer slot initialization.
    pub fn empty() -> Self {
        Self::Cpu {
            width: 0,
            height: 0,
            format: PixelFormat::Nv12,
            color_info: ColorInfo::default(),
            y_data: Vec::new(),
            u_data: Vec::new(),
            v_data: Vec::new(),
        }
    }

    /// Reshape this frame into a `Cpu` frame with the given geometry, keeping
    /// the existing plane buffers when it already is one, and return the planes
    /// for the caller to fill.
    ///
    /// `Vec::resize` is a no-op once capacity is sufficient, so a frame
    /// recycled from the triple buffer is reshaped without allocating.
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    pub fn reshape_cpu(
        &mut self,
        new_width: u32,
        new_height: u32,
        new_format: PixelFormat,
        y_len: usize,
        uv_len: usize,
    ) -> (&mut [u8], &mut [u8], &mut [u8]) {
        if !matches!(self, Self::Cpu { .. }) {
            *self = Self::empty();
        }

        match self {
            Self::Cpu {
                width,
                height,
                format,
                y_data,
                u_data,
                v_data,
                ..
            } => {
                *width = new_width;
                *height = new_height;
                *format = new_format;
                y_data.resize(y_len, 0);
                u_data.resize(uv_len, 0);
                v_data.resize(uv_len, 0);
                (
                    y_data.as_mut_slice(),
                    u_data.as_mut_slice(),
                    v_data.as_mut_slice(),
                )
            }
            Self::Gpu { .. } => unreachable!("coerced to Cpu above"),
        }
    }

    pub fn width(&self) -> u32 {
        match self {
            Self::Cpu { width, .. } => *width,
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            Self::Gpu { width, .. } => *width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::Cpu { height, .. } => *height,
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            Self::Gpu { height, .. } => *height,
        }
    }

    pub fn format(&self) -> Option<PixelFormat> {
        match self {
            Self::Cpu { format, .. } => Some(*format),
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            Self::Gpu { .. } => Some(PixelFormat::Yuvj422p), // nvJPEG always outputs YUV 4:2:2
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureStats {
    pub format_label: String,
    pub nominal_rate: Option<FrameRate>,
    pub fps: f32,
    pub width: u32,
    pub height: u32,
}

/// Sent once when the capture thread successfully opens a device at settings
/// different from what was originally requested (resolution/fps fallback).
#[derive(Debug, Clone, Copy)]
pub struct NegotiatedConfig {
    pub width: u32,
    pub height: u32,
    pub fps: FrameRate,
}

pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub fps: FrameRate,
    pub source: CaptureSource,
    pub audio_sink: Option<crate::audio::DirectShowAudioSink>,
}

#[allow(dead_code)]
pub enum CaptureSource {
    TestPattern {
        alternate_formats: bool,
        force_format: Option<PixelFormat>,
    },
    Device {
        device_name: String,
        pixel_format: String,
        decode_threads: usize,
        #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
        shared_gpu_handles: Option<crate::dx12_interop::ImportHandles>,
    },
    #[cfg(target_os = "windows")]
    DirectShow {
        device_name: String,
        pixel_format: String,
        decode_threads: usize,
        #[cfg(feature = "gpu-decode")]
        shared_gpu_handles: Option<crate::dx12_interop::ImportHandles>,
    },
    #[cfg(target_os = "linux")]
    V4L2 {
        device_name: String,
        pixel_format: String,
        decode_threads: usize,
    },
}

pub struct CaptureThread {
    frame_consumer: crate::triple_buffer::Consumer<CaptureFrame>,
    stats_rx: Receiver<CaptureStats>,
    error_rx: Receiver<String>,
    negotiated_rx: Receiver<NegotiatedConfig>,
    stop_flag: Arc<AtomicBool>,
    join_handle: Option<JoinHandle<()>>,
}

impl CaptureThread {
    pub fn start(config: CaptureConfig, event_proxy: EventLoopProxy<AppEvent>) -> Self {
        let (frame_producer, frame_consumer) =
            crate::triple_buffer::triple_buffer(CaptureFrame::empty);
        let (stats_tx, stats_rx) = unbounded();
        let (error_tx, error_rx) = unbounded();
        let (negotiated_tx, negotiated_rx) = unbounded();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = stop_flag.clone();
        let audio_sink = config.audio_sink;

        let join_handle = thread::spawn(move || match config.source {
            CaptureSource::TestPattern {
                alternate_formats,
                force_format,
            } => run_test_pattern(
                config.width,
                config.height,
                config.fps,
                alternate_formats,
                force_format,
                thread_stop_flag,
                frame_producer,
                stats_tx,
                &event_proxy,
            ),
            CaptureSource::Device {
                device_name,
                pixel_format,
                decode_threads,
                #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
                shared_gpu_handles,
            } => run_device_capture(
                device_name,
                config.width,
                config.height,
                config.fps,
                pixel_format,
                decode_threads,
                thread_stop_flag,
                frame_producer,
                stats_tx,
                error_tx,
                negotiated_tx,
                &event_proxy,
                audio_sink,
                #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
                shared_gpu_handles,
            ),
            #[cfg(target_os = "windows")]
            CaptureSource::DirectShow {
                device_name,
                pixel_format,
                decode_threads,
                #[cfg(feature = "gpu-decode")]
                shared_gpu_handles,
            } => run_device_capture(
                device_name,
                config.width,
                config.height,
                config.fps,
                pixel_format,
                decode_threads,
                thread_stop_flag,
                frame_producer,
                stats_tx,
                error_tx,
                negotiated_tx,
                &event_proxy,
                audio_sink,
                #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
                shared_gpu_handles,
            ),
            #[cfg(target_os = "linux")]
            CaptureSource::V4L2 {
                device_name,
                pixel_format,
                decode_threads,
            } => run_device_capture(
                device_name,
                config.width,
                config.height,
                config.fps,
                pixel_format,
                decode_threads,
                thread_stop_flag,
                frame_producer,
                stats_tx,
                error_tx,
                negotiated_tx,
                &event_proxy,
                audio_sink,
            ),
        });

        Self {
            frame_consumer,
            stats_rx,
            error_rx,
            negotiated_rx,
            stop_flag,
            join_handle: Some(join_handle),
        }
    }

    /// Borrows the latest frame if the capture thread has produced one since
    /// the last call. Returns None if already up-to-date.
    ///
    /// The frame stays in its triple-buffer slot so its plane buffers can be
    /// reused for a later frame — hence the borrow rather than an owned value.
    pub fn latest_frame(&mut self) -> Option<&CaptureFrame> {
        self.frame_consumer.read()
    }

    pub fn latest_stats(&self) -> Option<CaptureStats> {
        self.stats_rx.try_iter().last()
    }

    pub fn latest_error(&self) -> Option<String> {
        self.error_rx.try_iter().last()
    }

    pub fn latest_negotiated(&self) -> Option<NegotiatedConfig> {
        self.negotiated_rx.try_iter().last()
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

impl Drop for CaptureThread {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_test_pattern(
    width: u32,
    height: u32,
    fps: FrameRate,
    alternate_formats: bool,
    force_format: Option<PixelFormat>,
    stop_flag: Arc<AtomicBool>,
    mut frame_producer: Producer<CaptureFrame>,
    stats_tx: Sender<CaptureStats>,
    event_proxy: &EventLoopProxy<AppEvent>,
) {
    let frame_interval = Duration::from_secs_f64(1.0 / fps.as_f64());
    let mut frame_index = 0_u64;
    let mut last_stats_at = Instant::now();
    let mut stats_frame_counter = 0_u32;

    while !stop_flag.load(Ordering::Relaxed) {
        let loop_started = Instant::now();
        let format = force_format.unwrap_or_else(|| {
            if alternate_formats && ((frame_index / (fps.as_f64() as u64).max(1)) % 6) >= 3 {
                PixelFormat::Yuvj422p
            } else {
                PixelFormat::Nv12
            }
        });

        let frame = generate_test_frame(width, height, frame_index, format);
        frame_producer.write(frame);
        let _ = event_proxy.send_event(AppEvent::FrameReady);

        frame_index += 1;
        stats_frame_counter += 1;

        let elapsed = last_stats_at.elapsed();
        if elapsed >= Duration::from_millis(300) {
            let measured_fps = stats_frame_counter as f32 / elapsed.as_secs_f32();
            let format_label = match format {
                PixelFormat::Nv12 => "Test NV12",
                PixelFormat::Yuvj422p => "Test YUV422",
            }
            .to_string();
            let _ = stats_tx.send(CaptureStats {
                fps: measured_fps,
                width,
                height,
                format_label,
                nominal_rate: Some(fps),
            });
            last_stats_at = Instant::now();
            stats_frame_counter = 0;
        }

        let spent = loop_started.elapsed();
        if spent < frame_interval {
            thread::sleep(frame_interval - spent);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_device_capture(
    device_name: String,
    requested_width: u32,
    requested_height: u32,
    requested_fps: FrameRate,
    requested_pixel_format: String,
    decode_threads: usize,
    stop_flag: Arc<AtomicBool>,
    mut frame_producer: Producer<CaptureFrame>,
    stats_tx: Sender<CaptureStats>,
    error_tx: Sender<String>,
    negotiated_tx: Sender<NegotiatedConfig>,
    event_proxy: &EventLoopProxy<AppEvent>,
    audio_sink: Option<crate::audio::DirectShowAudioSink>,
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))] mut shared_gpu_handles: Option<
        crate::dx12_interop::ImportHandles,
    >,
) {
    #[cfg(target_os = "windows")]
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);

    let caps = crate::capabilities::get_device_capabilities(&device_name);
    let resolution = crate::capabilities::resolution_label(requested_width, requested_height);
    let requested = VideoFormat::ALL
        .into_iter()
        .find(|value| value.ffmpeg_name() == requested_pixel_format)
        .unwrap_or(VideoFormat::Auto);
    let attempts = if requested == VideoFormat::Auto {
        let formats =
            crate::capabilities::supported_video_formats(&caps, &resolution, requested_fps);
        if formats.len() > 1 {
            formats
                .into_iter()
                .filter(|f| *f != VideoFormat::Auto)
                .collect::<Vec<_>>()
        } else {
            vec![VideoFormat::Auto]
        }
    } else {
        vec![requested]
    };
    let mut last_error = None;
    for attempt in attempts {
        if stop_flag.load(Ordering::Relaxed) {
            return;
        }
        let pixel = if attempt == VideoFormat::Auto {
            None
        } else {
            Some(attempt.ffmpeg_name())
        };
        info!(
            "capture attempt: device='{}' format='{}' {}x{} @ {} FPS",
            device_name, attempt, requested_width, requested_height, requested_fps
        );
        let result = run_device_capture_inner(
            &device_name,
            requested_width,
            requested_height,
            requested_fps,
            pixel,
            decode_threads,
            stop_flag.clone(),
            &mut frame_producer,
            stats_tx.clone(),
            &negotiated_tx,
            event_proxy,
            audio_sink.clone(),
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            if attempt == VideoFormat::Mjpeg {
                shared_gpu_handles.take()
            } else {
                None
            },
        );
        match result {
            Ok(()) => return,
            Err(error) => {
                warn!("capture attempt failed: {error}");
                last_error = Some(error);
            }
        }
    }
    let _ =
        error_tx.send(last_error.unwrap_or_else(|| "No supported capture mode was found".into()));
}

#[allow(clippy::too_many_arguments)]
fn run_device_capture_inner(
    device_name: &str,
    requested_width: u32,
    requested_height: u32,
    requested_fps: FrameRate,
    requested_pixel_format: Option<&str>,
    decode_threads: usize,
    stop_flag: Arc<AtomicBool>,
    frame_producer: &mut Producer<CaptureFrame>,
    stats_tx: Sender<CaptureStats>,
    negotiated_tx: &Sender<NegotiatedConfig>,
    event_proxy: &EventLoopProxy<AppEvent>,
    audio_sink: Option<crate::audio::DirectShowAudioSink>,
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))] shared_gpu_handles: Option<
        crate::dx12_interop::ImportHandles,
    >,
) -> Result<(), String> {
    let (input_format, format_label) = find_input_format()?;
    let mut options = Dictionary::new();
    options.set(
        "video_size",
        &format!("{requested_width}x{requested_height}"),
    );
    options.set("framerate", &requested_fps.key());
    options.set("rtbufsize", "16M");
    options.set("probesize", "5000000");
    options.set("analyzeduration", "1000000");

    #[cfg(target_os = "windows")]
    let url = format!("video={device_name}");
    #[cfg(target_os = "linux")]
    let url = resolve_v4l2_node(device_name);
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let url = device_name.to_string();

    if let Some(format_name) = requested_pixel_format {
        #[cfg(target_os = "windows")]
        {
            if format_name.eq_ignore_ascii_case("mjpeg") {
                options.set("vcodec", "mjpeg");
            } else {
                options.set("pixel_format", format_name);
            }
        }
        #[cfg(target_os = "linux")]
        {
            if format_name.eq_ignore_ascii_case("mjpeg") {
                options.set("input_format", "mjpeg");
            } else {
                options.set("input_format", format_name);
            }
        }
    }

    #[cfg(target_os = "windows")]
    let mut input = if audio_sink.is_some() {
        let mut audio_opts = options.clone();
        audio_opts.set("audio_buffer_size", "20");
        let combined_url = format!("video={device_name}:audio={device_name}");
        let friendly = crate::devices::video_device_label(device_name);
        let opened = format::open_with(&combined_url, &input_format, audio_opts.clone()).or_else(
            |first_err| {
                if friendly != device_name && !friendly.contains(':') {
                    let alt_url = format!("video={device_name}:audio={friendly}");
                    format::open_with(&alt_url, &input_format, audio_opts)
                } else {
                    Err(first_err)
                }
            },
        );
        match opened {
            Ok(ctx) => ctx.input(),
            Err(audio_err) => {
                let video_ctx =
                    format::open_with(&url, &input_format, options).map_err(|error| {
                        format!(
                        "failed to open {format_label} input for '{device_name}' ({url}): {error}"
                    )
                    })?;
                warn!(
                    "DirectShow audio unavailable for '{device_name}' ({audio_err}); capturing video only"
                );
                video_ctx.input()
            }
        }
    } else {
        format::open_with(&url, &input_format, options)
            .map_err(|error| {
                format!("failed to open {format_label} input for '{device_name}' ({url}): {error}")
            })?
            .input()
    };
    #[cfg(not(target_os = "windows"))]
    let mut input = format::open_with(&url, &input_format, options)
        .map_err(|error| {
            format!("failed to open {format_label} input for '{device_name}' ({url}): {error}")
        })?
        .input();

    let input_stream = input
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| format!("no video stream found for '{device_name}'"))?;
    let stream_index = input_stream.index();
    let parameters = input_stream.parameters();

    let mut decoder_context = codec::context::Context::from_parameters(parameters)
        .map_err(|error| format!("failed to create decoder context: {error}"))?;
    decoder_context.set_threading(threading::Config {
        kind: threading::Type::Frame,
        count: if decoder_context.id() == codec::Id::MJPEG {
            decode_threads.max(4)
        } else {
            1
        },
    });

    let mut decoder = decoder_context
        .decoder()
        .video()
        .map_err(|error| format!("failed to open video decoder: {error}"))?;

    let actual_rate = input_stream.rate();
    let nominal_rate = if actual_rate.0 > 0 && actual_rate.1 > 0 {
        Some(FrameRate::new(actual_rate.0 as u32, actual_rate.1 as u32).normalized())
    } else {
        None
    };
    if decoder.width() != requested_width || decoder.height() != requested_height {
        return Err(format!(
            "Requested {}x{}, but the device negotiated {}x{}; capture was not silently resized",
            requested_width,
            requested_height,
            decoder.width(),
            decoder.height()
        ));
    }
    if let Some(rate) = nominal_rate {
        if !rate.equivalent(requested_fps) {
            return Err(format!(
                "Requested {requested_fps} FPS, but the device negotiated {rate} FPS"
            ));
        }
        let _ = negotiated_tx.send(NegotiatedConfig {
            width: decoder.width(),
            height: decoder.height(),
            fps: rate,
        });
    }
    let capture_format = actual_capture_format(decoder.id(), decoder.format());
    let capture_name = if decoder.id() == codec::Id::MJPEG {
        "MJPEG".to_string()
    } else if decoder.id() == codec::Id::RAWVIDEO {
        decoder
            .format()
            .descriptor()
            .map(|description| description.name().to_ascii_uppercase())
            .unwrap_or_else(|| "Unknown".into())
    } else {
        format!("{:?}", decoder.id())
    };
    if let Some(requested) = requested_pixel_format {
        if capture_format.ffmpeg_name() != requested {
            return Err(format!(
                "Requested format {requested}, but the device negotiated {capture_format}"
            ));
        }
    }
    info!(
        "stream negotiated: {}x{} | {} | nominal rate {}",
        decoder.width(),
        decoder.height(),
        capture_name,
        nominal_rate
            .map(|r| r.to_string())
            .unwrap_or_else(|| "Unknown".into())
    );

    let mut audio_decoder = if let Some(ref sink) = audio_sink {
        if let Some(audio_stream) = input.streams().best(media::Type::Audio) {
            let audio_stream_index = audio_stream.index();
            match codec::context::Context::from_parameters(audio_stream.parameters())
                .and_then(|ctx| ctx.decoder().audio())
            {
                Ok(dec) => {
                    let in_rate = dec.rate().max(1);
                    let in_channels = dec.channels().max(1);
                    let label = crate::devices::video_device_label(device_name);
                    info!(
                        "DirectShow audio negotiated for '{}': {} Hz, {} ch, {:?}",
                        label,
                        in_rate,
                        in_channels,
                        dec.format()
                    );
                    sink.diagnostics
                        .set_input_info(Some(crate::audio::AudioStreamInfo {
                            device_name: format!("DirectShow: {label}"),
                            sample_rate: in_rate,
                            channels: in_channels,
                            sample_format: cpal::SampleFormat::F32,
                        }));
                    sink.diagnostics
                        .set_status(crate::audio::AudioStatus::Connected);
                    Some((
                        audio_stream_index,
                        dec,
                        ffmpeg::util::frame::audio::Audio::empty(),
                        crate::audio::AudioResampler::new(
                            in_rate,
                            sink.target_sample_rate,
                            sink.target_channels as usize,
                        ),
                        Vec::<f32>::with_capacity(4096),
                        Vec::<f32>::with_capacity(4096),
                        Vec::<f32>::with_capacity(4096),
                    ))
                }
                Err(err) => {
                    warn!("failed to open DirectShow audio decoder for '{device_name}': {err}");
                    sink.diagnostics
                        .set_status(crate::audio::AudioStatus::NoInput);
                    None
                }
            }
        } else {
            sink.diagnostics
                .set_status(crate::audio::AudioStatus::NoInput);
            None
        }
    } else {
        None
    };

    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    let source_color = ColorInfo::from_ffmpeg(
        decoder.color_space(),
        decoder.color_range(),
        decoder.format(),
    );
    let mut decoded = Video::empty();
    let mut last_stats_at = Instant::now();
    let mut last_summary_at = Instant::now();
    let mut summary_frame_counter = 0_u64;
    let mut stats_frame_counter = 0_u32;
    let mut total_frames = 0_u64;
    let mut logged_first_frame = false;
    let mut packet_errors = 0_u32;

    // Frame arrival timing
    let mut last_packet_at: Option<Instant> = None;
    let mut arrival_min_us = u64::MAX;
    let mut arrival_max_us = 0_u64;
    let mut arrival_sum_us = 0_u64;
    let mut arrival_count = 0_u64;
    let mut scaler: Option<ScaleContext> = None;
    let mut scaled_frame = Video::empty();

    // Try to initialize GPU-accelerated MJPEG decode (NVIDIA nvJPEG)
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    let mut gpu_decoder = if decoder.id() == codec::Id::MJPEG {
        // Try zero-copy shared buffer mode first, then fall back to owned mode
        if let Some(handles) = shared_gpu_handles {
            info!("attempting zero-copy GPU decode with shared DX12 buffers");
            crate::gpu_decode::NvjpegDecoder::try_new_shared(handles).or_else(|| {
                info!("zero-copy init failed, falling back to owned GPU decode");
                crate::gpu_decode::NvjpegDecoder::try_new()
            })
        } else {
            crate::gpu_decode::NvjpegDecoder::try_new()
        }
    } else {
        None
    };
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    let use_gpu = gpu_decoder.is_some();
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    let is_zero_copy = gpu_decoder
        .as_ref()
        .map(|d| d.is_zero_copy())
        .unwrap_or(false);
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    let mut gpu_errors = 0_u32;
    #[cfg(not(all(target_os = "windows", feature = "gpu-decode")))]
    let use_gpu = false;
    #[cfg(not(all(target_os = "windows", feature = "gpu-decode")))]
    let is_zero_copy = false;

    info!(
        "capture thread opened DirectShow stream for '{}' at {}x{} @ {}fps using {} (gpu_decode={}, zero_copy={})",
        device_name,
        requested_width,
        requested_height,
        requested_fps,
        requested_pixel_format.unwrap_or("auto"),
        use_gpu,
        is_zero_copy,
    );

    for (stream, packet) in input.packets() {
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(());
        }

        if stream.index() != stream_index {
            if let (
                Some(ref sink),
                Some((
                    audio_idx,
                    ref mut a_dec,
                    ref mut a_frame,
                    ref mut resampler,
                    ref mut scratch_f32,
                    ref mut ch_buf,
                    ref mut resampled_buf,
                )),
            ) = (&audio_sink, &mut audio_decoder)
            {
                if stream.index() == *audio_idx && a_dec.send_packet(&packet).is_ok() {
                    while a_dec.receive_frame(a_frame).is_ok() {
                        extract_audio_samples_f32(a_frame, scratch_f32);
                        ch_buf.clear();
                        crate::audio::convert_channels(
                            scratch_f32,
                            a_frame.channels() as usize,
                            sink.target_channels as usize,
                            ch_buf,
                        );
                        resampled_buf.clear();
                        resampler.process(ch_buf, resampled_buf);
                        sink.ring.push_samples(resampled_buf);
                    }
                }
            }
            continue;
        }

        // Track frame arrival timing
        let now = Instant::now();
        if let Some(prev) = last_packet_at {
            let interval_us = now.duration_since(prev).as_micros() as u64;
            arrival_min_us = arrival_min_us.min(interval_us);
            arrival_max_us = arrival_max_us.max(interval_us);
            arrival_sum_us += interval_us;
            arrival_count += 1;
        }
        last_packet_at = Some(now);

        // Try GPU decode first when available (MJPEG only)
        #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
        if let Some(ref mut gpu) = gpu_decoder {
            if let Some(data) = packet.data() {
                // Decode straight into the back slot, reusing the plane buffers
                // the previous frame in that slot left behind. Bound to a local
                // so the borrow ends before the arms touch the producer again.
                let decoded = gpu.decode_into(data, frame_producer.back_slot());

                match decoded {
                    Ok(()) => {
                        let slot = frame_producer.back_slot();
                        let width = slot.width();
                        let height = slot.height();
                        let format = slot.format();
                        total_frames += 1;
                        gpu_errors = 0; // reset consecutive error count on success

                        if !logged_first_frame {
                            logged_first_frame = true;
                            info!(
                                "first GPU-decoded frame from '{}' => {}x{} {:?}",
                                device_name, width, height, format
                            );
                        }

                        let mut color = source_color;
                        color.default_range = ColorRange::Full;
                        frame_producer.back_slot().set_color_info(color);
                        frame_producer.publish();
                        let _ = event_proxy.send_event(AppEvent::FrameReady);

                        stats_frame_counter += 1;
                        summary_frame_counter += 1;
                        let elapsed = last_stats_at.elapsed();
                        if elapsed >= Duration::from_millis(300) {
                            let fps = stats_frame_counter as f32 / elapsed.as_secs_f32();
                            let backend = if gpu.is_zero_copy() {
                                "Zero-Copy"
                            } else {
                                "GPU"
                            };
                            let _ = stats_tx.send(CaptureStats {
                                fps,
                                width,
                                height,
                                format_label: format!("{capture_name} ({backend})"),
                                nominal_rate,
                            });
                            last_stats_at = Instant::now();
                            stats_frame_counter = 0;
                        }
                        let summary_elapsed = last_summary_at.elapsed();
                        if summary_elapsed >= Duration::from_secs(30) {
                            let avg_fps =
                                summary_frame_counter as f64 / summary_elapsed.as_secs_f64();
                            let arrival_info = if arrival_count > 0 {
                                let avg_ms =
                                    (arrival_sum_us as f64 / arrival_count as f64) / 1000.0;
                                let min_ms = arrival_min_us as f64 / 1000.0;
                                let max_ms = arrival_max_us as f64 / 1000.0;
                                format!(
                                    ", frame_arrival: avg={:.1}ms min={:.1}ms max={:.1}ms",
                                    avg_ms, min_ms, max_ms
                                )
                            } else {
                                String::new()
                            };
                            info!(
                                "decode summary: {} total frames, {:.1} avg fps (GPU), {}x{}, device='{}'{}",
                                total_frames, avg_fps, width, height, device_name, arrival_info
                            );
                            last_summary_at = Instant::now();
                            summary_frame_counter = 0;
                            arrival_min_us = u64::MAX;
                            arrival_max_us = 0;
                            arrival_sum_us = 0;
                            arrival_count = 0;
                        }
                        continue; // skip software decode
                    }
                    Err(crate::gpu_decode::GpuDecodeError::InvalidData(_)) => {
                        // Bad packet (e.g. config header) — skip to software decode
                        // for this packet, keep GPU decode active for the next one.
                        // The back slot is untouched and still holds its buffers.
                    }
                    Err(e) => {
                        // CUDA or nvJPEG error — count consecutive failures.
                        gpu_errors += 1;
                        if gpu_errors >= 3 {
                            warn!(
                                "nvJPEG failed {gpu_errors} times consecutively, \
                                 disabling GPU decode: {e}"
                            );
                            gpu_decoder = None;
                        } else {
                            warn!("nvJPEG decode error ({gpu_errors}/3): {e}");
                        }
                    }
                }
            }
        }

        // Software decode path (fallback or non-MJPEG formats)
        if let Err(error) = decoder.send_packet(&packet) {
            packet_errors += 1;
            if packet_errors <= 10 || packet_errors.is_multiple_of(50) {
                warn!(
                    "capture packet decode submission failed for '{}': {} (count={})",
                    device_name, error, packet_errors
                );
            }
            continue;
        }

        while decoder.receive_frame(&mut decoded).is_ok() {
            let frame = capture_frame_from_video(
                &decoded,
                &mut scaler,
                &mut scaled_frame,
                matches!(
                    decoded.format(),
                    format::Pixel::YUVJ422P
                        | format::Pixel::YUV422P
                        | format::Pixel::YUYV422
                        | format::Pixel::UYVY422
                ),
            )
            .map_err(|error| format!("failed to convert decoded frame: {error}"))?;
            let width = frame.width();
            let height = frame.height();
            total_frames += 1;

            if !logged_first_frame {
                logged_first_frame = true;
                info!(
                    "first decoded frame from '{}' => {}x{} {:?}",
                    device_name,
                    width,
                    height,
                    frame.format()
                );
            }

            frame_producer.write(frame);
            let _ = event_proxy.send_event(AppEvent::FrameReady);

            stats_frame_counter += 1;
            summary_frame_counter += 1;
            let elapsed = last_stats_at.elapsed();
            if elapsed >= Duration::from_millis(300) {
                let fps = stats_frame_counter as f32 / elapsed.as_secs_f32();
                let label = if decoder.id() == codec::Id::MJPEG {
                    format!("{capture_name} (SW)")
                } else {
                    capture_name.clone()
                };
                let _ = stats_tx.send(CaptureStats {
                    fps,
                    width,
                    height,
                    format_label: label,
                    nominal_rate,
                });
                last_stats_at = Instant::now();
                stats_frame_counter = 0;
            }
            let summary_elapsed = last_summary_at.elapsed();
            if summary_elapsed >= Duration::from_secs(30) {
                let avg_fps = summary_frame_counter as f64 / summary_elapsed.as_secs_f64();
                let arrival_info = if arrival_count > 0 {
                    let avg_ms = (arrival_sum_us as f64 / arrival_count as f64) / 1000.0;
                    let min_ms = arrival_min_us as f64 / 1000.0;
                    let max_ms = arrival_max_us as f64 / 1000.0;
                    format!(
                        ", frame_arrival: avg={:.1}ms min={:.1}ms max={:.1}ms",
                        avg_ms, min_ms, max_ms
                    )
                } else {
                    String::new()
                };
                info!(
                    "decode summary: {} total frames, {:.1} avg fps (SW), {}x{}, packet_errors={}, device='{}'{}",
                    total_frames, avg_fps, width, height, packet_errors, device_name, arrival_info
                );
                last_summary_at = Instant::now();
                summary_frame_counter = 0;
                arrival_min_us = u64::MAX;
                arrival_max_us = 0;
                arrival_sum_us = 0;
                arrival_count = 0;
            }
        }
    }

    Ok(())
}

fn capture_frame_from_video(
    frame: &Video,
    scaler: &mut Option<ScaleContext>,
    scaled_frame: &mut Video,
    prefer_yuvj422p: bool,
) -> Result<CaptureFrame, String> {
    let width = frame.width();
    let height = frame.height();

    let format = match frame.format() {
        format::Pixel::NV12 => PixelFormat::Nv12,
        format::Pixel::YUVJ422P | format::Pixel::YUV422P => PixelFormat::Yuvj422p,
        other => {
            let target = if prefer_yuvj422p {
                format::Pixel::YUV422P
            } else {
                format::Pixel::NV12
            };

            if scaler
                .as_ref()
                .map(|ctx| {
                    ctx.input().format != other
                        || ctx.input().width != width
                        || ctx.input().height != height
                        || ctx.output().format != target
                        || ctx.output().width != width
                        || ctx.output().height != height
                })
                .unwrap_or(true)
            {
                *scaler = Some(
                    ScaleContext::get(other, width, height, target, width, height, ScaleFlags::BILINEAR)
                        .map_err(|error| {
                            format!(
                                "unsupported pixel format {other:?} and failed to initialize converter to {target:?}: {error}"
                            )
                        })?,
                );
                *scaled_frame = Video::empty();
                warn!("converting decoder output from {other:?} to {target:?} for compatibility");
            }

            let Some(scale_ctx) = scaler.as_mut() else {
                return Err("scaler context unavailable".to_string());
            };

            let info =
                ColorInfo::from_ffmpeg(frame.color_space(), frame.color_range(), frame.format());
            let range = i32::from(info.resolve_range(ColorRange::Auto) == ColorRange::Full);
            let space = info.resolve_space(ColorSpace::Auto, height);
            let matrix = match space {
                ColorSpace::Bt601 => ffmpeg::ffi::SWS_CS_ITU601,
                ColorSpace::Bt2020 => ffmpeg::ffi::SWS_CS_BT2020,
                _ => ffmpeg::ffi::SWS_CS_ITU709,
            };
            // Preserve sample range through layout conversion. The shader owns
            // range expansion, including live manual overrides.
            let result = unsafe {
                let coefficients = ffmpeg::ffi::sws_getCoefficients(matrix);
                ffmpeg::ffi::sws_setColorspaceDetails(
                    scale_ctx.as_mut_ptr(),
                    coefficients,
                    range,
                    coefficients,
                    range,
                    0,
                    1 << 16,
                    1 << 16,
                )
            };
            if result < 0 {
                return Err("Failed to preserve colors during pixel conversion".into());
            }
            scale_ctx
                .run(frame, scaled_frame)
                .map_err(|error| format!("failed to convert frame via swscale: {error}"))?;
            let mut result = extract_supported_frame(scaled_frame)?;
            result.set_color_info(info);
            return Ok(result);
        }
    };

    extract_supported_frame_with_format(frame, format, width, height)
}

fn extract_supported_frame(frame: &Video) -> Result<CaptureFrame, String> {
    let width = frame.width();
    let height = frame.height();
    let format = match frame.format() {
        format::Pixel::NV12 => PixelFormat::Nv12,
        format::Pixel::YUVJ422P | format::Pixel::YUV422P => PixelFormat::Yuvj422p,
        other => return Err(format!("unsupported converted pixel format: {other:?}")),
    };
    extract_supported_frame_with_format(frame, format, width, height)
}

fn extract_supported_frame_with_format(
    frame: &Video,
    format: PixelFormat,
    width: u32,
    height: u32,
) -> Result<CaptureFrame, String> {
    let y_data = copy_plane(frame, 0, width as usize, height as usize);
    let u_data = match format {
        PixelFormat::Nv12 => copy_plane(frame, 1, width as usize, (height / 2) as usize),
        PixelFormat::Yuvj422p => copy_plane(frame, 1, (width / 2) as usize, height as usize),
    };
    let v_data = match format {
        PixelFormat::Nv12 => Vec::new(),
        PixelFormat::Yuvj422p => copy_plane(frame, 2, (width / 2) as usize, height as usize),
    };

    Ok(CaptureFrame::Cpu {
        width,
        height,
        format,
        color_info: ColorInfo::from_ffmpeg(
            frame.color_space(),
            frame.color_range(),
            frame.format(),
        ),
        y_data,
        u_data,
        v_data,
    })
}

fn actual_capture_format(codec: codec::Id, pixel: format::Pixel) -> VideoFormat {
    if codec == codec::Id::MJPEG {
        return VideoFormat::Mjpeg;
    }
    match pixel {
        format::Pixel::NV12 => VideoFormat::Nv12,
        format::Pixel::YUYV422 => VideoFormat::Yuy2,
        format::Pixel::UYVY422 => VideoFormat::Uyvy,
        format::Pixel::YUV420P => VideoFormat::Yuv420p,
        _ => VideoFormat::Auto,
    }
}

fn copy_plane(frame: &Video, plane: usize, row_bytes: usize, rows: usize) -> Vec<u8> {
    let stride = frame.stride(plane);
    let source = frame.data(plane);
    let mut output = vec![0_u8; row_bytes * rows];

    for row in 0..rows {
        let src_start = row * stride;
        let dst_start = row * row_bytes;
        output[dst_start..dst_start + row_bytes]
            .copy_from_slice(&source[src_start..src_start + row_bytes]);
    }

    output
}

fn extract_audio_samples_f32(frame: &ffmpeg::util::frame::audio::Audio, out: &mut Vec<f32>) {
    use cpal::Sample;
    use ffmpeg::format::sample::{Sample as FfSample, Type as SampleType};

    let samples = frame.samples();
    let channels = frame.channels() as usize;
    out.clear();
    if samples == 0 || channels == 0 {
        return;
    }
    out.reserve(samples * channels);

    match frame.format() {
        FfSample::U8(SampleType::Packed) => {
            let bytes = &frame.data(0)[..samples * channels];
            out.extend(bytes.iter().map(|&s| f32::from_sample(s)));
        }
        FfSample::U8(SampleType::Planar) => {
            for i in 0..samples {
                for ch in 0..channels {
                    out.push(f32::from_sample(frame.data(ch)[i]));
                }
            }
        }
        FfSample::I16(SampleType::Packed) => {
            let bytes = &frame.data(0)[..samples * channels * 2];
            for &chunk in bytes.as_chunks::<2>().0 {
                out.push(f32::from_sample(i16::from_ne_bytes(chunk)));
            }
        }
        FfSample::I16(SampleType::Planar) => {
            for i in 0..samples {
                let offset = i * 2;
                for ch in 0..channels {
                    let plane = frame.data(ch);
                    out.push(f32::from_sample(i16::from_ne_bytes([
                        plane[offset],
                        plane[offset + 1],
                    ])));
                }
            }
        }
        FfSample::I32(SampleType::Packed) => {
            let bytes = &frame.data(0)[..samples * channels * 4];
            for &chunk in bytes.as_chunks::<4>().0 {
                out.push(f32::from_sample(i32::from_ne_bytes(chunk)));
            }
        }
        FfSample::I32(SampleType::Planar) => {
            for i in 0..samples {
                let offset = i * 4;
                for ch in 0..channels {
                    let plane = frame.data(ch);
                    out.push(f32::from_sample(i32::from_ne_bytes([
                        plane[offset],
                        plane[offset + 1],
                        plane[offset + 2],
                        plane[offset + 3],
                    ])));
                }
            }
        }
        FfSample::F32(SampleType::Packed) => {
            let bytes = &frame.data(0)[..samples * channels * 4];
            for &chunk in bytes.as_chunks::<4>().0 {
                out.push(f32::from_ne_bytes(chunk));
            }
        }
        FfSample::F32(SampleType::Planar) => {
            for i in 0..samples {
                let offset = i * 4;
                for ch in 0..channels {
                    let plane = frame.data(ch);
                    out.push(f32::from_ne_bytes([
                        plane[offset],
                        plane[offset + 1],
                        plane[offset + 2],
                        plane[offset + 3],
                    ]));
                }
            }
        }
        _ => {}
    }
}

#[cfg(target_os = "windows")]
fn find_dshow_format() -> Option<ffmpeg::Format> {
    device::input::video().find(|format| match format {
        ffmpeg::Format::Input(input) => input.name() == "dshow",
        ffmpeg::Format::Output(_) => false,
    })
}

#[cfg(target_os = "linux")]
fn find_v4l2_format() -> Option<ffmpeg::Format> {
    device::input::video().find(|format| match format {
        ffmpeg::Format::Input(input) => input
            .name()
            .split(',')
            .any(|name| matches!(name, "video4linux2" | "v4l2")),
        ffmpeg::Format::Output(_) => false,
    })
}

fn find_input_format() -> Result<(ffmpeg::Format, String), String> {
    #[cfg(target_os = "windows")]
    {
        let format = find_dshow_format()
            .ok_or_else(|| "DirectShow input format was not found in FFmpeg".to_string())?;
        Ok((format, "DirectShow".to_string()))
    }
    #[cfg(target_os = "linux")]
    {
        let format = find_v4l2_format().ok_or_else(|| {
            "V4L2 (video4linux2) input format was not found in FFmpeg".to_string()
        })?;
        Ok((format, "V4L2".to_string()))
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        Err("Unsupported operating system for video capture".to_string())
    }
}

#[cfg(target_os = "linux")]
pub fn resolve_v4l2_node(device_name: &str) -> String {
    if device_name.starts_with("/dev/") {
        return device_name.to_string();
    }
    if let Some(start) = device_name.rfind('(') {
        if let Some(end) = device_name.rfind(')') {
            let candidate = &device_name[start + 1..end];
            if candidate.starts_with("/dev/") {
                return candidate.to_string();
            }
        }
    }
    device_name.to_string()
}

fn generate_test_frame(
    width: u32,
    height: u32,
    frame_index: u64,
    format: PixelFormat,
) -> CaptureFrame {
    let y_len = (width * height) as usize;
    let chroma_width = (width / 2) as usize;
    let chroma_height_420 = (height / 2) as usize;
    let chroma_height_422 = height as usize;
    let mut y_data = vec![0_u8; y_len];

    for y in 0..height as usize {
        for x in 0..width as usize {
            let xf = x as f32 / width as f32;
            let yf = y as f32 / height as f32;
            let phase = (frame_index as f32 * 0.04) + xf * PI * 2.0;
            let wave = ((phase.sin() * 0.5) + 0.5) * 80.0;
            let sweep = (((yf * 255.0) + frame_index as f32 * 2.0) as i32).rem_euclid(256) as u8;
            let bars = (((x * 8) / width as usize) * 28) as u8;
            y_data[y * width as usize + x] = sweep
                .saturating_div(2)
                .saturating_add(bars / 2)
                .saturating_add(wave as u8 / 2);
        }
    }

    match format {
        PixelFormat::Nv12 => {
            let mut uv_data = vec![0_u8; chroma_width * chroma_height_420 * 2];
            for y in 0..chroma_height_420 {
                for x in 0..chroma_width {
                    let index = (y * chroma_width + x) * 2;
                    let x_phase = ((x as f32 / chroma_width as f32) * PI * 2.0
                        + frame_index as f32 * 0.03)
                        .sin();
                    let y_phase = ((y as f32 / chroma_height_420 as f32) * PI * 2.0
                        + frame_index as f32 * 0.05)
                        .cos();
                    uv_data[index] = ((x_phase * 0.5 + 0.5) * 255.0) as u8;
                    uv_data[index + 1] = ((y_phase * 0.5 + 0.5) * 255.0) as u8;
                }
            }

            CaptureFrame::Cpu {
                width,
                height,
                format,
                color_info: ColorInfo {
                    space: Some(ColorSpace::Rec709),
                    range: Some(ColorRange::Full),
                    ..ColorInfo::default()
                },
                y_data,
                u_data: uv_data,
                v_data: Vec::new(),
            }
        }
        PixelFormat::Yuvj422p => {
            let mut u_data = vec![0_u8; chroma_width * chroma_height_422];
            let mut v_data = vec![0_u8; chroma_width * chroma_height_422];

            for y in 0..chroma_height_422 {
                for x in 0..chroma_width {
                    let index = y * chroma_width + x;
                    let x_phase = ((x as f32 / chroma_width as f32) * PI * 2.0
                        + frame_index as f32 * 0.06)
                        .sin();
                    let y_phase = ((y as f32 / chroma_height_422 as f32) * PI * 2.0
                        + frame_index as f32 * 0.02)
                        .cos();
                    u_data[index] = ((x_phase * 0.5 + 0.5) * 255.0) as u8;
                    v_data[index] = ((y_phase * 0.5 + 0.5) * 255.0) as u8;
                }
            }

            CaptureFrame::Cpu {
                width,
                height,
                format,
                color_info: ColorInfo {
                    space: Some(ColorSpace::Rec709),
                    range: Some(ColorRange::Full),
                    ..ColorInfo::default()
                },
                y_data,
                u_data,
                v_data,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_packed_i16_audio_samples_to_interleaved_f32() {
        let mut frame = ffmpeg::util::frame::audio::Audio::new(
            ffmpeg::format::sample::Sample::I16(ffmpeg::format::sample::Type::Packed),
            2,
            ffmpeg::ChannelLayout::STEREO,
        );
        let samples = [0i16, i16::MAX, i16::MIN, 0i16];
        for (i, sample) in samples.iter().enumerate() {
            frame.data_mut(0)[i * 2..i * 2 + 2].copy_from_slice(&sample.to_ne_bytes());
        }
        let mut out = Vec::new();
        extract_audio_samples_f32(&frame, &mut out);
        assert_eq!(out.len(), 4);
        assert!((out[0] - 0.0).abs() < 1e-5);
        assert!((out[1] - 1.0).abs() < 1e-4);
        assert!((out[2] - (-1.0)).abs() < 1e-4);
        assert!((out[3] - 0.0).abs() < 1e-5);
    }

    #[test]
    #[allow(irrefutable_let_patterns)]
    fn packed_conversion_preserves_limited_range_samples() {
        let mut input = Video::new(format::Pixel::YUYV422, 2, 2);
        input.set_color_range(ffmpeg::color::Range::MPEG);
        input.set_color_space(ffmpeg::color::Space::BT709);
        let stride = input.stride(0);
        for row in 0..2 {
            input.data_mut(0)[row * stride..row * stride + 4].copy_from_slice(&[16, 128, 235, 128]);
        }
        let frame = capture_frame_from_video(&input, &mut None, &mut Video::empty(), true).unwrap();
        let CaptureFrame::Cpu {
            y_data, color_info, ..
        } = frame
        else {
            panic!("Expected CPU frame")
        };
        assert_eq!(y_data, [16, 235, 16, 235]);
        assert_eq!(color_info.range, Some(ColorRange::Limited));
        assert_eq!(color_info.space, Some(ColorSpace::Rec709));
    }

    #[test]
    #[allow(irrefutable_let_patterns)]
    fn planar_conversion_preserves_full_range_metadata_and_samples() {
        let mut input = Video::new(format::Pixel::YUVJ420P, 2, 2);
        input.set_color_range(ffmpeg::color::Range::JPEG);
        let stride = input.stride(0);
        for row in 0..2 {
            input.data_mut(0)[row * stride..row * stride + 2].copy_from_slice(&[0, 255]);
        }
        input.data_mut(1)[0] = 128;
        input.data_mut(2)[0] = 128;
        let frame =
            capture_frame_from_video(&input, &mut None, &mut Video::empty(), false).unwrap();
        let CaptureFrame::Cpu {
            y_data, color_info, ..
        } = frame
        else {
            panic!("Expected CPU frame")
        };
        assert_eq!(y_data, [0, 255, 0, 255]);
        assert_eq!(color_info.range, Some(ColorRange::Full));
    }

    #[test]
    fn software_decoder_handles_capture_mjpeg() {
        let codec =
            codec::decoder::find(codec::Id::MJPEG).expect("FFmpeg must include MJPEG decoding");
        let mut decoder = codec::Context::new_with_codec(codec)
            .decoder()
            .video()
            .unwrap();
        let packet = ffmpeg::Packet::copy(include_bytes!("../assets/tests/nvjpeg-422.jpg"));
        decoder.send_packet(&packet).unwrap();
        let mut frame = Video::empty();
        decoder.receive_frame(&mut frame).unwrap();
        assert_eq!((frame.width(), frame.height()), (16, 8));
        assert_eq!(frame.format(), ffmpeg::format::Pixel::YUVJ422P);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn discovers_installed_v4l2_format() {
        ffmpeg::init().unwrap();
        assert!(
            find_v4l2_format().is_some(),
            "FFmpeg must recognize its V4L2 aliases"
        );
    }

    #[test]
    #[allow(irrefutable_let_patterns)]
    fn generates_expected_plane_sizes_for_nv12() {
        let frame = generate_test_frame(1280, 720, 0, PixelFormat::Nv12);
        let CaptureFrame::Cpu {
            y_data,
            u_data,
            v_data,
            ..
        } = &frame
        else {
            panic!("expected Cpu frame");
        };
        assert_eq!(y_data.len(), 1280 * 720);
        assert_eq!(u_data.len(), 1280 * 720 / 2);
        assert!(v_data.is_empty());
    }

    #[test]
    #[allow(irrefutable_let_patterns)]
    fn generates_expected_plane_sizes_for_yuvj422p() {
        let frame = generate_test_frame(1280, 720, 0, PixelFormat::Yuvj422p);
        let CaptureFrame::Cpu {
            y_data,
            u_data,
            v_data,
            ..
        } = &frame
        else {
            panic!("expected Cpu frame");
        };
        assert_eq!(y_data.len(), 1280 * 720);
        assert_eq!(u_data.len(), 640 * 720);
        assert_eq!(v_data.len(), 640 * 720);
    }
}
