//! NVIDIA FRUC worker thread (Windows).
//!
//! Owns every CUDA and FRUC object; nothing CUDA-related ever runs on the UI
//! thread or the capture thread. Per job it:
//!
//! 1. waits for the wgpu submission that produced the source slot,
//! 2. reads the 16-byte duplicate statistic and skips duplicates,
//! 3. runs NvOFFRUCProcess (previous unique frame + this frame -> midpoint),
//! 4. synchronises CUDA so the output buffer is complete before reporting.
//!
//! Resource registration order follows the SDK sample: outputs, then sources.

use super::cuda::{CudaDevice, ImportedBuffer};
use super::duplicate::{is_duplicate, DiffStats};
use super::fruc::{FrucLibrary, FrucSession};
use super::{Backend, BackendEvent, BackendInfo, Job, JobResult, Outcome};
use crate::render::FgImport;
use crossbeam_channel::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

pub struct WorkerConfig {
    pub runtime_dir: String,
    pub require_signature: bool,
    /// FRUC timestamp of frame `seq` is `seq * timestamp_scale`. NVIDIA's
    /// sample uses milliseconds (33.33 per 30 FPS frame); 1.0 = frame units.
    pub timestamp_scale: f64,
}

/// Timestamp scale used by the app: frame units unless
/// `TACKLECAST_FRUC_TS=ms` (sample-style milliseconds) is set.
pub fn default_timestamp_scale() -> f64 {
    match std::env::var("TACKLECAST_FRUC_TS").as_deref() {
        Ok("ms") => 1000.0 / 30.0,
        _ => 1.0,
    }
}

pub(super) enum Command {
    Process(Job),
    Shutdown,
}

pub(super) struct WorkerBackend {
    tx: Sender<Command>,
    rx: Receiver<BackendEvent>,
    thread: Option<JoinHandle<()>>,
}

pub fn spawn(
    import: FgImport,
    device: wgpu::Device,
    config: WorkerConfig,
    waker: Box<dyn Fn() + Send>,
) -> Box<dyn Backend> {
    spawn_thread(
        "tacklecast-framegen",
        move |commands, send| run(import, device, config, commands, send),
        waker,
    )
}

/// Run a backend's job loop on its own thread. `run` gets the command
/// channel and an event sender; a panic or error inside it becomes a fatal
/// event instead of taking the app down.
pub(super) fn spawn_thread<F>(name: &str, run: F, waker: Box<dyn Fn() + Send>) -> Box<dyn Backend>
where
    F: FnOnce(&Receiver<Command>, &dyn Fn(BackendEvent)) -> Result<(), String> + Send + 'static,
{
    let (tx, cmd_rx) = crossbeam_channel::unbounded::<Command>();
    let (event_tx, rx) = crossbeam_channel::unbounded::<BackendEvent>();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let send = |event: BackendEvent| {
                let _ = event_tx.send(event);
                waker();
            };
            // A panic in the driver/FRUC path must not take the app down.
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&cmd_rx, &send)));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => send(BackendEvent::Fatal(e)),
                Err(_) => send(BackendEvent::Fatal("worker thread panicked".into())),
            }
        })
        .expect("failed to spawn frame generation worker");
    Box::new(WorkerBackend {
        tx,
        rx,
        thread: Some(thread),
    })
}

impl Backend for WorkerBackend {
    fn submit(&mut self, job: Job) -> Result<(), String> {
        self.tx
            .send(Command::Process(job))
            .map_err(|_| "worker channel closed".to_string())
    }

    fn poll(&mut self) -> Option<BackendEvent> {
        self.rx.try_recv().ok()
    }

    fn shutdown(mut self: Box<Self>, keep_alive: Box<dyn Send>, wait: Option<Duration>) {
        let _ = self.tx.send(Command::Shutdown);
        let Some(thread) = self.thread.take() else {
            return;
        };
        match wait {
            Some(limit) => {
                let deadline = Instant::now() + limit;
                while !thread.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                if thread.is_finished() {
                    let _ = thread.join();
                    drop(keep_alive);
                } else {
                    warn!("frame generation worker did not stop in time; leaving it to exit");
                    std::mem::forget(keep_alive);
                }
            }
            None => {
                // Join off the UI thread; free the shared buffers only after
                // CUDA has released its imports.
                let _ = std::thread::Builder::new()
                    .name("tacklecast-framegen-reaper".into())
                    .spawn(move || {
                        let _ = thread.join();
                        drop(keep_alive);
                        info!("frame generation resources released");
                    });
            }
        }
    }
}

pub(super) fn wait_for_gpu(device: &wgpu::Device, fence: Option<wgpu::SubmissionIndex>) {
    match fence {
        Some(index) => {
            let _ = device.poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: None,
            });
        }
        None => {
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
        }
    }
}

/// Map and read the duplicate statistic and the luma profiles (frame and
/// reference) for a finished submission; returns the stats and the camera
/// motion estimated from the profiles.
pub(super) fn read_stats(
    device: &wgpu::Device,
    buffer: &wgpu::Buffer,
    width: usize,
    height: usize,
) -> Result<(DiffStats, Option<super::motion::MotionEstimate>), String> {
    let slice = buffer.slice(..);
    let (tx, rx) = crossbeam_channel::bounded(1);
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    let deadline = Instant::now() + Duration::from_millis(500);
    let result = loop {
        let _ = device.poll(wgpu::PollType::Poll);
        if let Ok(result) = rx.try_recv() {
            break result;
        }
        if Instant::now() > deadline {
            return Err("timed out mapping duplicate statistics".into());
        }
        std::thread::sleep(Duration::from_micros(100));
    };
    result.map_err(|e| format!("map duplicate statistics: {e}"))?;
    let (stats, motion) = {
        let data = slice.get_mapped_range();
        let head = std::mem::size_of::<DiffStats>();
        let stats = bytemuck::pod_read_unaligned::<DiffStats>(&data[..head]);
        let floats = 2 * (width + height);
        let motion = (data.len() >= head + floats * 4).then(|| {
            let profiles: Vec<f32> = data[head..head + floats * 4]
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            let (cur, prev) = profiles.split_at(width + height);
            let (cur_x, cur_y) = cur.split_at(width);
            let (prev_x, prev_y) = prev.split_at(width);
            super::motion::estimate_motion(prev_x, cur_x, prev_y, cur_y)
        });
        (stats, motion.flatten())
    };
    buffer.unmap();
    Ok((stats, motion))
}

fn run(
    import: FgImport,
    device: wgpu::Device,
    config: WorkerConfig,
    commands: &Receiver<Command>,
    send: &dyn Fn(BackendEvent),
) -> Result<(), String> {
    let init_started = Instant::now();
    // --- CUDA device matching the renderer's adapter -----------------------
    let cuda = match CudaDevice::open(import.adapter_luid) {
        Ok(cuda) => cuda,
        Err(e) => {
            send(BackendEvent::InitFailed(e));
            return Ok(());
        }
    };
    let info = cuda.info.clone();
    info!(
        "framegen CUDA device: {} ({}), {}, {}",
        info.name,
        info.compute_label(),
        info.driver_version_label(),
        info.identity_label()
    );
    if info.compute_major < 7 || (info.compute_major == 7 && info.compute_minor < 5) {
        send(BackendEvent::InitFailed(format!(
            "{} ({}) has no NVIDIA Optical Flow hardware; FRUC needs Turing (sm_75) or newer",
            info.name,
            info.compute_label()
        )));
        return Ok(());
    }

    // --- Import the shared DX12 buffers ------------------------------------
    let mut outputs: Vec<ImportedBuffer> = Vec::new();
    let mut sources: Vec<ImportedBuffer> = Vec::new();
    for handle in &import.output_handles {
        match cuda.import_d3d12_buffer(handle.handle, handle.size) {
            Ok(b) => outputs.push(b),
            Err(e) => {
                send(BackendEvent::InitFailed(format!(
                    "CUDA import of output buffer: {e}"
                )));
                return Ok(());
            }
        }
    }
    for handle in &import.source_handles {
        match cuda.import_d3d12_buffer(handle.handle, handle.size) {
            Ok(b) => sources.push(b),
            Err(e) => {
                send(BackendEvent::InitFailed(format!(
                    "CUDA import of source buffer: {e}"
                )));
                return Ok(());
            }
        }
    }
    // CUDA holds its own references now.
    let FgImport {
        width,
        height,
        pitch,
        buffer_size,
        source_handles,
        output_handles,
        readbacks,
        ..
    } = import;
    drop(source_handles);
    drop(output_handles);

    // --- FRUC library and session ------------------------------------------
    let config_ts_scale = config.timestamp_scale.max(1e-6);
    let lib = match FrucLibrary::load(&config.runtime_dir, config.require_signature) {
        Ok(lib) => lib,
        Err(e) => {
            send(BackendEvent::InitFailed(e));
            return Ok(());
        }
    };
    let mut resources: Vec<u64> = outputs.iter().map(|b| b.ptr).collect();
    resources.extend(sources.iter().map(|b| b.ptr));
    let output_index = |slot: usize| slot;
    let source_index = |slot: usize| outputs.len() + slot;

    let mut session = match FrucSession::new(&lib, width, height, pitch as usize, &resources) {
        Ok(session) => session,
        Err(e) => {
            send(BackendEvent::InitFailed(format!(
                "{e}. GPU: {} ({}), {}",
                info.name,
                info.compute_label(),
                info.driver_version_label()
            )));
            return Ok(());
        }
    };

    // --- Warm-up -----------------------------------------------------------
    // The DLL ships sm_75/sm_80 code plus PTX; on newer GPUs (e.g. Blackwell
    // sm_120) the driver JIT-compiles the PTX on first use. Do that now,
    // with known content, so the first real frames are not delayed.
    let warm_started = Instant::now();
    let warmup = (|| -> Result<(), String> {
        cuda.fill(sources[0].ptr, 32, buffer_size as usize)?;
        cuda.fill(sources[1].ptr, 96, buffer_size as usize)?;
        session.process(source_index(0), output_index(0), 0.0, -0.5, false)?;
        session.process(source_index(1), output_index(0), 1.0, 0.5, false)?;
        cuda.synchronize()
    })();
    if let Err(e) = warmup {
        send(BackendEvent::InitFailed(format!(
            "FRUC warm-up on {} ({}) failed: {e}. If the GPU is newer than the SDK, the driver \
             must JIT-compile the library's PTX; update the NVIDIA driver and retry",
            info.name,
            info.compute_label()
        )));
        return Ok(());
    }
    let warm = warm_started.elapsed();
    let description = format!(
        "NVIDIA FRUC on {} ({}, {}), {}x{}, runtime {} [{}]",
        info.name,
        info.compute_label(),
        info.driver_version_label(),
        width,
        height,
        lib.dir.display(),
        lib.signer
    );
    info!(
        "framegen ready in {:.0} ms (FRUC warm-up {:.0} ms)",
        init_started.elapsed().as_secs_f64() * 1000.0,
        warm.as_secs_f64() * 1000.0
    );
    send(BackendEvent::Ready(BackendInfo {
        description,
        warmup: warm,
        flow_output: false,
    }));

    // --- Job loop -----------------------------------------------------------
    let mut prev_ts: Option<f64> = None;
    let mut need_skip = true;
    for command in commands.iter() {
        let job = match command {
            Command::Process(job) => job,
            Command::Shutdown => break,
        };
        let started = Instant::now();
        // FRUC's CUDA runtime shares the primary context; keep it current.
        if let Err(e) = cuda.make_current() {
            warn!("{e}");
        }
        wait_for_gpu(&device, job.fence.0);
        let gpu_wait = started.elapsed();

        let (stats, motion) = if job.has_reference {
            match readbacks
                .get(job.source)
                .map(|b| read_stats(&device, b, width as usize, height as usize))
            {
                Some(Ok((stats, motion))) => (Some(stats), motion),
                Some(Err(e)) => {
                    warn!("duplicate check unavailable for this frame: {e}");
                    (None, None)
                }
                None => (None, None),
            }
        } else {
            (None, None)
        };
        if let Some(stats) = stats {
            if is_duplicate(&stats, &job.thresholds) {
                send(BackendEvent::Done(JobResult {
                    seq: job.seq,
                    outcome: Outcome::Duplicate(stats),
                    gpu_wait,
                    fruc_time: Duration::ZERO,
                    motion,
                }));
                continue;
            }
        }

        if !job.fruc {
            need_skip = true;
            send(BackendEvent::Done(JobResult {
                seq: job.seq,
                outcome: Outcome::Unique { stats },
                gpu_wait,
                fruc_time: Duration::ZERO,
                motion,
            }));
            continue;
        }

        let in_ts = job.seq as f64 * config_ts_scale;
        let skip = job.skip_warp || need_skip || prev_ts.is_none();
        let out_ts = prev_ts
            .map(|p| (p + in_ts) * 0.5)
            .unwrap_or(in_ts - 0.5 * config_ts_scale);
        let fruc_started = Instant::now();
        let result = session
            .process(
                source_index(job.source),
                output_index(job.output),
                in_ts,
                out_ts,
                skip,
            )
            .and_then(|outcome| cuda.synchronize().map(|_| outcome));
        let fruc_time = fruc_started.elapsed();
        let outcome = match result {
            Ok(outcome) => {
                prev_ts = Some(in_ts);
                need_skip = false;
                if skip {
                    Outcome::Unique { stats }
                } else {
                    Outcome::Interpolated {
                        repeated: outcome.repeated,
                        stats,
                    }
                }
            }
            Err(e) => {
                need_skip = true;
                Outcome::Failed(e)
            }
        };
        send(BackendEvent::Done(JobResult {
            seq: job.seq,
            outcome,
            gpu_wait,
            fruc_time,
            motion,
        }));
    }

    // --- Teardown (reverse order) -------------------------------------------
    if let Err(e) = cuda.synchronize() {
        error!("framegen teardown: {e}");
    }
    drop(session);
    drop(lib);
    drop(sources);
    drop(outputs);
    drop(cuda);
    info!("framegen worker stopped");
    Ok(())
}
