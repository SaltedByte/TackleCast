//! Native frame generation (2x motion-compensated interpolation).
//!
//! Architecture:
//!
//! - [`FrameGenController`] runs on the UI/render thread. It owns slot
//!   bookkeeping, the source clock and presentation schedule, fallback and
//!   diagnostics. It never blocks: GPU waits and interpolation happen on a
//!   worker thread.
//! - A [`Backend`] (on Windows: the NVIDIA FRUC worker in `worker.rs`)
//!   receives one [`Job`] per captured frame, classifies duplicates, runs
//!   NvOFFRUCProcess and reports a [`JobResult`].
//! - The renderer side ([`FgRenderer`], implemented by `render::Renderer`)
//!   converts frames into shared RGBA slots and presents slot images.
//!
//! Original frames are always shown even when interpolation fails; any
//! failure degrades to plain capture rendering with the reason in the UI.

// Some interfaces are only exercised by the Windows backend.
#![cfg_attr(
    not(all(target_os = "windows", feature = "gpu-decode")),
    allow(dead_code)
)]

pub mod duplicate;
pub mod ffi;
pub mod motion;
pub mod pacing;
pub mod recorder;
pub mod stats;

#[cfg(all(target_os = "windows", feature = "gpu-decode"))]
pub mod cuda;
#[cfg(all(target_os = "windows", feature = "gpu-decode"))]
pub mod fruc;
#[cfg(all(target_os = "windows", feature = "gpu-decode"))]
pub mod replay;
#[cfg(all(target_os = "windows", feature = "gpu-decode"))]
pub mod rife;
#[cfg(all(target_os = "windows", feature = "gpu-decode"))]
pub mod selftest;
#[cfg(all(target_os = "windows", feature = "gpu-decode"))]
pub mod worker;

use crate::render::{FgImage, FgImport, FgProtection};
use crate::settings::{
    FrameGenArtifactMode, FrameGenBackend, FrameGenDebugView, FrameGenMode, FrameGenTarget,
    RifeModel, Settings,
};
use duplicate::{DiffStats, DuplicateThresholds};
use pacing::{ArrivalKind, PipelineDelay, PresentQueue, SourceClock};
use stats::{DurationSummary, DurationWindow, PresentTimer, RateCounter};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Source (original) slots: in flight + last unique (FRUC's cached frame)
/// + frames waiting for display + the frame on screen, with headroom.
pub const SOURCE_SLOTS: usize = 6;
/// Interpolated output slots. FRUC accepts at most 10 registered resources.
pub const OUTPUT_SLOTS: usize = 3;
const _: () = assert!(SOURCE_SLOTS + OUTPUT_SLOTS <= ffi::NVOFFRUC_MAX_RESOURCE);

/// Pause interpolation (Target: 60 FPS) above this unique source rate...
const PAUSE_ABOVE_FPS: f32 = 45.0;
/// ...and resume below this one.
const RESUME_BELOW_FPS: f32 = 40.0;
const PAUSE_HYSTERESIS: Duration = Duration::from_secs(2);
/// Consecutive interpolation failures before frame generation gives up.
const MAX_CONSECUTIVE_FAILURES: u32 = 8;
/// A present is "due" this early, to absorb wake-up latency.
const PRESENT_TOLERANCE: Duration = Duration::from_micros(500);
/// Results that take longer than this mean the worker is stuck.
const WORKER_STALL: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct FgConfig {
    pub enabled: bool,
    /// Interpolation engine (NVIDIA FRUC or RIFE).
    pub backend: FrameGenBackend,
    /// RIFE network (RIFE backend only).
    pub rife_model: RifeModel,
    pub target: FrameGenTarget,
    pub runtime_dir: String,
    pub require_signature: bool,
    pub thresholds: DuplicateThresholds,
    pub overlay: bool,
    pub protection: FgProtection,
    /// Mean luma difference (8-bit levels) above which a generated frame is
    /// not shown (scene cut, whip pan, menu opening). 0 = never.
    pub skip_difference: f32,
    /// Scene-cut / full-screen transition handling enabled.
    pub transitions: bool,
}

/// Why a hitch (a present interval over 1.5x the expected one) happened:
/// the last pipeline event that removed a scheduled image before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitchCause {
    /// The capture delivered a repeated frame (game or card cadence).
    SourceDuplicate,
    /// A source frame arrived late or was missing (interval ~2T).
    SourceGap,
    /// FRUC reported a frame repetition (its own fallback).
    FrucRepeat,
    /// FRUC failed for this frame.
    FrucFailure,
    /// The generated frame finished after its display slot.
    LateGenerated,
    /// Scene cut / full-screen transition: generated frame not shown.
    Transition,
    /// No free output buffer for the generated frame.
    NoOutputSlot,
    /// The image was scheduled in time but presented > 4 ms late
    /// (rendering, UI or the swap chain).
    PresentLate,
    /// None of the above (e.g. irregular source timing).
    Other,
}

impl HitchCause {
    pub const ALL: [HitchCause; 9] = [
        Self::SourceDuplicate,
        Self::SourceGap,
        Self::FrucRepeat,
        Self::FrucFailure,
        Self::LateGenerated,
        Self::Transition,
        Self::NoOutputSlot,
        Self::PresentLate,
        Self::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::SourceDuplicate => "source-duplicate",
            Self::SourceGap => "source-gap",
            Self::FrucRepeat => "fruc-repeat",
            Self::FrucFailure => "fruc-failure",
            Self::LateGenerated => "late-generated",
            Self::Transition => "transition-skip",
            Self::NoOutputSlot => "no-output-slot",
            Self::PresentLate => "present-late",
            Self::Other => "other",
        }
    }
}

/// Scene cut / full-screen transition: mean luma change above `threshold`
/// (half of it while already in a transition: hysteresis) that a camera
/// translation does not explain (motion residual > 0.35 or no estimate).
pub fn transition_rule(
    mean_levels: f32,
    motion_residual: Option<f32>,
    in_transition: bool,
    threshold: f32,
) -> bool {
    if threshold <= 0.0 {
        return false;
    }
    let unexplained = motion_residual.is_none_or(|r| r > 0.35);
    let limit = if in_transition {
        threshold * 0.5
    } else {
        threshold
    };
    mean_levels > limit && unexplained
}

/// Correction settings for the selected artifact-reduction mode.
pub fn protection_from_settings(settings: &Settings) -> FgProtection {
    let mode = settings.framegen_artifact_mode;
    let on = mode != FrameGenArtifactMode::Off;
    FgProtection {
        fix_static: on && settings.framegen_fix_static,
        fix_motion: on && settings.framegen_fix_motion,
        fix_edges: on && settings.framegen_fix_edges,
        fix_fades: on && settings.framegen_fix_transitions,
        hud_input_removal: mode == FrameGenArtifactMode::Advanced,
        hud_guard: settings.framegen_hud_guard,
        view: match settings.framegen_debug_view {
            FrameGenDebugView::Final => crate::render::VIEW_FINAL,
            FrameGenDebugView::RawFruc => crate::render::VIEW_RAW,
            FrameGenDebugView::CorrectionOverlay => crate::render::VIEW_OVERLAY,
        },
        zones: settings.framegen_manual_zones,
        zone_width: settings.framegen_manual_zone_width / 100.0,
        zone_height: settings.framegen_manual_zone_height / 100.0,
        motion: None,
    }
}

impl FgConfig {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            enabled: settings.framegen_enabled && settings.framegen_mode == FrameGenMode::Double,
            backend: settings.framegen_backend,
            rife_model: settings.framegen_rife_model,
            target: settings.framegen_target,
            runtime_dir: settings.framegen_runtime_dir.clone(),
            require_signature: settings.framegen_require_signature,
            thresholds: settings.framegen_duplicates.sanitized(),
            overlay: settings.framegen_overlay,
            protection: protection_from_settings(settings),
            transitions: settings.framegen_artifact_mode != FrameGenArtifactMode::Off
                && settings.framegen_fix_transitions,
            skip_difference: settings.framegen_skip_difference,
        }
    }

    /// Settings whose change requires a new backend instance.
    fn backend_key(&self) -> (FrameGenBackend, RifeModel, String, bool) {
        (self.backend, self.rife_model, self.runtime_dir.clone(), self.require_signature)
    }
}

// ---------------------------------------------------------------------------
// Backend interface
// ---------------------------------------------------------------------------

/// GPU completion token for a capture submission (None in tests).
pub struct GpuFence(pub Option<wgpu::SubmissionIndex>);

pub struct Job {
    pub seq: u64,
    pub source: usize,
    pub output: usize,
    pub fence: GpuFence,
    /// Duplicate statistics were computed against a reference slot.
    pub has_reference: bool,
    /// Run FRUC for this frame (false while paused: classify only).
    pub fruc: bool,
    /// Refresh FRUC state without warping (discontinuity).
    pub skip_warp: bool,
    pub thresholds: DuplicateThresholds,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Duplicate(DiffStats),
    /// Interpolated frame written to the job's output slot.
    Interpolated {
        repeated: bool,
        stats: Option<DiffStats>,
    },
    /// Unique frame; FRUC state updated (or skipped) but no output written.
    Unique {
        stats: Option<DiffStats>,
    },
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct JobResult {
    pub seq: u64,
    pub outcome: Outcome,
    /// Time the worker waited for the capture submission to finish.
    pub gpu_wait: Duration,
    /// Time spent inside NvOFFRUCProcess (+ synchronize).
    pub fruc_time: Duration,
    /// Estimated camera motion since the reference frame.
    pub motion: Option<motion::MotionEstimate>,
}

#[derive(Debug, Clone)]
pub struct BackendInfo {
    pub description: String,
    pub warmup: Duration,
    /// Output slots hold flow + blend mask (RIFE `fm` models); the renderer
    /// composes the image when publishing.
    pub flow_output: bool,
}

#[derive(Debug, Clone)]
pub enum BackendEvent {
    Ready(BackendInfo),
    InitFailed(String),
    Done(JobResult),
    Fatal(String),
}

pub trait Backend: Send {
    fn submit(&mut self, job: Job) -> Result<(), String>;
    fn poll(&mut self) -> Option<BackendEvent>;
    /// Stop the backend. `keep_alive` (the GPU resources CUDA imported) must
    /// outlive the backend's CUDA objects and is dropped after it exits.
    fn shutdown(self: Box<Self>, keep_alive: Box<dyn Send>, wait: Option<Duration>);
}

/// Creates a backend for freshly prepared GPU resources.
pub type BackendFactory =
    Box<dyn FnMut(FgImport, Option<wgpu::Device>, &FgConfig) -> Result<Box<dyn Backend>, String>>;

/// Renderer operations the controller needs (implemented by `Renderer`).
pub trait FgRenderer {
    fn fg_prepare(&mut self, width: u32, height: u32) -> Result<FgImport, String>;
    fn fg_release(&mut self) -> Option<Box<dyn Send>>;
    fn fg_capture(
        &mut self,
        slot: usize,
        reference: Option<usize>,
        protection: FgProtection,
    ) -> Option<GpuFence>;
    /// Publish generated output `slot`, which lies between real frames in
    /// source slots `prev` and `next`, applying artifact protection.
    fn fg_publish_output(
        &mut self,
        slot: usize,
        prev: Option<usize>,
        next: usize,
        protection: FgProtection,
    );
    fn set_fg_display(&mut self, image: Option<FgImage>);
    fn current_video_size(&self) -> Option<(u32, u32)>;
    fn device_handle(&self) -> Option<wgpu::Device>;
    /// Artifact-correction statistics and GPU pass times, if available.
    fn fg_report(&self) -> Option<crate::render::FixReport> {
        None
    }
    /// Whether output slots hold flow + mask that the renderer composes.
    fn fg_set_flow_output(&mut self, _flow_output: bool) {}
}

impl FgRenderer for crate::render::Renderer {
    fn fg_prepare(&mut self, width: u32, height: u32) -> Result<FgImport, String> {
        crate::render::Renderer::fg_prepare(self, width, height, SOURCE_SLOTS, OUTPUT_SLOTS)
    }
    fn fg_release(&mut self) -> Option<Box<dyn Send>> {
        crate::render::Renderer::fg_release(self).map(|g| Box::new(g) as Box<dyn Send>)
    }
    fn fg_capture(
        &mut self,
        slot: usize,
        reference: Option<usize>,
        protection: FgProtection,
    ) -> Option<GpuFence> {
        crate::render::Renderer::fg_capture(self, slot, reference, protection)
            .map(|s| GpuFence(Some(s)))
    }
    fn fg_publish_output(
        &mut self,
        slot: usize,
        prev: Option<usize>,
        next: usize,
        protection: FgProtection,
    ) {
        crate::render::Renderer::fg_publish_output(self, slot, prev, next, protection)
    }
    fn set_fg_display(&mut self, image: Option<FgImage>) {
        crate::render::Renderer::set_fg_display(self, image)
    }
    fn fg_report(&self) -> Option<crate::render::FixReport> {
        crate::render::Renderer::fg_report(self)
    }
    fn fg_set_flow_output(&mut self, flow_output: bool) {
        crate::render::Renderer::fg_set_flow_output(self, flow_output)
    }
    fn current_video_size(&self) -> Option<(u32, u32)> {
        crate::render::Renderer::current_video_size(self)
    }
    fn device_handle(&self) -> Option<wgpu::Device> {
        Some(crate::render::Renderer::device_handle(self))
    }
}

/// The production backend factory for this platform.
pub fn platform_backend_factory(waker: Box<dyn Fn() + Send + Sync>) -> BackendFactory {
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    {
        let waker = std::sync::Arc::new(waker);
        Box::new(move |import, device, config| {
            let device = device.ok_or("no GPU device")?;
            let waker = waker.clone();
            Ok(match config.backend {
                FrameGenBackend::NvidiaFruc => worker::spawn(
                    import,
                    device,
                    worker::WorkerConfig {
                        runtime_dir: config.runtime_dir.clone(),
                        require_signature: config.require_signature,
                        timestamp_scale: worker::default_timestamp_scale(),
                    },
                    Box::new(move || (waker)()),
                ),
                FrameGenBackend::Rife => rife::spawn(
                    import,
                    device,
                    rife::RifeConfig {
                        dir: rife::default_dir(),
                        model: config.rife_model.stem().to_string(),
                    },
                    Box::new(move || (waker)()),
                ),
            })
        })
    }
    #[cfg(not(all(target_os = "windows", feature = "gpu-decode")))]
    {
        let _ = waker;
        Box::new(|_, _, _| {
            Err("frame generation requires the Windows build with GPU support".into())
        })
    }
}

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum FgState {
    Disabled,
    /// Resources allocated, waiting for the backend to initialise.
    Starting,
    Active,
    /// Gave up; showing native capture. Carries the reason.
    Failed(String),
}

/// What the caller should do with the frame it just uploaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAction {
    /// Draw the newest YUV frame now (frame generation not presenting).
    RedrawNow,
    /// Frame generation will present it on schedule.
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Original,
    Generated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayItem {
    image: FgImage,
    kind: ItemKind,
}

#[derive(Debug, Default, Clone, Copy)]
struct SlotUse {
    in_flight: bool,
    queued: u8,
    displayed: bool,
}

impl SlotUse {
    fn free(&self) -> bool {
        !self.in_flight && self.queued == 0 && !self.displayed
    }
}

struct Pending {
    seq: u64,
    source: usize,
    output: usize,
    arrival: Instant,
    submitted: Instant,
}

/// Counters and timings for the overlay, UI and logs.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FgDiagnostics {
    pub state: String,
    pub backend: String,
    pub active: bool,
    pub paused: bool,
    pub captured_fps: f32,
    pub unique_fps: f32,
    pub generated_fps: f32,
    pub displayed_fps: f32,
    pub source_period_ms: f32,
    pub pipeline_delay_ms: f32,
    /// Extra latency of original frames versus native display.
    pub added_latency_ms: f32,
    pub fruc_time: DurationSummary,
    pub result_latency: DurationSummary,
    pub gpu_wait: DurationSummary,
    pub frametime: DurationSummary,
    pub captured_total: u64,
    pub unique_total: u64,
    pub generated_total: u64,
    pub duplicates: u64,
    pub dropped_no_slot: u64,
    pub source_gaps: u64,
    pub late_generated: u64,
    /// Generated frames not shown because of a scene cut / full-screen
    /// transition (application decision, not FRUC's).
    pub transition_skips: u64,
    /// Frames with small real changes that the old duplicate rule would
    /// have dropped (now shown).
    pub near_duplicates_kept: u64,
    /// Last camera-motion estimate (px per source frame, x/y) and how much
    /// of the change it leaves unexplained (0 = pure pan).
    pub motion_px: [f32; 2],
    pub motion_residual: f32,
    /// CPU time to encode and submit the capture copy / the publish.
    pub capture_cpu: DurationSummary,
    pub publish_cpu: DurationSummary,
    /// How late presents happened relative to their schedule.
    pub present_lateness: DurationSummary,
    /// Presents more than 4 ms after their scheduled time.
    pub missed_deadlines: u64,
    /// Artifact correction: coverage and GPU pass times.
    pub correction: Option<crate::render::FixReport>,
    /// Hitches by cause (only causes that occurred).
    pub hitch_causes: Vec<(&'static str, u64)>,
    pub fruc_repeats: u64,
    pub failures: u64,
    pub hitches: u64,
    pub resyncs: u64,
    pub queued_presents: usize,
    /// Last duplicate-detection measurement (8-bit luma levels / fraction).
    pub last_diff_mean: f32,
    pub last_diff_changed: f32,
    pub last_diff_max: f32,
    pub last_error: Option<String>,
}

pub struct FrameGenController {
    config: FgConfig,
    backend_key: (FrameGenBackend, RifeModel, String, bool),
    state: FgState,
    factory: BackendFactory,
    backend: Option<Box<dyn Backend>>,
    backend_desc: String,
    dims: Option<(u32, u32)>,
    sources: [SlotUse; SOURCE_SLOTS],
    outputs: [SlotUse; OUTPUT_SLOTS],
    /// FRUC's cached previous frame; must not be overwritten.
    fruc_prev: Option<usize>,
    /// Most recently submitted source slot: the duplicate reference.
    last_submitted: Option<usize>,
    last_submit_arrival: Option<Instant>,
    pending: std::collections::VecDeque<Pending>,
    next_seq: u64,
    need_resync: bool,
    clock: SourceClock,
    delay: PipelineDelay,
    queue: PresentQueue<DisplayItem>,
    current: Option<DisplayItem>,
    new_image_pending_present: bool,
    paused: bool,
    pause_candidate_since: Option<Instant>,
    consecutive_failures: u32,
    // statistics
    captured: RateCounter,
    unique: RateCounter,
    generated: RateCounter,
    displayed: RateCounter,
    fruc_time: DurationWindow,
    result_latency: DurationWindow,
    present_timer: PresentTimer,
    duplicates: u64,
    dropped_no_slot: u64,
    late_generated: u64,
    transition_skips: u64,
    in_transition: bool,
    near_duplicates_kept: u64,
    last_motion: Option<motion::MotionEstimate>,
    capture_cpu: DurationWindow,
    publish_cpu: DurationWindow,
    present_lateness: DurationWindow,
    missed_deadlines: u64,
    /// Scheduled time of the image waiting to be presented.
    pending_present_due: Option<Instant>,
    /// Recent events that removed a scheduled image (for hitch causes).
    skip_events: std::collections::VecDeque<(Instant, HitchCause)>,
    hitch_causes: [u64; 9],
    last_present_at: Option<Instant>,
    gpu_report: Option<crate::render::FixReport>,
    fruc_repeats: u64,
    failures: u64,
    last_error: Option<String>,
    last_summary: Instant,
    last_diff: Option<DiffStats>,
    gpu_wait: DurationWindow,
}

impl FrameGenController {
    pub fn new(config: FgConfig, factory: BackendFactory) -> Self {
        let state = if config.enabled {
            FgState::Starting
        } else {
            FgState::Disabled
        };
        let backend_key = config.backend_key();
        Self {
            config,
            backend_key,
            state,
            factory,
            backend: None,
            backend_desc: String::new(),
            dims: None,
            sources: Default::default(),
            outputs: Default::default(),
            fruc_prev: None,
            last_submitted: None,
            last_submit_arrival: None,
            pending: Default::default(),
            next_seq: 1,
            need_resync: true,
            clock: SourceClock::new(),
            delay: PipelineDelay::new(Duration::from_millis(5)),
            queue: PresentQueue::default(),
            current: None,
            new_image_pending_present: false,
            paused: false,
            pause_candidate_since: None,
            consecutive_failures: 0,
            captured: RateCounter::default(),
            unique: RateCounter::default(),
            generated: RateCounter::default(),
            displayed: RateCounter::default(),
            fruc_time: DurationWindow::new(240),
            result_latency: DurationWindow::new(240),
            present_timer: PresentTimer::default(),
            duplicates: 0,
            dropped_no_slot: 0,
            late_generated: 0,
            transition_skips: 0,
            in_transition: false,
            near_duplicates_kept: 0,
            last_motion: None,
            capture_cpu: DurationWindow::new(600),
            publish_cpu: DurationWindow::new(600),
            present_lateness: DurationWindow::new(600),
            missed_deadlines: 0,
            pending_present_due: None,
            skip_events: std::collections::VecDeque::with_capacity(64),
            hitch_causes: [0; 9],
            last_present_at: None,
            gpu_report: None,
            fruc_repeats: 0,
            failures: 0,
            last_error: None,
            last_summary: Instant::now(),
            last_diff: None,
            gpu_wait: DurationWindow::new(240),
        }
    }

    pub fn state(&self) -> &FgState {
        &self.state
    }

    pub fn config(&self) -> &FgConfig {
        &self.config
    }

    /// Apply new settings. Turning off releases everything; changing the
    /// runtime folder restarts the backend; failures can be retried by
    /// toggling or by changing settings.
    pub fn configure(&mut self, config: FgConfig, renderer: &mut dyn FgRenderer) {
        if config == self.config && !matches!(self.state, FgState::Failed(_)) {
            return;
        }
        let key = config.backend_key();
        let was_enabled = self.config.enabled;
        let restart = key != self.backend_key || matches!(self.state, FgState::Failed(_));
        self.config = config;
        self.backend_key = key;
        if !self.config.enabled {
            if was_enabled || self.backend.is_some() {
                info!("frame generation disabled");
            }
            self.stop(renderer, None);
            self.state = FgState::Disabled;
            return;
        }
        if restart || !was_enabled {
            self.stop(renderer, None);
            self.state = FgState::Starting;
            info!("frame generation enabled; starting on the next captured frame");
        }
    }

    /// Capture restarted, window minimised, or similar: frame timing and
    /// FRUC history are no longer continuous.
    pub fn break_continuity(&mut self, renderer: &mut dyn FgRenderer) {
        self.need_resync = true;
        self.clock.break_continuity();
        self.present_timer.break_continuity();
        self.clear_schedule(renderer);
    }

    fn clear_schedule(&mut self, renderer: &mut dyn FgRenderer) {
        for item in self.queue.drain() {
            self.release_item(item, false);
        }
        if let Some(item) = self.current.take() {
            self.release_item(item, true);
        }
        renderer.set_fg_display(None);
    }

    /// Release everything (disable, failure, resize, exit).
    fn stop(&mut self, renderer: &mut dyn FgRenderer, wait: Option<Duration>) {
        self.clear_schedule(renderer);
        let keep_alive = renderer.fg_release();
        if let Some(backend) = self.backend.take() {
            backend.shutdown(keep_alive.unwrap_or_else(|| Box::new(())), wait);
        }
        self.dims = None;
        self.sources = Default::default();
        self.outputs = Default::default();
        self.fruc_prev = None;
        self.last_submitted = None;
        self.last_submit_arrival = None;
        self.pending.clear();
        self.need_resync = true;
        self.paused = false;
        self.pause_candidate_since = None;
        self.consecutive_failures = 0;
        self.clock.reset();
        self.present_timer.break_continuity();
    }

    /// Shut down at application exit, waiting briefly for CUDA teardown.
    pub fn shutdown(&mut self, renderer: &mut dyn FgRenderer) {
        self.stop(renderer, Some(Duration::from_millis(1500)));
    }

    fn fail(&mut self, renderer: &mut dyn FgRenderer, reason: String) {
        warn!("frame generation disabled for this session: {reason}");
        self.last_error = Some(reason.clone());
        self.stop(renderer, None);
        self.state = FgState::Failed(reason);
    }

    /// Retry after a failure (UI button).
    pub fn retry(&mut self, renderer: &mut dyn FgRenderer) {
        if matches!(self.state, FgState::Failed(_)) && self.config.enabled {
            self.stop(renderer, None);
            self.state = FgState::Starting;
        }
    }

    fn start(&mut self, renderer: &mut dyn FgRenderer, width: u32, height: u32) {
        match renderer.fg_prepare(width, height) {
            Ok(import) => {
                let device = renderer.device_handle();
                match (self.factory)(import, device, &self.config) {
                    Ok(backend) => {
                        info!("frame generation: initialising backend for {width}x{height}");
                        self.backend = Some(backend);
                        self.dims = Some((width, height));
                        self.state = FgState::Starting;
                        self.backend_desc = format!("Initialising {}...", self.config.backend);
                    }
                    Err(e) => self.fail(renderer, e),
                }
            }
            Err(e) => self.fail(renderer, format!("could not allocate GPU resources: {e}")),
        }
    }

    /// A source slot nobody needs: not in flight, queued or on screen, not
    /// FRUC's cached previous frame, and not the duplicate-check reference
    /// (overwriting the reference would compare a frame with itself and read
    /// stale "identical" statistics, which froze the picture).
    fn free_source(&self) -> Option<usize> {
        (0..SOURCE_SLOTS).find(|&i| {
            self.sources[i].free() && self.fruc_prev != Some(i) && self.last_submitted != Some(i)
        })
    }

    fn free_output(&self) -> Option<usize> {
        (0..OUTPUT_SLOTS).find(|&i| self.outputs[i].free())
    }

    /// Called after the renderer uploaded a new captured frame.
    pub fn on_frame_uploaded(
        &mut self,
        renderer: &mut dyn FgRenderer,
        now: Instant,
    ) -> FrameAction {
        if !self.config.enabled || matches!(self.state, FgState::Disabled | FgState::Failed(_)) {
            return FrameAction::RedrawNow;
        }
        let Some((width, height)) = renderer.current_video_size() else {
            return FrameAction::RedrawNow;
        };
        if self.dims != Some((width, height)) {
            if self.dims.is_some() {
                info!("frame generation: source size changed to {width}x{height}; restarting");
            }
            self.stop(renderer, None);
            self.start(renderer, width, height);
            return FrameAction::RedrawNow;
        }
        if self.state != FgState::Active {
            return FrameAction::RedrawNow;
        }
        self.captured.record(now);

        let Some(source) = self.free_source() else {
            self.dropped_no_slot += 1;
            self.need_resync = true;
            return if self.paused {
                FrameAction::RedrawNow
            } else {
                FrameAction::Deferred
            };
        };
        let output = self.free_output();
        let reference = self.last_submitted.filter(|r| *r != source);
        let started = Instant::now();
        // The capture-side HUD guard fills along the camera motion; the
        // latest estimate (previous pair) is the best available guess.
        let mut protection = self.config.protection;
        protection.motion = self
            .last_motion
            .filter(|m| m.residual < 0.5)
            .map(|m| m.shift);
        let Some(fence) = renderer.fg_capture(source, reference, protection) else {
            return FrameAction::RedrawNow;
        };
        self.capture_cpu.record(started.elapsed());

        // A long pause since the previous frame means FRUC must not
        // interpolate across it.
        let period = self
            .clock
            .period()
            .unwrap_or(Duration::from_secs_f64(1.0 / 30.0));
        let gap = self.last_submit_arrival.is_some_and(|last| {
            now.saturating_duration_since(last).as_secs_f64()
                > pacing::MAX_PERIOD.max(period.as_secs_f64() * 2.5)
        });
        let skip_warp = self.need_resync || gap;
        let seq = self.next_seq;
        self.next_seq += 1;
        // Without a free output slot FRUC must not run at all: any output
        // index would alias a buffer whose frame may still be on its way
        // to the screen. The frame is still classified and shown.
        let job = Job {
            seq,
            source,
            output: output.unwrap_or(usize::MAX),
            fence,
            has_reference: reference.is_some(),
            fruc: !self.paused && output.is_some(),
            skip_warp,
            thresholds: self.config.thresholds,
        };
        let Some(backend) = self.backend.as_mut() else {
            return FrameAction::RedrawNow;
        };
        if let Err(e) = backend.submit(job) {
            self.fail(renderer, format!("frame generation worker stopped: {e}"));
            return FrameAction::RedrawNow;
        }
        self.need_resync = false;
        self.sources[source].in_flight = true;
        if let Some(o) = output {
            self.outputs[o].in_flight = true;
        }
        self.pending.push_back(Pending {
            seq,
            source,
            output: output.unwrap_or(usize::MAX),
            arrival: now,
            submitted: now,
        });
        self.last_submitted = Some(source);
        self.last_submit_arrival = Some(now);
        if self.paused {
            FrameAction::RedrawNow
        } else {
            FrameAction::Deferred
        }
    }

    /// Drain backend events. Returns true when the screen may need a redraw.
    pub fn process_events(&mut self, renderer: &mut dyn FgRenderer, now: Instant) -> bool {
        let mut redraw = false;
        while let Some(event) = self.backend.as_mut().and_then(|backend| backend.poll()) {
            match event {
                BackendEvent::Ready(info) => {
                    info!(
                        "frame generation active: {} (warm-up {:.0} ms)",
                        info.description,
                        info.warmup.as_secs_f64() * 1000.0
                    );
                    self.backend_desc = info.description;
                    renderer.fg_set_flow_output(info.flow_output);
                    self.state = FgState::Active;
                    self.need_resync = true;
                    redraw = true;
                }
                BackendEvent::InitFailed(reason) => {
                    self.fail(renderer, reason);
                    return true;
                }
                BackendEvent::Fatal(reason) => {
                    self.fail(renderer, format!("frame generation worker error: {reason}"));
                    return true;
                }
                BackendEvent::Done(result) => {
                    redraw |= self.on_result(renderer, result, now);
                    if matches!(self.state, FgState::Failed(_)) {
                        return true;
                    }
                }
            }
        }
        // Detect a worker that stopped answering.
        if let Some(front) = self.pending.front() {
            if now.saturating_duration_since(front.submitted) > WORKER_STALL {
                self.fail(
                    renderer,
                    "the frame generation worker stopped responding".into(),
                );
                return true;
            }
        }
        if self.last_summary.elapsed() >= Duration::from_secs(30) && self.state == FgState::Active {
            self.log_summary(now);
        }
        redraw
    }

    fn on_result(
        &mut self,
        renderer: &mut dyn FgRenderer,
        result: JobResult,
        now: Instant,
    ) -> bool {
        let Some(index) = self.pending.iter().position(|p| p.seq == result.seq) else {
            return false;
        };
        let pending = self.pending.remove(index).expect("index valid");
        self.sources[pending.source].in_flight = false;
        let output = (pending.output != usize::MAX).then_some(pending.output);
        if let Some(o) = output {
            self.outputs[o].in_flight = false;
        }
        self.result_latency
            .record(now.saturating_duration_since(pending.arrival));
        self.gpu_wait.record(result.gpu_wait);

        if let Outcome::Duplicate(stats)
        | Outcome::Interpolated {
            stats: Some(stats), ..
        }
        | Outcome::Unique { stats: Some(stats) } = &result.outcome
        {
            self.last_diff = Some(*stats);
        }
        let (unique, generated, repeated) = match &result.outcome {
            Outcome::Duplicate(_) => {
                self.duplicates += 1;
                self.note_skip(now, HitchCause::SourceDuplicate);
                return false;
            }
            Outcome::Interpolated { repeated, .. } => (true, true, *repeated),
            Outcome::Unique { .. } => (true, false, false),
            Outcome::Failed(reason) => {
                self.failures += 1;
                self.note_skip(now, HitchCause::FrucFailure);
                self.consecutive_failures += 1;
                if self.consecutive_failures == 1 || self.failures.is_multiple_of(100) {
                    warn!("frame interpolation failed (showing original): {reason}");
                }
                self.last_error = Some(reason.clone());
                self.need_resync = true;
                if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    let reason = reason.clone();
                    self.fail(renderer, format!("interpolation keeps failing: {reason}"));
                    return true;
                }
                (true, false, false)
            }
        };
        if !matches!(result.outcome, Outcome::Failed(_)) {
            self.consecutive_failures = 0;
        }
        if generated {
            self.fruc_time.record(result.fruc_time);
        }
        debug_assert!(unique);
        self.unique.record(now);
        // The real frame before this one (what FRUC interpolated from).
        let prev_source = self.fruc_prev.filter(|p| *p != pending.source);
        let stats = match &result.outcome {
            Outcome::Interpolated { stats, .. } | Outcome::Unique { stats } => *stats,
            _ => None,
        };
        // Frames the previous (too loose) duplicate rule would have dropped
        // but which do contain changes: counted to confirm the fix in logs.
        if stats.is_some_and(|d| {
            d.mean_levels() <= 0.6 && d.changed_fraction() <= 0.002 && d.samples > 0
        }) {
            self.near_duplicates_kept += 1;
        }
        if let Some(motion) = result.motion {
            self.last_motion = Some(motion);
        }
        let cut = self.is_transition(stats, result.motion);
        // FRUC now caches this frame as "previous" (it processed it).
        if !matches!(result.outcome, Outcome::Failed(_)) {
            self.fruc_prev = Some(pending.source);
        }

        let hint = Some(1.0 / 30.0);
        let gaps_before = self.clock.gaps();
        let arrival = self.clock.on_unique_frame(pending.arrival, hint);
        if self.clock.gaps() > gaps_before {
            self.note_skip(now, HitchCause::SourceGap);
        }
        let period = arrival.period;
        self.delay
            .observe(now.saturating_duration_since(pending.arrival), period);
        self.update_pause(now);
        if self.paused {
            // Native rate is already high: present originals immediately
            // through the normal YUV path.
            return false;
        }

        let delay = self.delay.current();
        let half = period / 2;
        let original_at = pacing::offset(arrival.phase, (delay + half).as_secs_f64()).max(now);
        if generated {
            let generated_at = pacing::offset(arrival.phase, delay.as_secs_f64());
            let continuous = arrival.kind == ArrivalKind::Continuous;
            // Too late to sit evenly between the two originals: drop it.
            let too_late = now > generated_at + period / 4;
            if repeated {
                // FRUC fell back to a frame repeat; showing it would be a
                // visible hitch, so present the original timeline only.
                self.fruc_repeats += 1;
                self.note_skip(now, HitchCause::FrucRepeat);
            } else if !continuous {
                // Interpolating across a gap: not a valid in-between frame.
                self.note_skip(now, HitchCause::SourceGap);
            } else if too_late {
                self.late_generated += 1;
                self.note_skip(now, HitchCause::LateGenerated);
            } else if cut {
                // Scene cut / full-screen change: no valid in-between
                // image exists; this one interval shows the originals.
                self.transition_skips += 1;
                self.note_skip(now, HitchCause::Transition);
            } else if let Some(o) = output {
                let mut protection = self.config.protection;
                protection.motion = result.motion.map(|m| m.shift);
                let started = Instant::now();
                renderer.fg_publish_output(o, prev_source, pending.source, protection);
                self.publish_cpu.record(started.elapsed());
                self.gpu_report = renderer.fg_report();
                self.outputs[o].queued += 1;
                self.queue.push(
                    generated_at.max(now),
                    DisplayItem {
                        image: FgImage::Generated(o),
                        kind: ItemKind::Generated,
                    },
                );
            } else {
                self.note_skip(now, HitchCause::NoOutputSlot);
            }
        }
        self.sources[pending.source].queued += 1;
        self.queue.push(
            original_at,
            DisplayItem {
                image: FgImage::Source(pending.source),
                kind: ItemKind::Original,
            },
        );
        true
    }

    /// Scene cut or full-screen transition: a large luma change that camera
    /// motion does not explain. Hysteresis: once in a transition, a smaller
    /// change still counts, so an ongoing animation (map zoom, menu wipe)
    /// does not flip between interpolated and skipped frames.
    fn is_transition(
        &mut self,
        stats: Option<DiffStats>,
        motion: Option<motion::MotionEstimate>,
    ) -> bool {
        if !self.config.transitions || self.config.skip_difference <= 0.0 {
            self.in_transition = false;
            return false;
        }
        let Some(stats) = stats else {
            return false;
        };
        let cut = transition_rule(
            stats.mean_levels(),
            motion.map(|m| m.residual),
            self.in_transition,
            self.config.skip_difference,
        );
        self.in_transition = cut;
        cut
    }

    fn update_pause(&mut self, now: Instant) {
        if self.config.target != FrameGenTarget::Fps60 {
            self.paused = false;
            return;
        }
        let rate = self.unique.rate(now);
        let wants_change = if self.paused {
            rate < RESUME_BELOW_FPS
        } else {
            rate > PAUSE_ABOVE_FPS
        };
        if !wants_change {
            self.pause_candidate_since = None;
            return;
        }
        let since = *self.pause_candidate_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= PAUSE_HYSTERESIS {
            self.paused = !self.paused;
            self.pause_candidate_since = None;
            self.need_resync = true;
            info!(
                "frame generation {} (unique source {:.1} FPS, target 60 FPS)",
                if self.paused { "paused" } else { "resumed" },
                rate
            );
        }
    }

    fn release_item(&mut self, item: DisplayItem, was_displayed: bool) {
        let slot = match item.image {
            FgImage::Source(i) => &mut self.sources[i],
            FgImage::Generated(i) => &mut self.outputs[i],
        };
        if was_displayed {
            slot.displayed = false;
        } else {
            slot.queued = slot.queued.saturating_sub(1);
        }
    }

    /// Show whatever is due. Returns true when a new image must be drawn.
    pub fn poll_display(&mut self, renderer: &mut dyn FgRenderer, now: Instant) -> bool {
        if self.state != FgState::Active {
            return false;
        }
        if self.paused {
            if self.current.is_some() || !self.queue.is_empty() {
                self.clear_schedule(renderer);
                return true;
            }
            return false;
        }
        let due = self.queue.take_due(now, PRESENT_TOLERANCE);
        for item in due.skipped {
            if item.kind == ItemKind::Generated {
                self.late_generated += 1;
                self.note_skip(now, HitchCause::LateGenerated);
            }
            self.release_item(item, false);
        }
        let Some(item) = due.show else {
            return false;
        };
        self.pending_present_due = due.show_at;
        // queued -> displayed
        self.release_item(item, false);
        match item.image {
            FgImage::Source(i) => self.sources[i].displayed = true,
            FgImage::Generated(i) => self.outputs[i].displayed = true,
        }
        if let Some(previous) = self.current.replace(item) {
            self.release_item(previous, true);
            // The same slot may be shown again; keep it marked.
            match item.image {
                FgImage::Source(i) => self.sources[i].displayed = true,
                FgImage::Generated(i) => self.outputs[i].displayed = true,
            }
        }
        if item.kind == ItemKind::Generated {
            self.generated.record(now);
        }
        renderer.set_fg_display(Some(item.image));
        self.new_image_pending_present = true;
        true
    }

    /// When the event loop must wake for the next scheduled present.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.state == FgState::Active && !self.paused {
            self.queue.next_deadline()
        } else {
            None
        }
    }

    /// Called right after a frame was presented.
    pub fn on_presented(&mut self, now: Instant) {
        if !self.new_image_pending_present {
            return;
        }
        self.new_image_pending_present = false;
        self.displayed.record(now);
        let mut late = Duration::ZERO;
        if let Some(due) = self.pending_present_due.take() {
            late = now.saturating_duration_since(due);
            self.present_lateness.record(late);
            if late > Duration::from_millis(4) {
                self.missed_deadlines += 1;
            }
        }
        let expected = self.clock.period().map(|p| p / 2);
        if self.present_timer.record(now, expected) {
            let cause = self.hitch_cause(now, late, expected.unwrap_or_default());
            self.hitch_causes[HitchCause::ALL
                .iter()
                .position(|c| *c == cause)
                .unwrap_or(8)] += 1;
        }
        self.last_present_at = Some(now);
    }

    fn note_skip(&mut self, now: Instant, cause: HitchCause) {
        if self.skip_events.len() == 64 {
            self.skip_events.pop_front();
        }
        self.skip_events.push_back((now, cause));
    }

    /// The most recent skip event since shortly before the previous
    /// present (one source frame earlier), else presentation lateness.
    fn hitch_cause(&self, now: Instant, late: Duration, expected: Duration) -> HitchCause {
        let since = self
            .last_present_at
            .map(|t| t.checked_sub(expected * 2).unwrap_or(t))
            .unwrap_or(now);
        if let Some((_, cause)) = self
            .skip_events
            .iter()
            .rev()
            .find(|(t, _)| *t >= since && *t <= now)
        {
            return *cause;
        }
        if late > Duration::from_millis(4) {
            HitchCause::PresentLate
        } else {
            HitchCause::Other
        }
    }

    pub fn diagnostics(&mut self, now: Instant) -> FgDiagnostics {
        let state = match &self.state {
            FgState::Disabled => "Off".to_string(),
            FgState::Starting => "Starting".to_string(),
            FgState::Active if self.paused => "Paused (source already fast)".to_string(),
            FgState::Active => "Active".to_string(),
            FgState::Failed(reason) => format!("Unavailable: {reason}"),
        };
        let period = self.clock.period();
        let delay = self.delay.current();
        FgDiagnostics {
            state,
            backend: self.backend_desc.clone(),
            active: self.state == FgState::Active,
            paused: self.paused,
            captured_fps: self.captured.rate(now),
            unique_fps: self.unique.rate(now),
            generated_fps: self.generated.rate(now),
            displayed_fps: self.displayed.rate(now),
            source_period_ms: period.map(|p| p.as_secs_f32() * 1000.0).unwrap_or(0.0),
            pipeline_delay_ms: delay.as_secs_f32() * 1000.0,
            added_latency_ms: period
                .map(|p| (delay + p / 2).as_secs_f32() * 1000.0)
                .unwrap_or(0.0),
            fruc_time: self.fruc_time.summary(),
            result_latency: self.result_latency.summary(),
            gpu_wait: self.gpu_wait.summary(),
            frametime: self.present_timer.summary(),
            captured_total: self.captured.total(),
            unique_total: self.unique.total(),
            generated_total: self.generated.total(),
            duplicates: self.duplicates,
            dropped_no_slot: self.dropped_no_slot,
            source_gaps: self.clock.gaps(),
            late_generated: self.late_generated,
            transition_skips: self.transition_skips,
            near_duplicates_kept: self.near_duplicates_kept,
            motion_px: self.last_motion.map(|m| m.shift).unwrap_or([0.0; 2]),
            motion_residual: self.last_motion.map(|m| m.residual).unwrap_or(0.0),
            capture_cpu: self.capture_cpu.summary(),
            publish_cpu: self.publish_cpu.summary(),
            present_lateness: self.present_lateness.summary(),
            missed_deadlines: self.missed_deadlines,
            correction: self.gpu_report.clone(),
            hitch_causes: HitchCause::ALL
                .iter()
                .zip(self.hitch_causes)
                .filter(|(_, n)| *n > 0)
                .map(|(c, n)| (c.label(), n))
                .collect(),
            fruc_repeats: self.fruc_repeats,
            failures: self.failures,
            hitches: self.present_timer.hitches(),
            resyncs: self.clock.resyncs(),
            queued_presents: self.queue.len(),
            last_diff_mean: self.last_diff.map(|d| d.mean_levels()).unwrap_or(0.0),
            last_diff_changed: self.last_diff.map(|d| d.changed_fraction()).unwrap_or(0.0),
            last_diff_max: self.last_diff.map(|d| d.max_levels()).unwrap_or(0.0),
            last_error: self.last_error.clone(),
        }
    }

    /// Raw recent present intervals (ms) for reports.
    pub fn present_intervals_ms(&self) -> Vec<f32> {
        self.present_timer.intervals().values().collect()
    }

    fn log_summary(&mut self, now: Instant) {
        self.last_summary = now;
        let d = self.diagnostics(now);
        info!(
            "framegen summary: {} | captured {:.1} unique {:.1} generated {:.1} displayed {:.1} fps | \
             FRUC {:.2}/{:.2}/{:.2} ms (mean/p95/p99) | frametime {:.2}±{:.2} ms (p99 {:.2}, max {:.2}) | \
             +{:.1} ms latency | dup {} (near-dup kept {}) drop {} gaps {} late {} transition-skips {} \
             FRUC-repeats {} fail {} hitches {} | present lateness p99 {:.2} ms, missed {} | \
             CPU capture {:.2}/{:.2} publish {:.2}/{:.2} ms (mean/p99)",
            d.state,
            d.captured_fps,
            d.unique_fps,
            d.generated_fps,
            d.displayed_fps,
            d.fruc_time.mean_ms,
            d.fruc_time.p95_ms,
            d.fruc_time.p99_ms,
            d.frametime.mean_ms,
            d.frametime.stddev_ms,
            d.frametime.p99_ms,
            d.frametime.max_ms,
            d.added_latency_ms,
            d.duplicates,
            d.near_duplicates_kept,
            d.dropped_no_slot,
            d.source_gaps,
            d.late_generated,
            d.transition_skips,
            d.fruc_repeats,
            d.failures,
            d.hitches,
            d.present_lateness.p99_ms,
            d.missed_deadlines,
            d.capture_cpu.mean_ms,
            d.capture_cpu.p99_ms,
            d.publish_cpu.mean_ms,
            d.publish_cpu.p99_ms,
        );
        if !d.hitch_causes.is_empty() {
            info!(
                "framegen hitch causes (since start): {}",
                d.hitch_causes
                    .iter()
                    .map(|(c, n)| format!("{c} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(c) = &d.correction {
            let gpu = |s: &Option<DurationSummary>| {
                s.as_ref()
                    .map(|s| format!("{:.3}/{:.3}/{:.3} ms", s.mean_ms, s.p95_ms, s.p99_ms))
                    .unwrap_or_else(|| "n/a".into())
            };
            info!(
                "framegen corrections ({:?}): corrected area mean {:.2}% max {:.2}% | \
                 GPU correction {} capture-side {} (mean/p95/p99){}",
                self.config.protection,
                c.mean_corrected_pct,
                c.max_corrected_pct,
                gpu(&c.fix_gpu),
                gpu(&c.prep_gpu),
                if c.fix_failed {
                    " | correction pass FAILED to build"
                } else {
                    ""
                },
            );
        }
    }
}

#[cfg(test)]
mod tests;
