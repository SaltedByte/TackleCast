//! Deterministic replay of a recorded clip through the real pipeline.
//!
//! `TackleCast-FrameGen.exe --fg-replay=<clip folder>` runs every recorded
//! frame through exactly what the live app uses — DX12 shared buffers, the
//! CUDA import, NVIDIA FRUC, the duplicate check, the motion estimate and
//! the correction passes — once per mode, each with a fresh pipeline:
//!
//! * `native` — no generation (the in-between slot shows the previous frame)
//! * `raw`    — NVIDIA FRUC output unchanged
//! * `dynamic`, `advanced` — the artifact-reduction modes
//! * `zones`  — the v4-style fixed corner regions (for comparison only)
//!
//! Output (`<clip>/replay_<time>/` unless `--fg-replay-out=` is given):
//! * `report.json`, `summary.txt` — per mode: frames, duplicates, FRUC
//!   repeats, transition skips, corrected area (mean/p95/max, and how often
//!   it exceeded 5%), FRUC and correction GPU time (mean/p95/p99), source
//!   timing from the recorded timestamps;
//! * `intervals/iNNNNN_<mode>.png` + `iNNNNN_A.png` / `_B.png` /
//!   `_diff_<mode>.png` (pixels the mode changed vs raw FRUC) for every
//!   `--fg-replay-every=N`th generated frame, and `iNNNNN_compare.png`
//!   (half size: A | raw | dynamic / B | advanced | changed pixels);
//! * `sequence/<mode>/NNNNNN.png` — half-size frames at the 60 FPS output
//!   cadence for `--fg-replay-sequence=SECONDS` (default 3) and
//!   `index.html` to play them side by side or step through them.
//!
//! Everything is computed offline; nothing here runs in the live app.

use super::duplicate::DuplicateThresholds;
use super::recorder::Clip;
use super::stats::{summarize, DurationSummary};
use super::{BackendEvent, GpuFence, Job, Outcome, OUTPUT_SLOTS, SOURCE_SLOTS};
use crate::render::{
    FgGpu, FgProtection, FG_FORMAT, VIEW_FINAL, ZONE_BOTTOM_LEFT, ZONE_BOTTOM_RIGHT,
};
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Native,
    #[default]
    Raw,
    Dynamic,
    Advanced,
    Zones,
    /// HUD guard only (no other corrections).
    Hud,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Raw => "raw",
            Self::Dynamic => "dynamic",
            Self::Advanced => "advanced",
            Self::Zones => "zones",
            Self::Hud => "hud",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "native" => Self::Native,
            "raw" => Self::Raw,
            "dynamic" => Self::Dynamic,
            "advanced" => Self::Advanced,
            "zones" => Self::Zones,
            "hud" => Self::Hud,
            _ => return None,
        })
    }

    fn protection(self) -> FgProtection {
        let dynamic = FgProtection {
            fix_static: true,
            fix_motion: true,
            fix_edges: true,
            fix_fades: true,
            view: VIEW_FINAL,
            ..Default::default()
        };
        match self {
            Self::Native | Self::Raw => FgProtection::default(),
            Self::Dynamic => dynamic,
            Self::Advanced => FgProtection {
                hud_input_removal: true,
                ..dynamic
            },
            Self::Hud => FgProtection {
                hud_guard: true,
                view: VIEW_FINAL,
                ..Default::default()
            },
            Self::Zones => FgProtection {
                zones: ZONE_BOTTOM_LEFT | ZONE_BOTTOM_RIGHT,
                zone_width: 0.15,
                zone_height: 0.25,
                ..Default::default()
            },
        }
    }
}

pub struct ReplayArgs {
    pub clip: PathBuf,
    pub out_dir: Option<PathBuf>,
    pub runtime_dir: String,
    pub modes: Vec<Mode>,
    /// Export images for every Nth generated frame (0 = none).
    pub export_every: usize,
    /// Only export intervals whose frame index is in this range
    /// (`--fg-replay-range=START-END`, inclusive).
    pub export_range: std::ops::RangeInclusive<usize>,
    /// Seconds of 60 FPS output frames to export, from the clip start.
    pub sequence_seconds: f64,
    pub sequence_start: f64,
    /// Scene-cut threshold (as the live setting).
    pub skip_difference: f32,
    /// FRUC timestamp scale (1.0 = frame units, 33.33 = ms like NVIDIA's
    /// sample): `--fruc-ts=ms|frames`.
    pub timestamp_scale: f64,
    /// Interpolation engine: `--fg-engine=fruc|rife` (default fruc).
    pub rife: bool,
    /// RIFE model stem: `--fg-rife-model=rife46fmh` (default rife426fmh).
    pub rife_model: String,
}

impl ReplayArgs {
    pub fn from_cli(args: &[String]) -> Option<Self> {
        let value = |name: &str| {
            args.iter()
                .find_map(|a| a.strip_prefix(&format!("{name}=")).map(str::to_string))
        };
        let clip = PathBuf::from(value("--fg-replay")?);
        let modes = value("--fg-replay-modes")
            .map(|v| v.split(',').filter_map(Mode::parse).collect::<Vec<_>>())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| {
                vec![
                    Mode::Native,
                    Mode::Raw,
                    Mode::Dynamic,
                    Mode::Advanced,
                    Mode::Zones,
                ]
            });
        Some(Self {
            clip,
            out_dir: value("--fg-replay-out").map(PathBuf::from),
            runtime_dir: value("--fruc-runtime").unwrap_or_default(),
            modes,
            export_every: value("--fg-replay-every")
                .and_then(|v| v.parse().ok())
                .unwrap_or(15),
            export_range: value("--fg-replay-range")
                .and_then(|v| {
                    let (a, b) = v.split_once('-')?;
                    Some(a.trim().parse().ok()?..=b.trim().parse().ok()?)
                })
                .unwrap_or(0..=usize::MAX),
            sequence_seconds: value("--fg-replay-sequence")
                .and_then(|v| v.parse().ok())
                .unwrap_or(3.0),
            sequence_start: value("--fg-replay-from")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
            skip_difference: 32.0,
            timestamp_scale: match value("--fruc-ts").as_deref() {
                Some("ms") => 1000.0 / 30.0,
                Some("frames") => 1.0,
                _ => super::worker::default_timestamp_scale(),
            },
            rife: value("--fg-engine").as_deref() == Some("rife"),
            rife_model: value("--fg-rife-model").unwrap_or_else(|| "rife426fmh".into()),
        })
    }
}

#[derive(Debug, Default, Serialize)]
struct ModeReport {
    mode: Mode,
    frames: usize,
    unique: usize,
    duplicates: usize,
    generated: usize,
    fruc_repeats: usize,
    fruc_failures: usize,
    transition_skips: usize,
    /// Share of the frame the correction pass changed (shader counters).
    corrected_pct: Option<DurationSummaryPct>,
    /// Generated frames with more than 5% of the screen corrected.
    frames_over_5pct: usize,
    /// Share of pixels that differ from raw FRUC by more than 2 levels
    /// (measured on the images, independent of the shader).
    changed_vs_raw_pct: Option<DurationSummaryPct>,
    fruc_ms: Option<DurationSummary>,
    /// GPU time of the correction passes (timestamp queries).
    correction_gpu_ms: Option<DurationSummary>,
    /// GPU time of the capture-side passes (profiles, HUD removal).
    capture_gpu_ms: Option<DurationSummary>,
    /// Wall time from publish submission to GPU idle (fallback timing).
    publish_wall_ms: Option<DurationSummary>,
    /// FRUC + correction above half a source frame (would risk a late
    /// generated frame at the default pipeline delay).
    over_half_frame_budget: usize,
    error: Option<String>,
}

/// Percent statistics (reusing the duration summary shape: values in %).
type DurationSummaryPct = DurationSummary;

#[derive(Debug, Serialize)]
struct SourceTiming {
    frames: usize,
    duration_s: f64,
    interval_ms: DurationSummary,
    /// Intervals over 1.5x the median (missed / late source frames).
    long_intervals: usize,
    frames_dropped_while_recording: u32,
}

#[derive(Debug, Serialize)]
struct Report {
    clip: String,
    width: u32,
    height: u32,
    gpu: String,
    engine: String,
    timestamp_queries: bool,
    source: SourceTiming,
    modes: Vec<ModeReport>,
    notes: Vec<String>,
}

pub fn run(args: ReplayArgs) -> i32 {
    super::selftest::attach_parent_console();
    let clip = match Clip::open(&args.clip) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("replay: {e}");
            return 2;
        }
    };
    let out_dir = args.out_dir.clone().unwrap_or_else(|| {
        let stamp = time::OffsetDateTime::now_local()
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
            .format(time::macros::format_description!(
                "[year][month][day]_[hour][minute][second]"
            ))
            .unwrap_or_else(|_| "out".into());
        args.clip.join(format!("replay_{stamp}"))
    });
    for sub in ["intervals", "sequence"] {
        if let Err(e) = std::fs::create_dir_all(out_dir.join(sub)) {
            eprintln!("replay: cannot create {}: {e}", out_dir.display());
            return 2;
        }
    }
    println!(
        "Replaying {} ({} frames, {}x{}) -> {}",
        clip.dir.display(),
        clip.frames.len(),
        clip.meta.width,
        clip.meta.height,
        out_dir.display()
    );

    let source = source_timing(&clip);
    let mut report = Report {
        clip: clip.dir.display().to_string(),
        width: clip.meta.width,
        height: clip.meta.height,
        gpu: String::new(),
        engine: if args.rife {
            format!("RIFE {} (DirectML)", args.rife_model)
        } else {
            "NVIDIA FRUC".into()
        },
        timestamp_queries: false,
        source,
        modes: Vec::new(),
        notes: vec![
            "Each mode runs on a fresh pipeline with identical input frames.".into(),
            "native: the in-between slot shows the previous real frame (30 FPS).".into(),
            "zones: v4-style fixed bottom corners from the real frame (comparison only).".into(),
            "Colour conversion uses neutral picture adjustments; artifacts are unaffected.".into(),
        ],
    };
    let mut exported: Vec<u32> = Vec::new();
    for mode in &args.modes {
        println!("  mode {} ...", mode.name());
        let started = Instant::now();
        let r = match run_mode(&args, &clip, *mode, &out_dir, &mut report, &mut exported) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  mode {}: {e}", mode.name());
                ModeReport {
                    mode: *mode,
                    error: Some(e),
                    ..Default::default()
                }
            }
        };
        println!(
            "  mode {} done in {:.1} s: {} generated, corrected area mean {:.2}%",
            mode.name(),
            started.elapsed().as_secs_f64(),
            r.generated,
            r.corrected_pct.as_ref().map(|s| s.mean_ms).unwrap_or(0.0)
        );
        report.modes.push(r);
    }
    exported.sort_unstable();
    exported.dedup();
    build_comparisons(&out_dir, &exported, &args.modes);
    write_index_html(&out_dir, &exported, &args.modes, &clip);
    let summary = summary_text(&report);
    let _ = std::fs::write(out_dir.join("summary.txt"), &summary);
    let _ = std::fs::write(
        out_dir.join("report.json"),
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    );
    println!("{summary}");
    println!("Open {} to compare.", out_dir.join("index.html").display());
    if report.modes.iter().any(|m| m.error.is_some()) {
        1
    } else {
        0
    }
}

fn source_timing(clip: &Clip) -> SourceTiming {
    let intervals: Vec<f32> = clip
        .frames
        .windows(2)
        .map(|w| ((w[1].1 - w[0].1) * 1000.0) as f32)
        .collect();
    let mut sorted = intervals.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(33.3);
    SourceTiming {
        frames: clip.frames.len(),
        duration_s: clip.frames.last().map(|f| f.1).unwrap_or(0.0),
        long_intervals: intervals.iter().filter(|v| **v > median * 1.5).count(),
        interval_ms: summarize(intervals.into_iter()),
        frames_dropped_while_recording: clip.meta.frames_dropped,
    }
}

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    name: String,
}

fn open_gpu() -> Result<Gpu, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .map_err(|e| format!("no DX12 adapter: {e}"))?;
    let name = adapter.get_info().name;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("tacklecast-replay"),
        required_features: adapter.features() & wgpu::Features::TIMESTAMP_QUERY,
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        ..Default::default()
    }))
    .map_err(|e| format!("request_device: {e}"))?;
    Ok(Gpu {
        device,
        queue,
        name,
    })
}

fn write_rgba(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8], w: u32, h: u32) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
}

fn map_read(device: &wgpu::Device, buffer: &wgpu::Buffer) -> Result<Vec<u8>, String> {
    let slice = buffer.slice(..);
    let (tx, rx) = crossbeam_channel::bounded(1);
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("map: {e}")),
        Err(_) => return Err("map timed out".into()),
    }
    let data = slice.get_mapped_range().to_vec();
    buffer.unmap();
    Ok(data)
}

/// Copy a pitched RGBA buffer or a texture into packed RGBA8.
enum Src<'a> {
    Buffer(&'a wgpu::Buffer, u32),
    Texture(&'a wgpu::Texture),
}

fn read_rgba(gpu: &Gpu, src: Src<'_>, w: u32, h: u32) -> Result<Vec<u8>, String> {
    let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("tacklecast-replay-readback"),
        size: row as u64 * h as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    let pitch = match src {
        Src::Buffer(buffer, pitch) => {
            encoder.copy_buffer_to_buffer(
                buffer,
                0,
                &staging,
                0,
                (pitch as u64 * h as u64).min(staging.size()),
            );
            pitch
        }
        Src::Texture(texture) => {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &staging,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row),
                        rows_per_image: Some(h),
                    },
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            row
        }
    };
    gpu.queue.submit(std::iter::once(encoder.finish()));
    let data = map_read(&gpu.device, &staging)?;
    let mut packed = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        let s = y * pitch as usize;
        let d = y * (w * 4) as usize;
        packed[d..d + (w * 4) as usize].copy_from_slice(&data[s..s + (w * 4) as usize]);
    }
    Ok(packed)
}

/// Percent of pixels whose max channel difference exceeds `levels`.
fn changed_pct(a: &[u8], b: &[u8], levels: u8) -> f32 {
    let n = a.len() / 4;
    let changed = a
        .chunks_exact(4)
        .zip(b.chunks_exact(4))
        .filter(|(p, q)| (0..3).any(|c| p[c].abs_diff(q[c]) > levels))
        .count();
    changed as f32 * 100.0 / n.max(1) as f32
}

/// Visualise where `final_` differs from `raw`: grey raw image, changed
/// pixels in magenta.
fn diff_image(raw: &[u8], final_: &[u8]) -> Vec<u8> {
    raw.chunks_exact(4)
        .zip(final_.chunks_exact(4))
        .flat_map(|(r, f)| {
            let changed = (0..3).any(|c| r[c].abs_diff(f[c]) > 2);
            let grey = ((r[0] as u16 + r[1] as u16 + r[2] as u16) / 6) as u8;
            if changed {
                [255, 40, 255, 255]
            } else {
                [grey, grey, grey, 255]
            }
        })
        .collect()
}

fn half(data: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    let (hw, hh) = (w / 2, h / 2);
    let mut out = vec![0u8; (hw * hh * 4) as usize];
    for y in 0..hh {
        for x in 0..hw {
            for c in 0..4 {
                let px = |dx: u32, dy: u32| {
                    data[(((2 * y + dy) * w + 2 * x + dx) * 4 + c) as usize] as u16
                };
                out[((y * hw + x) * 4 + c) as usize] =
                    ((px(0, 0) + px(1, 0) + px(0, 1) + px(1, 1)) / 4) as u8;
            }
        }
    }
    (out, hw, hh)
}

fn save_png(path: &Path, data: &[u8], w: u32, h: u32) {
    if let Err(e) = image::save_buffer(path, data, w, h, image::ColorType::Rgba8) {
        eprintln!("could not save {}: {e}", path.display());
    }
}

fn run_mode(
    args: &ReplayArgs,
    clip: &Clip,
    mode: Mode,
    out_dir: &Path,
    report: &mut Report,
    exported: &mut Vec<u32>,
) -> Result<ModeReport, String> {
    let (w, h) = (clip.meta.width, clip.meta.height);
    let mut r = ModeReport {
        mode,
        frames: clip.frames.len(),
        ..Default::default()
    };
    let seq_dir = out_dir.join("sequence").join(mode.name());
    let _ = std::fs::create_dir_all(&seq_dir);
    let seq_range = args.sequence_start..args.sequence_start + args.sequence_seconds;
    let mut seq_index = 0u32;
    let save_seq = |data: &[u8], seq_index: &mut u32| {
        let (hd, hw, hh) = half(data, w, h);
        save_png(&seq_dir.join(format!("{:06}.png", *seq_index)), &hd, hw, hh);
        *seq_index += 1;
    };

    if mode == Mode::Native {
        // No generation: each real frame is shown for two output frames.
        for (i, (index, t)) in clip.frames.iter().enumerate() {
            let frame = clip.load_rgba(*index)?;
            if seq_range.contains(t) {
                save_seq(&frame, &mut seq_index);
                save_seq(&frame, &mut seq_index);
            }
            if args.export_every > 0
                && i > 0
                && i % args.export_every == 0
                && args.export_range.contains(&(*index as usize))
            {
                save_png(
                    &out_dir
                        .join("intervals")
                        .join(format!("i{index:05}_native.png")),
                    &clip.load_rgba(clip.frames[i - 1].0)?,
                    w,
                    h,
                );
            }
        }
        r.unique = clip.frames.len();
        return Ok(r);
    }

    let gpu = open_gpu()?;
    report.gpu = gpu.name.clone();
    report.timestamp_queries = gpu
        .device
        .features()
        .contains(wgpu::Features::TIMESTAMP_QUERY);
    let (mut fg, import) = FgGpu::new(&gpu.device, FG_FORMAT, w, h, SOURCE_SLOTS, OUTPUT_SLOTS)?;
    let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<()>();
    let waker: Box<dyn Fn() + Send> = Box::new(move || {
        let _ = wake_tx.send(());
    });
    let mut backend = if args.rife {
        super::rife::spawn(
            import,
            gpu.device.clone(),
            super::rife::RifeConfig {
                dir: super::rife::default_dir(),
                model: args.rife_model.clone(),
            },
            waker,
        )
    } else {
        super::worker::spawn(
            import,
            gpu.device.clone(),
            super::worker::WorkerConfig {
                runtime_dir: args.runtime_dir.clone(),
                require_signature: true,
                timestamp_scale: args.timestamp_scale,
            },
            waker,
        )
    };
    let wait_event =
        |backend: &mut Box<dyn super::Backend>, limit: Duration| -> Option<BackendEvent> {
            let deadline = Instant::now() + limit;
            loop {
                if let Some(e) = backend.poll() {
                    return Some(e);
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                let _ = wake_rx.recv_timeout(left.min(Duration::from_millis(50)));
            }
        };
    match wait_event(&mut backend, Duration::from_secs(120)) {
        Some(BackendEvent::Ready(info)) => fg.flow_output = info.flow_output,
        Some(BackendEvent::InitFailed(e)) | Some(BackendEvent::Fatal(e)) => {
            backend.shutdown(Box::new(fg), Some(Duration::from_secs(3)));
            return Err(e);
        }
        other => {
            backend.shutdown(Box::new(fg), Some(Duration::from_secs(3)));
            return Err(format!("FRUC did not become ready: {other:?}"));
        }
    }

    let protection = mode.protection();
    let mut csv = std::io::BufWriter::new(
        std::fs::File::create(out_dir.join(format!("frames_{}.csv", mode.name())))
            .map_err(|e| format!("frames csv: {e}"))?,
    );
    let _ = writeln!(
        csv,
        "index,arrival_ms,fruc_ms,motion_dx,motion_dy,motion_residual,diff_mean,diff_max,\
         outcome,fruc_repeat,cut,changed_vs_raw_pct,corrected_pct"
    );
    let half_frame_ms = (report.source.interval_ms.p50_ms / 2.0).max(8.0);
    let mut fruc_ms = Vec::new();
    let mut publish_ms = Vec::new();
    let mut corrected = Vec::new();
    let mut changed = Vec::new();
    let mut last_unique: Option<(usize, u32, Vec<u8>)> = None;
    // Camera motion of the latest pair, used by the capture-side HUD guard
    // exactly like the live controller does.
    let mut last_motion: Option<[f32; 2]> = None;
    let mut next_slot = 0usize;
    let mut in_transition = false;
    let mut result: Result<(), String> = Ok(());

    for (frame_no, (index, t)) in clip.frames.iter().enumerate() {
        let seq = frame_no as u64 + 1;
        let frame = clip.load_rgba(*index)?;
        let reference = last_unique.as_ref().map(|u| u.0);
        let slot = loop {
            let s = next_slot % SOURCE_SLOTS;
            next_slot += 1;
            if Some(s) != reference {
                break s;
            }
        };
        let output = frame_no % OUTPUT_SLOTS;
        write_rgba(&gpu.queue, &fg.sources[slot].texture, &frame, w, h);
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        let mut capture_protection = protection;
        capture_protection.motion = last_motion;
        fg.encode_transfer(
            &gpu.device,
            &gpu.queue,
            &mut encoder,
            slot,
            reference,
            capture_protection,
        );
        let submission = gpu.queue.submit(std::iter::once(encoder.finish()));
        fg.after_capture_submit();
        if let Err(e) = backend.submit(Job {
            seq,
            source: slot,
            output,
            fence: GpuFence(Some(submission)),
            has_reference: reference.is_some(),
            fruc: true,
            skip_warp: reference.is_none(),
            thresholds: DuplicateThresholds::default(),
        }) {
            result = Err(e);
            break;
        }
        let done = match wait_event(&mut backend, Duration::from_secs(10)) {
            Some(BackendEvent::Done(d)) => d,
            Some(BackendEvent::Fatal(e)) => {
                result = Err(e);
                break;
            }
            other => {
                result = Err(format!("unexpected worker event: {other:?}"));
                break;
            }
        };
        last_motion = done.motion.filter(|m| m.residual < 0.5).map(|m| m.shift);
        let in_seq = seq_range.contains(t);
        let (dmean, dmax) = match &done.outcome {
            Outcome::Duplicate(s)
            | Outcome::Interpolated { stats: Some(s), .. }
            | Outcome::Unique { stats: Some(s) } => (s.mean_levels(), s.max_levels()),
            _ => (f32::NAN, f32::NAN),
        };
        let row_head = format!(
            "{index},{:.3},{:.3},{:.1},{:.1},{:.3},{:.2},{:.1}",
            t * 1000.0,
            done.fruc_time.as_secs_f64() * 1000.0,
            done.motion.map(|m| m.shift[0]).unwrap_or(f32::NAN),
            done.motion.map(|m| m.shift[1]).unwrap_or(f32::NAN),
            done.motion.map(|m| m.residual).unwrap_or(f32::NAN),
            dmean,
            dmax,
        );
        match &done.outcome {
            Outcome::Duplicate(_) => {
                r.duplicates += 1;
                let _ = writeln!(csv, "{row_head},duplicate,0,0,,");
                continue;
            }
            Outcome::Failed(_) => {
                let _ = writeln!(csv, "{row_head},failed,0,0,,");
                r.fruc_failures += 1;
                r.unique += 1;
                if in_seq {
                    save_seq(&frame, &mut seq_index);
                    save_seq(&frame, &mut seq_index);
                }
                last_unique = Some((slot, *index, frame));
                continue;
            }
            Outcome::Unique { .. } => {
                let _ = writeln!(csv, "{row_head},unique,0,0,,");
                r.unique += 1;
                if in_seq {
                    save_seq(&frame, &mut seq_index);
                    save_seq(&frame, &mut seq_index);
                }
                last_unique = Some((slot, *index, frame));
                continue;
            }
            Outcome::Interpolated { repeated, stats } => {
                r.unique += 1;
                fruc_ms.push(done.fruc_time.as_secs_f32() * 1000.0);
                if *repeated {
                    r.fruc_repeats += 1;
                }
                let cut = stats.is_some_and(|s| {
                    super::transition_rule(
                        s.mean_levels(),
                        done.motion.map(|m| m.residual),
                        in_transition,
                        args.skip_difference,
                    )
                });
                in_transition = cut;
                let Some((prev_slot, _, prev_frame)) = last_unique.as_ref() else {
                    last_unique = Some((slot, *index, frame));
                    continue;
                };
                // Publish through the same path as the live app.
                let mut prot = protection;
                prot.motion = done.motion.map(|m| m.shift);
                let mut encoder = gpu.device.create_command_encoder(&Default::default());
                let started = Instant::now();
                if fg.flow_output {
                    fg.encode_flow_warp(
                        &gpu.device,
                        &gpu.queue,
                        &mut encoder,
                        (output, *prev_slot, slot),
                    );
                }
                if prot.any() {
                    fg.encode_publish_corrected(
                        &gpu.device,
                        &gpu.queue,
                        &mut encoder,
                        (output, *prev_slot, slot),
                        prot,
                    );
                } else {
                    fg.encode_publish_output(&mut encoder, output);
                }
                gpu.queue.submit(std::iter::once(encoder.finish()));
                fg.after_publish_submit();
                let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
                let wall = started.elapsed().as_secs_f32() * 1000.0;
                publish_ms.push(wall);
                fg.poll_fix_stats();
                if prot.any() {
                    if let Some(stats) = fg.fix_report().last {
                        corrected.push(stats.corrected_pct);
                        if stats.corrected_pct > 5.0 {
                            r.frames_over_5pct += 1;
                        }
                    }
                }
                if done.fruc_time.as_secs_f32() * 1000.0 + wall > half_frame_ms {
                    r.over_half_frame_budget += 1;
                }
                let raw = read_rgba(
                    &gpu,
                    Src::Buffer(&fg.outputs[output].buffer, fg.pitch),
                    w,
                    h,
                )?;
                // As live: a cut or a FRUC frame repetition is not shown
                // (the previous real frame stays on screen).
                let shown = !cut && !*repeated;
                let final_ = if shown {
                    read_rgba(&gpu, Src::Texture(&fg.outputs[output].texture), w, h)?
                } else {
                    if cut {
                        r.transition_skips += 1;
                    }
                    prev_frame.clone()
                };
                if shown {
                    r.generated += 1;
                }
                let changed_now = changed_pct(&raw, &final_, 2);
                changed.push(changed_now);
                let _ = writeln!(
                    csv,
                    "{row_head},interpolated,{},{},{:.3},{}",
                    u8::from(*repeated),
                    u8::from(cut),
                    changed_now,
                    fg.fix_report()
                        .last
                        .filter(|_| prot.any())
                        .map(|s| format!("{:.3}", s.corrected_pct))
                        .unwrap_or_default()
                );
                // Export by frame index so every mode (and --fruc-reference)
                // writes the same intervals.
                let export = args.export_every > 0
                    && (*index as usize).is_multiple_of(args.export_every)
                    && args.export_range.contains(&(*index as usize));
                if export {
                    let dir = out_dir.join("intervals");
                    let a_path = dir.join(format!("i{index:05}_A.png"));
                    if !a_path.exists() {
                        save_png(&a_path, prev_frame, w, h);
                        save_png(&dir.join(format!("i{index:05}_B.png")), &frame, w, h);
                    }
                    save_png(
                        &dir.join(format!("i{index:05}_{}.png", mode.name())),
                        &final_,
                        w,
                        h,
                    );
                    if mode == Mode::Raw {
                        // The FRUC output itself, even when not shown.
                        save_png(&dir.join(format!("i{index:05}_rawfruc.png")), &raw, w, h);
                    } else {
                        save_png(
                            &dir.join(format!("i{index:05}_diff_{}.png", mode.name())),
                            &diff_image(&raw, &final_),
                            w,
                            h,
                        );
                    }
                    exported.push(*index);
                }
                if in_seq {
                    save_seq(&final_, &mut seq_index);
                    save_seq(&frame, &mut seq_index);
                }
                last_unique = Some((slot, *index, frame));
            }
        }
    }

    let fix = fg.fix_report();
    r.correction_gpu_ms = fix.fix_gpu;
    r.capture_gpu_ms = fix.prep_gpu;
    r.fruc_ms = (!fruc_ms.is_empty()).then(|| summarize(fruc_ms.into_iter()));
    r.publish_wall_ms = (!publish_ms.is_empty()).then(|| summarize(publish_ms.into_iter()));
    r.corrected_pct = (!corrected.is_empty()).then(|| summarize(corrected.into_iter()));
    r.changed_vs_raw_pct = (!changed.is_empty()).then(|| summarize(changed.into_iter()));
    backend.shutdown(Box::new(()), Some(Duration::from_secs(3)));
    drop(fg);
    result.map(|()| r)
}

fn load_png(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

/// Half-size 3x2 grid per exported interval:
/// A | raw | dynamic  /  B | advanced (or zones) | dynamic changed pixels.
fn build_comparisons(out_dir: &Path, exported: &[u32], modes: &[Mode]) {
    let dir = out_dir.join("intervals");
    let second = if modes.contains(&Mode::Advanced) {
        "advanced"
    } else {
        "zones"
    };
    for index in exported {
        let tiles = [
            format!("i{index:05}_A.png"),
            format!("i{index:05}_raw.png"),
            format!("i{index:05}_dynamic.png"),
            format!("i{index:05}_B.png"),
            format!("i{index:05}_{second}.png"),
            format!("i{index:05}_diff_dynamic.png"),
        ];
        let images: Vec<Option<(Vec<u8>, u32, u32)>> = tiles
            .iter()
            .map(|t| load_png(&dir.join(t)).map(|(d, w, h)| half(&d, w, h)))
            .collect();
        let Some((_, tw, th)) = images
            .iter()
            .flatten()
            .next()
            .map(|(d, w, h)| (d.len(), *w, *h))
        else {
            continue;
        };
        let (gw, gh) = (tw * 3, th * 2);
        let mut grid = vec![0u8; (gw * gh * 4) as usize];
        for (k, img) in images.iter().enumerate() {
            let Some((data, w, h)) = img else {
                continue;
            };
            if (*w, *h) != (tw, th) {
                continue;
            }
            let (ox, oy) = ((k as u32 % 3) * tw, (k as u32 / 3) * th);
            for y in 0..th {
                let s = (y * tw * 4) as usize;
                let d = (((oy + y) * gw + ox) * 4) as usize;
                grid[d..d + (tw * 4) as usize].copy_from_slice(&data[s..s + (tw * 4) as usize]);
            }
        }
        save_png(&dir.join(format!("i{index:05}_compare.png")), &grid, gw, gh);
    }
}

fn write_index_html(out_dir: &Path, exported: &[u32], modes: &[Mode], clip: &Clip) {
    let counts: Vec<(String, usize)> = modes
        .iter()
        .map(|m| {
            let n = std::fs::read_dir(out_dir.join("sequence").join(m.name()))
                .map(|d| d.count())
                .unwrap_or(0);
            (m.name().to_string(), n)
        })
        .collect();
    let modes_js = serde_json::to_string(&counts).unwrap_or_else(|_| "[]".into());
    let intervals: String = exported
        .iter()
        .map(|i| {
            format!(
                "<section><h3>Interval ending at frame {i}</h3>\
                 <p>Top: previous real frame A | raw FRUC | Dynamic. Bottom: next real frame B | \
                 Advanced (or zones) | pixels Dynamic changed (magenta).</p>\
                 <a href=\"intervals/i{i:05}_compare.png\"><img src=\"intervals/i{i:05}_compare.png\" \
                 loading=\"lazy\"></a></section>"
            )
        })
        .collect();
    let html = format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>TackleCast replay</title>
<style>body{{font-family:system-ui,sans-serif;background:#111;color:#ddd;margin:20px}}
img{{max-width:100%;image-rendering:auto}}.row{{display:flex;gap:8px;flex-wrap:wrap}}
.cell{{flex:1 1 30%}}.cell img{{width:100%}}button,select{{font-size:15px;margin:4px}}
section{{margin:24px 0}}code{{color:#9cf}}</style></head><body>
<h1>Replay of {clip}</h1>
<p>Same recorded frames through each mode. See <code>summary.txt</code> for numbers.</p>
<h2>Sequence at the 60 FPS output cadence (half size)</h2>
<div><button id="play">Play</button><button id="prev">&#9664; Step</button>
<button id="next">Step &#9654;</button> speed <select id="speed"><option value="1">1x (60 FPS)</option>
<option value="0.25">0.25x</option><option value="0.1">0.1x</option></select>
frame <span id="pos">0</span></div>
<div class="row" id="players"></div>
<h2>Selected intervals</h2>{intervals}
<script>
const modes={modes_js};let i=0,timer=null;const players=document.getElementById('players');
const imgs=modes.filter(m=>m[1]>0).map(m=>{{const c=document.createElement('div');c.className='cell';
c.innerHTML='<b>'+m[0]+'</b><br>';const im=document.createElement('img');c.appendChild(im);
players.appendChild(c);return [im,m[1],m[0]];}});
const max=Math.max(1,...imgs.map(x=>x[1]));
function show(){{for(const [im,n,name] of imgs){{im.src='sequence/'+name+'/'+String(Math.min(i,n-1)).padStart(6,'0')+'.png';}}
document.getElementById('pos').textContent=i;}}
function step(d){{i=(i+d+max)%max;show();}}
document.getElementById('next').onclick=()=>step(1);document.getElementById('prev').onclick=()=>step(-1);
document.getElementById('play').onclick=()=>{{if(timer){{clearInterval(timer);timer=null;return;}}
timer=setInterval(()=>step(1),1000/(60*parseFloat(document.getElementById('speed').value)));}};
show();
</script></body></html>"#,
        clip = clip.dir.display(),
    );
    let _ = std::fs::write(out_dir.join("index.html"), html);
}

fn fmt_ms(s: &Option<DurationSummary>) -> String {
    s.as_ref()
        .map(|s| format!("{:.2} / {:.2} / {:.2}", s.mean_ms, s.p95_ms, s.p99_ms))
        .unwrap_or_else(|| "n/a".into())
}

fn fmt_pct(s: &Option<DurationSummary>) -> String {
    s.as_ref()
        .map(|s| format!("{:.2} / {:.2} / {:.2}", s.mean_ms, s.p95_ms, s.max_ms))
        .unwrap_or_else(|| "n/a".into())
}

fn summary_text(r: &Report) -> String {
    let mut out = format!(
        "TackleCast frame-generation replay\nclip: {}\nengine: {}\n{}x{} on {} (GPU timestamps: {})\n\n\
         Source (recorded arrivals): {} frames over {:.1} s, interval mean {:.2} ms, \
         stddev {:.2}, p99 {:.2}, max {:.2}; {} intervals > 1.5x median; {} frames \
         not stored while recording\n\n",
        r.clip,
        r.engine,
        r.width,
        r.height,
        r.gpu,
        if r.timestamp_queries { "yes" } else { "no" },
        r.source.frames,
        r.source.duration_s,
        r.source.interval_ms.mean_ms,
        r.source.interval_ms.stddev_ms,
        r.source.interval_ms.p99_ms,
        r.source.interval_ms.max_ms,
        r.source.long_intervals,
        r.source.frames_dropped_while_recording,
    );
    for m in &r.modes {
        out += &format!("[{}]\n", serde_json::to_string(&m.mode).unwrap_or_default());
        if let Some(e) = &m.error {
            out += &format!("  ERROR: {e}\n\n");
            continue;
        }
        out += &format!(
            "  frames {}  unique {}  duplicates {}  generated {}  FRUC repeats {}  \
             FRUC failures {}  transition skips {}\n",
            m.frames,
            m.unique,
            m.duplicates,
            m.generated,
            m.fruc_repeats,
            m.fruc_failures,
            m.transition_skips
        );
        if m.mode != Mode::Native {
            out += &format!(
                "  FRUC ms (mean/p95/p99): {}\n  correction GPU ms: {}\n  capture-side GPU ms: {}\n  \
                 publish wall ms: {}\n  FRUC+publish over half a source frame: {} frames\n  \
                 corrected area % (mean/p95/max): {}   frames > 5%: {}\n  \
                 pixels changed vs raw FRUC % (mean/p95/max): {}\n",
                fmt_ms(&m.fruc_ms),
                fmt_ms(&m.correction_gpu_ms),
                fmt_ms(&m.capture_gpu_ms),
                fmt_ms(&m.publish_wall_ms),
                m.over_half_frame_budget,
                fmt_pct(&m.corrected_pct),
                m.frames_over_5pct,
                fmt_pct(&m.changed_vs_raw_pct),
            );
        }
        out += "\n";
    }
    for n in &r.notes {
        out += &format!("note: {n}\n");
    }
    out
}

/// `--fruc-reference=<clip>`: the recorded frames through FRUC exactly the
/// way NVIDIA's NvOFFRUCSample does it (CUDA cuDevicePtr resources from
/// cuMemAlloc, 1 interpolate + 2 alternating render buffers, pitch 0,
/// millisecond timestamps, bSkipWarp never set, every frame processed, no
/// duplicate check, no DX12, no wgpu, no corrections). If its output equals
/// the replay's raw FRUC output, the app's DX12/CUDA transfer path is not
/// the source of an artifact.
pub struct ReferenceArgs {
    pub clip: PathBuf,
    pub out_dir: Option<PathBuf>,
    pub runtime_dir: String,
    pub every: usize,
    /// Timestamp step per frame: 33.33 (ms, as the sample) or 1.0.
    pub timestamp_step: f64,
}

impl ReferenceArgs {
    pub fn from_cli(args: &[String]) -> Option<Self> {
        let value = |name: &str| {
            args.iter()
                .find_map(|a| a.strip_prefix(&format!("{name}=")).map(str::to_string))
        };
        Some(Self {
            clip: PathBuf::from(value("--fruc-reference")?),
            out_dir: value("--fruc-reference-out").map(PathBuf::from),
            runtime_dir: value("--fruc-runtime").unwrap_or_default(),
            every: value("--fg-replay-every")
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            timestamp_step: match value("--fruc-ts").as_deref() {
                Some("frames") => 1.0,
                _ => 1000.0 / 30.0,
            },
        })
    }
}

pub fn run_reference(args: ReferenceArgs) -> i32 {
    super::selftest::attach_parent_console();
    match reference(&args) {
        Ok(summary) => {
            println!("{summary}");
            0
        }
        Err(e) => {
            eprintln!("fruc reference: {e}");
            1
        }
    }
}

fn reference(args: &ReferenceArgs) -> Result<String, String> {
    use super::cuda::CudaDevice;
    use super::fruc::{FrucLibrary, FrucSession};
    let clip = Clip::open(&args.clip)?;
    let out_dir = args.out_dir.clone().unwrap_or_else(|| {
        args.clip.join(if args.timestamp_step > 1.0 {
            "reference_ms"
        } else {
            "reference_frames"
        })
    });
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    let (w, h) = (clip.meta.width, clip.meta.height);
    let size = (w * h * 4) as usize;
    let cuda = CudaDevice::open(None)?;
    let lib = FrucLibrary::load(&args.runtime_dir, true)?;
    let out = cuda.alloc(size)?;
    let render = [cuda.alloc(size)?, cuda.alloc(size)?];
    // Registration order as the sample: interpolate buffer, then renders.
    let resources = [out.ptr, render[0].ptr, render[1].ptr];
    let mut session = FrucSession::new(&lib, w, h, 0, &resources)?;
    let mut csv = std::io::BufWriter::new(
        std::fs::File::create(out_dir.join("frames.csv")).map_err(|e| e.to_string())?,
    );
    let _ = writeln!(csv, "index,fruc_ms,fruc_repeat");
    let mut output = vec![0u8; size];
    let mut times = Vec::new();
    let mut repeats = 0usize;
    let mut last_render = 0.0f64;
    let mut render_index = 0usize;
    for (k, (index, _)) in clip.frames.iter().enumerate() {
        let frame = clip.load_rgba(*index)?;
        render_index = (render_index + 1) % 2;
        cuda.upload(render[render_index].ptr, &frame)?;
        let render_time = last_render + args.timestamp_step;
        last_render = render_time;
        let interp_time = last_render - args.timestamp_step * 0.5;
        let started = Instant::now();
        let outcome = session.process(1 + render_index, 0, render_time, interp_time, false)?;
        cuda.synchronize()?;
        let ms = started.elapsed().as_secs_f32() * 1000.0;
        if k > 0 {
            times.push(ms);
        }
        if outcome.repeated {
            repeats += 1;
        }
        let _ = writeln!(csv, "{index},{ms:.3},{}", u8::from(outcome.repeated));
        if k > 0 && args.every > 0 && (*index as usize).is_multiple_of(args.every) {
            cuda.download(out.ptr, &mut output)?;
            save_png(&out_dir.join(format!("ref_i{index:05}.png")), &output, w, h);
        }
    }
    let t = summarize(times.into_iter());
    let summary = format!(
        "FRUC reference (NvOFFRUCSample flow) on {}\n{} frames, timestamp step {:.3}\n\
         FRUC ms mean {:.2} p95 {:.2} p99 {:.2} max {:.2}\nframe repetitions reported by FRUC: {}\n\
         outputs: {}\n",
        clip.dir.display(),
        clip.frames.len(),
        args.timestamp_step,
        t.mean_ms,
        t.p95_ms,
        t.p99_ms,
        t.max_ms,
        repeats,
        out_dir.display()
    );
    let _ = std::fs::write(out_dir.join("summary.txt"), &summary);
    Ok(summary)
}
