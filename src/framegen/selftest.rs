//! `--fruc-selftest`: reproducible validation of NVIDIA FRUC on this PC.
//!
//! Stage 1 (CUDA only) isolates the FRUC library: synthetic RGBA frames with
//! known motion are uploaded to `cuMemAlloc` buffers, interpolated, and the
//! result is compared with the analytically rendered ground-truth midpoint.
//! Two simple baselines show what "not motion-compensated" looks like:
//! repeating the previous frame, and a 50/50 blend (ghosting).
//!
//! Stage 2 (production path) runs the exact pipeline TackleCast uses at run
//! time: wgpu DX12 textures -> shared buffers -> CUDA import -> worker
//! thread -> FRUC -> shared output buffer -> wgpu readback. It also checks
//! duplicate detection and that the worker shuts down and frees everything.
//!
//! Results go to `fruc_selftest_report.json`, `fruc_selftest.txt` and PNG
//! crops in the output folder. Nothing here is a claim about live capture.

use super::cuda::CudaDevice;
use super::fruc::FrucLibrary;
use super::fruc::FrucSession;
use super::stats::{summarize, DurationSummary};
use super::{BackendEvent, GpuFence, Job, Outcome};
use crate::render::framegen_gpu::{FgGpu, FG_FORMAT};
use serde::Serialize;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct SelftestArgs {
    pub out_dir: PathBuf,
    pub runtime_dir: String,
    pub width: u32,
    pub height: u32,
}

impl SelftestArgs {
    pub fn from_cli(args: &[String]) -> Option<Self> {
        let flag = args
            .iter()
            .find(|a| a.as_str() == "--fruc-selftest" || a.starts_with("--fruc-selftest="))?;
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        let out_dir = flag
            .split_once('=')
            .map(|(_, v)| PathBuf::from(v))
            .unwrap_or_else(|| exe_dir.join("fruc-selftest"));
        let value = |name: &str| {
            args.iter()
                .find_map(|a| a.strip_prefix(name).map(str::to_string))
        };
        let runtime_dir = value("--fruc-runtime=").unwrap_or_default();
        let (width, height) = value("--fruc-size=")
            .and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or((2560, 1440));
        Some(Self {
            out_dir,
            runtime_dir,
            width,
            height,
        })
    }
}

// ---------------------------------------------------------------------------
// Synthetic scenes with exact ground truth
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Serialize)]
enum Scene {
    /// Static textured background, two textured objects moving at
    /// (+24, 0) and (-16, +12) px/frame.
    MovingObjects,
    /// The whole picture pans by +12 px/frame (camera pan).
    CameraPan,
}

fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}

fn texture(x: i64, y: i64, seed: u32) -> [u8; 3] {
    // 12 px cells with hashed brightness plus fine detail: plenty of
    // trackable structure for optical flow, like a game texture.
    let cell = hash(
        (x.div_euclid(12) as u32).wrapping_mul(73_856_093)
            ^ (y.div_euclid(12) as u32).wrapping_mul(19_349_663)
            ^ seed,
    );
    let base = 40 + (cell % 150) as i64;
    let fine = ((x * 7 + y * 3).rem_euclid(17) - 8) * 3;
    let v = (base + fine).clamp(0, 255) as u8;
    let tint = (cell >> 8) % 3;
    match tint {
        0 => [v, v.saturating_sub(20), v.saturating_sub(40)],
        1 => [v.saturating_sub(30), v, v.saturating_sub(10)],
        _ => [v.saturating_sub(10), v.saturating_sub(25), v],
    }
}

struct SceneObject {
    x0: i64,
    y0: i64,
    vx: f64,
    vy: f64,
    w: i64,
    h: i64,
    seed: u32,
}

fn objects(width: u32, height: u32) -> [SceneObject; 2] {
    let (w, h) = (width as i64, height as i64);
    [
        SceneObject {
            x0: w / 5,
            y0: h / 3,
            vx: 24.0,
            vy: 0.0,
            w: 192,
            h: 192,
            seed: 0xA5A5,
        },
        SceneObject {
            x0: w * 3 / 5,
            y0: h / 4,
            vx: -16.0,
            vy: 12.0,
            w: 160,
            h: 120,
            seed: 0x5A5A,
        },
    ]
}

/// Render `scene` at time `t` (frames). All half-frame offsets are whole
/// pixels, so ground truth at t + 0.5 is exact.
fn render(scene: Scene, t: f64, width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0_u8; (width * height * 4) as usize];
    let pan = match scene {
        Scene::CameraPan => (12.0 * t).round() as i64,
        Scene::MovingObjects => 0,
    };
    let objs = objects(width, height);
    for y in 0..height as i64 {
        for x in 0..width as i64 {
            let mut rgb = texture(x - pan, y, 0x1234);
            if let Scene::MovingObjects = scene {
                for o in &objs {
                    let ox = o.x0 + (o.vx * t).round() as i64;
                    let oy = o.y0 + (o.vy * t).round() as i64;
                    if x >= ox && x < ox + o.w && y >= oy && y < oy + o.h {
                        let c = texture(x - ox, y - oy, o.seed);
                        rgb = [c[0].saturating_add(40), c[1].saturating_add(40), c[2]];
                    }
                }
            }
            let i = ((y * width as i64 + x) * 4) as usize;
            out[i..i + 3].copy_from_slice(&rgb);
            out[i + 3] = 255;
        }
    }
    out
}

/// Region where motion happens between frames `t0` and `t1`.
fn motion_region(scene: Scene, t0: f64, t1: f64, width: u32, height: u32) -> (u32, u32, u32, u32) {
    match scene {
        Scene::CameraPan => (0, 0, width, height),
        Scene::MovingObjects => {
            let mut x_min = i64::MAX;
            let mut y_min = i64::MAX;
            let mut x_max = i64::MIN;
            let mut y_max = i64::MIN;
            for o in objects(width, height) {
                for t in [t0, t1] {
                    let ox = o.x0 + (o.vx * t).round() as i64;
                    let oy = o.y0 + (o.vy * t).round() as i64;
                    x_min = x_min.min(ox);
                    y_min = y_min.min(oy);
                    x_max = x_max.max(ox + o.w);
                    y_max = y_max.max(oy + o.h);
                }
            }
            let x0 = x_min.clamp(0, width as i64) as u32;
            let y0 = y_min.clamp(0, height as i64) as u32;
            let x1 = x_max.clamp(0, width as i64) as u32;
            let y1 = y_max.clamp(0, height as i64) as u32;
            (x0, y0, x1 - x0, y1 - y0)
        }
    }
}

fn psnr(a: &[u8], b: &[u8], width: u32, region: (u32, u32, u32, u32)) -> f64 {
    let (rx, ry, rw, rh) = region;
    let mut sum = 0.0_f64;
    let mut n = 0_u64;
    for y in ry..ry + rh {
        for x in rx..rx + rw {
            let i = ((y * width + x) * 4) as usize;
            for c in 0..3 {
                let d = a[i + c] as f64 - b[i + c] as f64;
                sum += d * d;
                n += 1;
            }
        }
    }
    let mse = sum / n.max(1) as f64;
    if mse <= 1e-9 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

fn blend(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter()
        .zip(b)
        .map(|(x, y)| (*x as u16 + *y as u16).div_ceil(2) as u8)
        .collect()
}

fn save_crop(path: &Path, data: &[u8], width: u32, region: (u32, u32, u32, u32)) {
    let (rx, ry, rw, rh) = region;
    let mut crop = Vec::with_capacity((rw * rh * 4) as usize);
    for y in ry..ry + rh {
        let start = ((y * width + rx) * 4) as usize;
        crop.extend_from_slice(&data[start..start + (rw * 4) as usize]);
    }
    if let Err(e) = image::save_buffer(path, &crop, rw, rh, image::ColorType::Rgba8) {
        tracing::warn!("could not save {}: {e}", path.display());
    }
}

/// Crop around the motion, at most 1280x720, for the PNGs.
fn crop_region(motion: (u32, u32, u32, u32), width: u32, height: u32) -> (u32, u32, u32, u32) {
    let (mx, my, mw, mh) = motion;
    let cw = 1280.min(width);
    let ch = 720.min(height);
    let cx = (mx + mw / 2).saturating_sub(cw / 2).min(width - cw);
    let cy = (my + mh / 2).saturating_sub(ch / 2).min(height - ch);
    let _ = (mw, mh);
    (cx, cy, cw, ch)
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[derive(Serialize, Default)]
struct QualityResult {
    scene: String,
    step: usize,
    psnr_fruc_motion_db: f64,
    psnr_repeat_motion_db: f64,
    psnr_blend_motion_db: f64,
    psnr_fruc_full_db: f64,
    repeated_flag: bool,
    motion_compensated: bool,
}

#[derive(Serialize, Default)]
struct Stage1 {
    passed: bool,
    width: u32,
    height: u32,
    quality: Vec<QualityResult>,
    process_time: DurationSummary,
    process_time_1080p: Option<DurationSummary>,
    error: Option<String>,
}

#[derive(Serialize, Default)]
struct Stage2 {
    passed: bool,
    backend: String,
    warmup_ms: f64,
    jobs: usize,
    interpolated: usize,
    duplicates_detected: usize,
    duplicate_expected: usize,
    unique_misclassified_as_duplicate: usize,
    failures: usize,
    psnr_fruc_motion_db: Option<f64>,
    psnr_blend_motion_db: Option<f64>,
    job_latency: DurationSummary,
    fruc_time: DurationSummary,
    gpu_wait: DurationSummary,
    shutdown_clean: bool,
    error: Option<String>,
}

#[derive(Serialize, Default)]
struct Report {
    tool: String,
    timestamp_unix: u64,
    gpu: String,
    compute_capability: String,
    cuda_driver: String,
    runtime_dir: String,
    signer: String,
    stage1_cuda_only: Stage1,
    stage2_production_path: Stage2,
    passed: bool,
    notes: Vec<String>,
}

// ---------------------------------------------------------------------------

pub fn run(args: SelftestArgs) -> i32 {
    attach_parent_console();
    let _ = std::fs::create_dir_all(&args.out_dir);
    let mut report = Report {
        tool: format!(
            "TackleCast-FrameGen {} --fruc-selftest",
            env!("CARGO_PKG_VERSION")
        ),
        timestamp_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        ..Default::default()
    };
    report.notes.push(
        "Synthetic frames only. This validates FRUC and the GPU pipeline on this PC; it is not a \
         live capture-card or PS5 test."
            .into(),
    );

    match stage1(&args, &mut report) {
        Ok(stage) => report.stage1_cuda_only = stage,
        Err(e) => {
            report.stage1_cuda_only.error = Some(e);
        }
    }
    if report.stage1_cuda_only.error.is_none() {
        match stage2(&args) {
            Ok(stage) => report.stage2_production_path = stage,
            Err(e) => report.stage2_production_path.error = Some(e),
        }
    } else {
        report.stage2_production_path.error = Some("skipped: stage 1 failed".into());
    }
    report.passed = report.stage1_cuda_only.passed && report.stage2_production_path.passed;

    let json = serde_json::to_string_pretty(&report).unwrap_or_default();
    let _ = std::fs::write(args.out_dir.join("fruc_selftest_report.json"), &json);
    let text = summary_text(&report);
    let _ = std::fs::write(args.out_dir.join("fruc_selftest.txt"), &text);
    println!("{text}");
    tracing::info!("FRUC self-test finished: passed={}", report.passed);
    if report.passed {
        0
    } else {
        1
    }
}

fn stage1(args: &SelftestArgs, report: &mut Report) -> Result<Stage1, String> {
    let cuda = CudaDevice::open(None)?;
    report.gpu = cuda.info.name.clone();
    report.compute_capability = cuda.info.compute_label();
    report.cuda_driver = cuda.info.driver_version_label();
    let lib = FrucLibrary::load(&args.runtime_dir, true)?;
    report.runtime_dir = lib.dir.display().to_string();
    report.signer = lib.signer.clone();
    if cuda.info.compute_major >= 9 {
        report.notes.push(format!(
            "{} is newer than the GPU code inside NvOFFRUC.dll (sm_75/sm_80); it runs through \
             driver JIT compilation of the bundled PTX 7.2.",
            report.compute_capability
        ));
    }

    let (w, h) = (args.width, args.height);
    let size = (w * h * 4) as usize;
    let mut stage = Stage1 {
        width: w,
        height: h,
        ..Default::default()
    };
    let out = cuda.alloc(size)?;
    let src = [cuda.alloc(size)?, cuda.alloc(size)?];
    let resources = [out.ptr, src[0].ptr, src[1].ptr];
    let mut session = FrucSession::new(&lib, w, h, (w * 4) as usize, &resources)?;
    let mut times = Vec::new();
    let mut output = vec![0_u8; size];
    let mut ts = 0.0_f64;

    for scene in [Scene::MovingObjects, Scene::CameraPan] {
        let steps = 24;
        for k in 0..=steps {
            let frame = render(scene, k as f64, w, h);
            let slot = k % 2;
            cuda.upload(src[slot].ptr, &frame)?;
            let started = Instant::now();
            let first = k == 0;
            let result = session.process(1 + slot, 0, ts + 1.0, ts + 0.5, first)?;
            cuda.synchronize()?;
            if !first {
                times.push(started.elapsed().as_secs_f32() * 1000.0);
            }
            ts += 1.0;
            if !first && (k == 8 || k == 16) {
                cuda.download(out.ptr, &mut output)?;
                let prev = render(scene, (k - 1) as f64, w, h);
                let truth = render(scene, k as f64 - 0.5, w, h);
                let region = motion_region(scene, (k - 1) as f64, k as f64, w, h);
                let blended = blend(&prev, &frame);
                let q = QualityResult {
                    scene: format!("{:?}", scene_name(scene)),
                    step: k,
                    psnr_fruc_motion_db: psnr(&output, &truth, w, region),
                    psnr_repeat_motion_db: psnr(&prev, &truth, w, region),
                    psnr_blend_motion_db: psnr(&blended, &truth, w, region),
                    psnr_fruc_full_db: psnr(&output, &truth, w, (0, 0, w, h)),
                    repeated_flag: result.repeated,
                    motion_compensated: false,
                };
                let q = QualityResult {
                    motion_compensated: q.psnr_fruc_motion_db > q.psnr_blend_motion_db + 1.0
                        && q.psnr_fruc_motion_db > q.psnr_repeat_motion_db + 1.0,
                    ..q
                };
                if k == 16 {
                    let crop = crop_region(region, w, h);
                    let name = scene_name(scene);
                    let dir = &args.out_dir;
                    save_crop(&dir.join(format!("{name}_1_previous.png")), &prev, w, crop);
                    save_crop(
                        &dir.join(format!("{name}_2_fruc_interpolated.png")),
                        &output,
                        w,
                        crop,
                    );
                    save_crop(
                        &dir.join(format!("{name}_3_ground_truth.png")),
                        &truth,
                        w,
                        crop,
                    );
                    save_crop(&dir.join(format!("{name}_4_next.png")), &frame, w, crop);
                    save_crop(
                        &dir.join(format!("{name}_5_blend_baseline.png")),
                        &blended,
                        w,
                        crop,
                    );
                }
                stage.quality.push(q);
            }
        }
        // Reset continuity between scenes.
        ts += 10.0;
    }
    stage.process_time = summarize(times.into_iter());
    drop(session);

    // Timing at 1080p as well (capture cards are often run at 1080p).
    if (w, h) != (1920, 1080) {
        let (w2, h2) = (1920_u32, 1080_u32);
        let size2 = (w2 * h2 * 4) as usize;
        let out2 = cuda.alloc(size2)?;
        let s2 = [cuda.alloc(size2)?, cuda.alloc(size2)?];
        let mut session2 = FrucSession::new(
            &lib,
            w2,
            h2,
            (w2 * 4) as usize,
            &[out2.ptr, s2[0].ptr, s2[1].ptr],
        )?;
        let mut t2 = Vec::new();
        for k in 0..40 {
            let frame = render(Scene::MovingObjects, k as f64, w2, h2);
            cuda.upload(s2[k % 2].ptr, &frame)?;
            let started = Instant::now();
            session2.process(1 + k % 2, 0, k as f64 + 1.0, k as f64 + 0.5, k == 0)?;
            cuda.synchronize()?;
            if k > 2 {
                t2.push(started.elapsed().as_secs_f32() * 1000.0);
            }
        }
        stage.process_time_1080p = Some(summarize(t2.into_iter()));
    }

    stage.passed = !stage.quality.is_empty() && stage.quality.iter().all(|q| q.motion_compensated);
    Ok(stage)
}

fn scene_name(scene: Scene) -> &'static str {
    match scene {
        Scene::MovingObjects => "moving_objects",
        Scene::CameraPan => "camera_pan",
    }
}

fn stage2(args: &SelftestArgs) -> Result<Stage2, String> {
    let (w, h) = (args.width, args.height);
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
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("fruc-selftest"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        ..Default::default()
    }))
    .map_err(|e| format!("request_device: {e}"))?;

    let (mut gpu, import) = FgGpu::new(
        &device,
        FG_FORMAT,
        w,
        h,
        super::SOURCE_SLOTS,
        super::OUTPUT_SLOTS,
    )?;
    let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<()>();
    let mut backend = super::worker::spawn(
        import,
        device.clone(),
        super::worker::WorkerConfig {
            runtime_dir: args.runtime_dir.clone(),
            require_signature: true,
            timestamp_scale: super::worker::default_timestamp_scale(),
        },
        Box::new(move || {
            let _ = wake_tx.send(());
        }),
    );
    let mut stage = Stage2::default();

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
        Some(BackendEvent::Ready(info)) => {
            stage.backend = info.description;
            stage.warmup_ms = info.warmup.as_secs_f64() * 1000.0;
        }
        Some(BackendEvent::InitFailed(e)) | Some(BackendEvent::Fatal(e)) => {
            backend.shutdown(Box::new(gpu), Some(Duration::from_secs(3)));
            return Err(e);
        }
        _ => {
            backend.shutdown(Box::new(gpu), Some(Duration::from_secs(3)));
            return Err("worker did not become ready within 120 s".into());
        }
    }

    // 20 frames of moving objects; frame 10 is sent twice (a duplicate).
    let mut latencies = Vec::new();
    let mut fruc_times = Vec::new();
    let mut gpu_waits = Vec::new();
    let mut last_slot: Option<usize> = None;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("fruc-selftest-readback"),
        size: gpu.pitch as u64 * h as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut schedule: Vec<(f64, bool)> = (0..20).map(|k| (k as f64, false)).collect();
    schedule.insert(11, (10.0, true));
    stage.duplicate_expected = 1;

    for (index, (t, expect_duplicate)) in schedule.iter().enumerate() {
        let slot = index % super::SOURCE_SLOTS;
        let output = index % super::OUTPUT_SLOTS;
        let frame = render(Scene::MovingObjects, *t, w, h);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &gpu.sources[slot].texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame,
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
        let mut encoder = device.create_command_encoder(&Default::default());
        gpu.encode_transfer(
            &device,
            &queue,
            &mut encoder,
            slot,
            last_slot,
            crate::render::FgProtection::default(),
        );
        let submission = queue.submit(std::iter::once(encoder.finish()));
        let submitted = Instant::now();
        backend.submit(Job {
            seq: index as u64 + 1,
            source: slot,
            output,
            fence: GpuFence(Some(submission)),
            has_reference: last_slot.is_some(),
            fruc: true,
            skip_warp: index == 0,
            thresholds: Default::default(),
        })?;
        last_slot = Some(slot);
        stage.jobs += 1;
        let result = match wait_event(&mut backend, Duration::from_secs(10)) {
            Some(BackendEvent::Done(r)) => r,
            Some(BackendEvent::Fatal(e)) => return Err(e),
            other => return Err(format!("unexpected worker event: {other:?}")),
        };
        latencies.push(submitted.elapsed().as_secs_f32() * 1000.0);
        gpu_waits.push(result.gpu_wait.as_secs_f32() * 1000.0);
        match &result.outcome {
            Outcome::Duplicate(_) => {
                if *expect_duplicate {
                    stage.duplicates_detected += 1;
                } else {
                    stage.unique_misclassified_as_duplicate += 1;
                }
            }
            Outcome::Interpolated { .. } => {
                stage.interpolated += 1;
                fruc_times.push(result.fruc_time.as_secs_f32() * 1000.0);
                // Check one interpolated frame end to end (t = 5.5).
                if (*t - 6.0).abs() < 1e-9 {
                    let mut encoder = device.create_command_encoder(&Default::default());
                    encoder.copy_buffer_to_buffer(
                        &gpu.outputs[output].buffer,
                        0,
                        &readback,
                        0,
                        readback.size(),
                    );
                    let sub = queue.submit(std::iter::once(encoder.finish()));
                    let _ = device.poll(wgpu::PollType::Wait {
                        submission_index: Some(sub),
                        timeout: None,
                    });
                    let slice = readback.slice(..);
                    let (tx, rx) = crossbeam_channel::bounded(1);
                    slice.map_async(wgpu::MapMode::Read, move |r| {
                        let _ = tx.send(r);
                    });
                    let _ = device.poll(wgpu::PollType::wait_indefinitely());
                    if let Ok(Ok(())) = rx.recv_timeout(Duration::from_secs(5)) {
                        let data = slice.get_mapped_range();
                        let mut packed = vec![0_u8; (w * h * 4) as usize];
                        for row in 0..h as usize {
                            let src = row * gpu.pitch as usize;
                            let dst = row * (w * 4) as usize;
                            packed[dst..dst + (w * 4) as usize]
                                .copy_from_slice(&data[src..src + (w * 4) as usize]);
                        }
                        drop(data);
                        readback.unmap();
                        let prev = render(Scene::MovingObjects, 5.0, w, h);
                        let next = render(Scene::MovingObjects, 6.0, w, h);
                        let truth = render(Scene::MovingObjects, 5.5, w, h);
                        let region = motion_region(Scene::MovingObjects, 5.0, 6.0, w, h);
                        stage.psnr_fruc_motion_db = Some(psnr(&packed, &truth, w, region));
                        stage.psnr_blend_motion_db =
                            Some(psnr(&blend(&prev, &next), &truth, w, region));
                        save_crop(
                            &args.out_dir.join("production_path_interpolated.png"),
                            &packed,
                            w,
                            crop_region(region, w, h),
                        );
                    }
                }
            }
            Outcome::Unique { .. } => {}
            Outcome::Failed(e) => {
                stage.failures += 1;
                stage.error.get_or_insert_with(|| e.clone());
            }
        }
    }
    stage.job_latency = summarize(latencies.into_iter());
    stage.fruc_time = summarize(fruc_times.into_iter());
    stage.gpu_wait = summarize(gpu_waits.into_iter());

    // Clean shutdown: the worker must exit and release CUDA within 3 s.
    let started = Instant::now();
    backend.shutdown(Box::new(()), Some(Duration::from_secs(3)));
    stage.shutdown_clean = started.elapsed() < Duration::from_secs(3);
    drop(gpu);

    stage.passed = stage.failures == 0
        && stage.duplicates_detected == stage.duplicate_expected
        && stage.unique_misclassified_as_duplicate == 0
        && stage.interpolated >= 15
        && stage
            .psnr_fruc_motion_db
            .zip(stage.psnr_blend_motion_db)
            .is_some_and(|(f, b)| f > b + 1.0)
        && stage.shutdown_clean;
    Ok(stage)
}

fn summary_text(r: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "TackleCast-FrameGen FRUC self-test: {}",
        if r.passed { "PASSED" } else { "FAILED" }
    );
    let _ = writeln!(
        s,
        "GPU: {} ({}), {}",
        r.gpu, r.compute_capability, r.cuda_driver
    );
    let _ = writeln!(
        s,
        "FRUC runtime: {} (signed by {})",
        r.runtime_dir, r.signer
    );
    let s1 = &r.stage1_cuda_only;
    let _ = writeln!(
        s,
        "\n[Stage 1: FRUC on CUDA buffers, {}x{}] {}",
        s1.width,
        s1.height,
        if s1.passed { "PASS" } else { "FAIL" }
    );
    if let Some(e) = &s1.error {
        let _ = writeln!(s, "  error: {e}");
    }
    for q in &s1.quality {
        let _ = writeln!(
            s,
            "  {} step {}: PSNR in motion area FRUC {:.2} dB | repeat {:.2} dB | blend {:.2} dB | full-frame FRUC {:.2} dB{}  => {}",
            q.scene,
            q.step,
            q.psnr_fruc_motion_db,
            q.psnr_repeat_motion_db,
            q.psnr_blend_motion_db,
            q.psnr_fruc_full_db,
            if q.repeated_flag { " (FRUC reported frame repetition)" } else { "" },
            if q.motion_compensated { "motion-compensated" } else { "NOT better than baselines" }
        );
    }
    let t = &s1.process_time;
    let _ = writeln!(
        s,
        "  NvOFFRUCProcess time: mean {:.2} ms, p95 {:.2} ms, max {:.2} ms ({} calls)",
        t.mean_ms, t.p95_ms, t.max_ms, t.count
    );
    if let Some(t) = &s1.process_time_1080p {
        let _ = writeln!(
            s,
            "  1920x1080: mean {:.2} ms, p95 {:.2} ms",
            t.mean_ms, t.p95_ms
        );
    }
    let s2 = &r.stage2_production_path;
    let _ = writeln!(
        s,
        "\n[Stage 2: production path DX12 -> CUDA -> FRUC -> DX12] {}",
        if s2.passed { "PASS" } else { "FAIL" }
    );
    if let Some(e) = &s2.error {
        let _ = writeln!(s, "  error: {e}");
    }
    let _ = writeln!(s, "  backend: {}", s2.backend);
    let _ = writeln!(
        s,
        "  warm-up {:.0} ms; jobs {}, interpolated {}, failures {}",
        s2.warmup_ms, s2.jobs, s2.interpolated, s2.failures
    );
    let _ = writeln!(
        s,
        "  duplicate detection: {}/{} injected duplicates found, {} unique frames misclassified",
        s2.duplicates_detected, s2.duplicate_expected, s2.unique_misclassified_as_duplicate
    );
    if let (Some(f), Some(b)) = (s2.psnr_fruc_motion_db, s2.psnr_blend_motion_db) {
        let _ = writeln!(
            s,
            "  end-to-end interpolated frame: FRUC {f:.2} dB vs blend {b:.2} dB (motion area)"
        );
    }
    let _ = writeln!(
        s,
        "  per-frame latency (submit -> result): mean {:.2} ms, p95 {:.2} ms",
        s2.job_latency.mean_ms, s2.job_latency.p95_ms
    );
    let _ = writeln!(
        s,
        "  FRUC time in worker: mean {:.2} ms, p95 {:.2} ms; GPU wait mean {:.2} ms",
        s2.fruc_time.mean_ms, s2.fruc_time.p95_ms, s2.gpu_wait.mean_ms
    );
    let _ = writeln!(
        s,
        "  worker shutdown and resource release: {}",
        if s2.shutdown_clean {
            "clean"
        } else {
            "TIMED OUT"
        }
    );
    for note in &r.notes {
        let _ = writeln!(s, "\nNote: {note}");
    }
    s
}

pub(crate) fn attach_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: harmless if there is no parent console.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
