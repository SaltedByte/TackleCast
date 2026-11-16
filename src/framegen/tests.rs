//! Controller tests with a mock renderer and a mock backend on a simulated
//! clock. They cover pacing at 30 and 29.97 FPS, duplicate handling, late
//! results, failure fallback, enable/disable/re-enable, resizes and pause.

use super::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RendererLog {
    prepared: usize,
    released: usize,
    captures: Vec<(usize, Option<usize>)>,
    published: Vec<usize>,
    display: Option<FgImage>,
}

struct MockRenderer {
    size: Option<(u32, u32)>,
    log: Arc<Mutex<RendererLog>>,
}

impl FgRenderer for MockRenderer {
    fn fg_prepare(&mut self, width: u32, height: u32) -> Result<FgImport, String> {
        self.log.lock().unwrap().prepared += 1;
        Ok(FgImport {
            width,
            height,
            pitch: width * 4,
            buffer_size: (width * 4 * height) as u64,
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            source_handles: Vec::new(),
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            output_handles: Vec::new(),
            readbacks: Vec::new(),
            adapter_luid: None,
        })
    }
    fn fg_release(&mut self) -> Option<Box<dyn Send>> {
        self.log.lock().unwrap().released += 1;
        self.log.lock().unwrap().display = None;
        Some(Box::new(()))
    }
    fn fg_capture(
        &mut self,
        slot: usize,
        reference: Option<usize>,
        _protection: crate::render::FgProtection,
    ) -> Option<GpuFence> {
        let mut log = self.log.lock().unwrap();
        // Writing into the slot on screen would tear the visible image.
        assert_ne!(
            log.display,
            Some(FgImage::Source(slot)),
            "captured into the displayed slot"
        );
        assert_ne!(reference, Some(slot), "frame compared with its own slot");
        log.captures.push((slot, reference));
        Some(GpuFence(None))
    }
    fn fg_publish_output(
        &mut self,
        slot: usize,
        prev: Option<usize>,
        next: usize,
        _protection: crate::render::FgProtection,
    ) {
        assert_ne!(prev, Some(next), "protection compares a frame with itself");
        self.log.lock().unwrap().published.push(slot);
    }
    fn set_fg_display(&mut self, image: Option<FgImage>) {
        self.log.lock().unwrap().display = image;
    }
    fn current_video_size(&self) -> Option<(u32, u32)> {
        self.size
    }
    fn device_handle(&self) -> Option<wgpu::Device> {
        None
    }
}

/// What the mock backend does with each job.
#[derive(Clone, Copy)]
enum Behaviour {
    Interpolate,
    Duplicate,
    Fail,
    /// Interpolated, but the frame differs hugely from the previous one.
    BigChange,
    /// Interpolated during a very fast camera turn (300 px per frame).
    FastPan,
}

struct MockShared {
    /// Every (fruc, output) pair the controller submitted.
    fruc_outputs: Vec<(bool, usize)>,
    now: Instant,
    processing: Duration,
    jobs: Vec<(Instant, Job, Behaviour)>,
    outbox: Vec<BackendEvent>,
    behaviour: Box<dyn FnMut(u64) -> Behaviour + Send>,
    shutdowns: usize,
    init: Result<(), String>,
    ready_sent: bool,
    submitted: u64,
}

struct MockBackend(Arc<Mutex<MockShared>>);

impl Backend for MockBackend {
    fn submit(&mut self, job: Job) -> Result<(), String> {
        let mut s = self.0.lock().unwrap();
        let behaviour = (s.behaviour)(job.seq);
        s.fruc_outputs.push((job.fruc, job.output));
        let ready = s.now + s.processing;
        s.submitted += 1;
        s.jobs.push((ready, job, behaviour));
        Ok(())
    }
    fn poll(&mut self) -> Option<BackendEvent> {
        let mut s = self.0.lock().unwrap();
        if !s.ready_sent {
            s.ready_sent = true;
            return Some(match s.init.clone() {
                Ok(()) => BackendEvent::Ready(BackendInfo {
                    description: "mock".into(),
                    warmup: Duration::ZERO,
                    flow_output: false,
                }),
                Err(e) => BackendEvent::InitFailed(e),
            });
        }
        if !s.outbox.is_empty() {
            return Some(s.outbox.remove(0));
        }
        let now = s.now;
        if s.jobs.first().is_some_and(|(ready, _, _)| *ready <= now) {
            let (_, job, behaviour) = s.jobs.remove(0);
            let outcome = match behaviour {
                Behaviour::Duplicate if job.has_reference => {
                    Outcome::Duplicate(DiffStats::default())
                }
                Behaviour::Fail => Outcome::Failed("mock failure".into()),
                _ if !job.fruc || job.skip_warp => Outcome::Unique { stats: None },
                Behaviour::BigChange => Outcome::Interpolated {
                    repeated: false,
                    // Mean difference 60 levels over 1000 samples.
                    stats: Some(DiffStats {
                        sum_fixed: 60 * 16 * 1000,
                        changed: 900,
                        samples: 1000,
                        max_fixed: 255 * 16,
                    }),
                },
                _ => Outcome::Interpolated {
                    repeated: false,
                    stats: None,
                },
            };
            let motion = Some(match behaviour {
                // A whip pan: large but fully explained by camera motion.
                Behaviour::FastPan => super::motion::MotionEstimate {
                    shift: [300.0, 0.0],
                    residual: 0.05,
                },
                // A scene cut: large change that no translation explains.
                Behaviour::BigChange => super::motion::MotionEstimate {
                    shift: [3.0, 0.0],
                    residual: 0.9,
                },
                _ => super::motion::MotionEstimate {
                    shift: [20.0, 0.0],
                    residual: 0.05,
                },
            });
            return Some(BackendEvent::Done(JobResult {
                seq: job.seq,
                outcome,
                gpu_wait: Duration::ZERO,
                fruc_time: Duration::from_millis(3),
                motion,
            }));
        }
        None
    }
    fn shutdown(self: Box<Self>, _keep_alive: Box<dyn Send>, _wait: Option<Duration>) {
        self.0.lock().unwrap().shutdowns += 1;
    }
}

struct Harness {
    controller: FrameGenController,
    renderer: MockRenderer,
    log: Arc<Mutex<RendererLog>>,
    shared: Arc<Mutex<MockShared>>,
    factory_calls: Arc<Mutex<usize>>,
    start: Instant,
    presents: Vec<(f64, ItemKindView)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKindView {
    Original,
    Generated,
}

fn config(enabled: bool) -> FgConfig {
    FgConfig {
        enabled,
        backend: crate::settings::FrameGenBackend::NvidiaFruc,
        rife_model: crate::settings::RifeModel::default(),
        target: FrameGenTarget::Fps60,
        runtime_dir: String::new(),
        require_signature: true,
        thresholds: DuplicateThresholds::default(),
        overlay: false,
        protection: crate::render::FgProtection {
            fix_static: true,
            fix_motion: true,
            fix_edges: true,
            fix_fades: true,
            ..Default::default()
        },
        skip_difference: 32.0,
        transitions: true,
    }
}

impl Harness {
    fn new(enabled: bool, behaviour: impl FnMut(u64) -> Behaviour + Send + 'static) -> Self {
        let start = Instant::now();
        let shared = Arc::new(Mutex::new(MockShared {
            fruc_outputs: Vec::new(),
            now: start,
            processing: Duration::from_millis(4),
            jobs: Vec::new(),
            outbox: Vec::new(),
            behaviour: Box::new(behaviour),
            shutdowns: 0,
            init: Ok(()),
            ready_sent: false,
            submitted: 0,
        }));
        let factory_calls = Arc::new(Mutex::new(0));
        let factory: BackendFactory = {
            let shared = shared.clone();
            let calls = factory_calls.clone();
            Box::new(move |_import, _device, _config| {
                *calls.lock().unwrap() += 1;
                shared.lock().unwrap().ready_sent = false;
                Ok(Box::new(MockBackend(shared.clone())) as Box<dyn Backend>)
            })
        };
        let log = Arc::new(Mutex::new(RendererLog::default()));
        Self {
            controller: FrameGenController::new(config(enabled), factory),
            renderer: MockRenderer {
                size: Some((2560, 1440)),
                log: log.clone(),
            },
            log,
            shared,
            factory_calls,
            start,
            presents: Vec::new(),
        }
    }

    fn at(&self, seconds: f64) -> Instant {
        self.start + Duration::from_secs_f64(seconds)
    }

    /// Run a source at `fps` (each frame repeated `repeat` times by the
    /// "capture card") for `seconds`, stepping the event loop every 0.25 ms.
    fn run(&mut self, fps: f64, repeat: u32, seconds: f64, from: f64) -> Vec<FrameAction> {
        let mut actions = Vec::new();
        let capture_interval = 1.0 / (fps * repeat as f64);
        let mut next_frame = from;
        let step = 0.00025;
        let mut t = from;
        while t < from + seconds {
            let now = self.at(t);
            self.shared.lock().unwrap().now = now;
            if t >= next_frame {
                actions.push(self.controller.on_frame_uploaded(&mut self.renderer, now));
                next_frame += capture_interval;
            }
            self.controller.process_events(&mut self.renderer, now);
            if self.controller.poll_display(&mut self.renderer, now) {
                let kind = match self.log.lock().unwrap().display {
                    Some(FgImage::Generated(_)) => ItemKindView::Generated,
                    _ => ItemKindView::Original,
                };
                self.presents.push((t, kind));
                self.controller.on_presented(now);
            }
            t += step;
        }
        actions
    }

    fn intervals_after(&self, settle: f64) -> Vec<f64> {
        let times: Vec<f64> = self
            .presents
            .iter()
            .filter(|(t, _)| *t >= settle)
            .map(|(t, _)| *t)
            .collect();
        times.windows(2).map(|w| w[1] - w[0]).collect()
    }
}

fn mean_and_max_dev(values: &[f64]) -> (f64, f64) {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let max_dev = values.iter().map(|v| (v - mean).abs()).fold(0.0, f64::max);
    (mean, max_dev)
}

#[test]
fn disabled_by_default_does_nothing() {
    let mut h = Harness::new(false, |_| Behaviour::Interpolate);
    let actions = h.run(30.0, 1, 0.5, 0.0);
    assert!(actions.iter().all(|a| *a == FrameAction::RedrawNow));
    assert_eq!(*h.factory_calls.lock().unwrap(), 0);
    assert_eq!(h.log.lock().unwrap().prepared, 0);
    assert_eq!(h.controller.state(), &FgState::Disabled);
}

#[test]
fn paces_30_fps_into_even_60_fps_output() {
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    h.run(30.0, 1, 4.0, 0.0);
    assert_eq!(h.controller.state(), &FgState::Active);
    let intervals = h.intervals_after(1.0);
    let (mean, max_dev) = mean_and_max_dev(&intervals);
    assert!((mean - 1.0 / 60.0).abs() < 0.0003, "mean {mean}");
    assert!(max_dev < 0.001, "max deviation {max_dev}");
    // Strict alternation: generated, original, generated, original...
    let kinds: Vec<_> = h
        .presents
        .iter()
        .filter(|(t, _)| *t >= 1.0)
        .map(|(_, k)| *k)
        .collect();
    assert!(kinds.windows(2).all(|w| w[0] != w[1]), "not alternating");
    let d = h.controller.diagnostics(h.at(4.0));
    assert!(d.generated_total > 100);
    assert_eq!(d.failures, 0);
    assert_eq!(d.dropped_no_slot, 0);
    assert!(
        d.added_latency_ms > 16.0 && d.added_latency_ms < 34.0,
        "{}",
        d.added_latency_ms
    );
}

#[test]
fn paces_29_97_fps_into_59_94_fps_output() {
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    h.run(30_000.0 / 1001.0, 1, 6.0, 0.0);
    let intervals = h.intervals_after(1.5);
    let (mean, max_dev) = mean_and_max_dev(&intervals);
    assert!((mean - 1001.0 / 60_000.0).abs() < 0.0003, "mean {mean}");
    assert!(max_dev < 0.001, "max deviation {max_dev}");
}

#[test]
fn duplicated_60hz_capture_still_yields_even_60_fps() {
    // 30 FPS game, captured at 60 FPS: every frame arrives twice.
    let mut h = Harness::new(true, |seq| {
        if seq % 2 == 0 {
            Behaviour::Duplicate
        } else {
            Behaviour::Interpolate
        }
    });
    h.run(30.0, 2, 4.0, 0.0);
    let d = h.controller.diagnostics(h.at(4.0));
    assert!(d.duplicates > 100, "duplicates {}", d.duplicates);
    let intervals = h.intervals_after(1.0);
    let (mean, max_dev) = mean_and_max_dev(&intervals);
    assert!((mean - 1.0 / 60.0).abs() < 0.0003, "mean {mean}");
    assert!(max_dev < 0.001, "max deviation {max_dev}");
}

#[test]
fn slow_interpolation_drops_generated_frames_but_keeps_originals() {
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    h.shared.lock().unwrap().processing = Duration::from_millis(30);
    h.run(30.0, 1, 3.0, 0.0);
    let d = h.controller.diagnostics(h.at(3.0));
    // Originals keep flowing at the source rate.
    let originals = h
        .presents
        .iter()
        .filter(|(t, k)| *t > 1.0 && *k == ItemKindView::Original)
        .count();
    assert!(originals >= 55, "originals {originals}");
    assert_eq!(d.failures, 0);
}

#[test]
fn failures_fall_back_to_originals_then_disable() {
    let mut h = Harness::new(true, |seq| {
        if seq > 40 {
            Behaviour::Fail
        } else {
            Behaviour::Interpolate
        }
    });
    let actions = h.run(30.0, 1, 3.0, 0.0);
    match h.controller.state() {
        FgState::Failed(reason) => assert!(reason.contains("mock failure"), "{reason}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    // After giving up, frames are drawn natively and resources are freed.
    assert_eq!(*actions.last().unwrap(), FrameAction::RedrawNow);
    assert_eq!(h.log.lock().unwrap().display, None);
    assert!(h.log.lock().unwrap().released >= 1);
    assert!(h.shared.lock().unwrap().shutdowns >= 1);
}

#[test]
fn init_failure_reports_reason_and_uses_native_path() {
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    h.shared.lock().unwrap().init = Err("NvOFFRUC.dll was not found".into());
    let actions = h.run(30.0, 1, 0.5, 0.0);
    assert!(actions.iter().all(|a| *a == FrameAction::RedrawNow));
    match h.controller.state() {
        FgState::Failed(reason) => assert!(reason.contains("NvOFFRUC.dll")),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn disable_and_reenable_releases_and_restarts() {
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    h.run(30.0, 1, 1.0, 0.0);
    assert_eq!(h.controller.state(), &FgState::Active);
    h.controller.configure(config(false), &mut h.renderer);
    assert_eq!(h.controller.state(), &FgState::Disabled);
    assert_eq!(h.shared.lock().unwrap().shutdowns, 1);
    assert!(h.log.lock().unwrap().released >= 1);
    assert_eq!(h.log.lock().unwrap().display, None);
    let off = h.run(30.0, 1, 0.5, 1.0);
    assert!(off.iter().all(|a| *a == FrameAction::RedrawNow));

    h.controller.configure(config(true), &mut h.renderer);
    h.presents.clear();
    h.run(30.0, 1, 2.0, 1.5);
    assert_eq!(h.controller.state(), &FgState::Active);
    assert_eq!(*h.factory_calls.lock().unwrap(), 2);
    let intervals = h.intervals_after(2.5);
    let (mean, _) = mean_and_max_dev(&intervals);
    assert!((mean - 1.0 / 60.0).abs() < 0.0005, "mean {mean}");
}

#[test]
fn resolution_change_restarts_backend() {
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    h.run(30.0, 1, 1.0, 0.0);
    h.renderer.size = Some((1920, 1080));
    h.run(30.0, 1, 1.0, 1.0);
    assert_eq!(*h.factory_calls.lock().unwrap(), 2);
    assert_eq!(h.shared.lock().unwrap().shutdowns, 1);
    assert_eq!(h.controller.state(), &FgState::Active);
}

#[test]
fn fast_sources_pause_for_60_fps_target_only() {
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    let actions = h.run(60.0, 1, 4.0, 0.0);
    assert!(h.controller.diagnostics(h.at(4.0)).paused);
    assert_eq!(*actions.last().unwrap(), FrameAction::RedrawNow);

    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    let mut cfg = config(true);
    cfg.target = FrameGenTarget::AlwaysDouble;
    h.controller.configure(cfg, &mut h.renderer);
    h.run(60.0, 1, 4.0, 0.0);
    let d = h.controller.diagnostics(h.at(4.0));
    assert!(!d.paused);
    assert!(d.generated_fps > 50.0, "generated {}", d.generated_fps);
}

#[test]
fn dropped_source_frame_resyncs_without_starving_slots() {
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    h.run(30.0, 1, 2.0, 0.0);
    // Card drops ~3 frames, then continues.
    h.run(30.0, 1, 2.0, 2.1);
    let d = h.controller.diagnostics(h.at(4.1));
    assert!(d.source_gaps >= 1 || d.dropped_no_slot == 0);
    assert_eq!(d.dropped_no_slot, 0);
    let late = h.intervals_after(2.6);
    let (mean, _) = mean_and_max_dev(&late);
    assert!((mean - 1.0 / 60.0).abs() < 0.0005, "mean {mean}");
}

#[test]
fn reference_is_previous_submission() {
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    h.run(30.0, 1, 1.0, 0.0);
    let captures = h.log.lock().unwrap().captures.clone();
    assert!(captures.len() > 10);
    for pair in captures.windows(2) {
        assert_eq!(pair[1].1, Some(pair[0].0));
    }
}

#[test]
fn fruc_never_receives_an_unowned_output_slot() {
    // Results slower than two source frames keep output slots busy.
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    h.shared.lock().unwrap().processing = Duration::from_millis(110);
    h.run(30.0, 1, 3.0, 0.0);
    let submitted = h.shared.lock().unwrap().fruc_outputs.clone();
    let d = h.controller.diagnostics(h.at(3.0));
    assert!(
        submitted.iter().any(|(fruc, _)| !fruc),
        "expected some classify-only jobs: {} submitted, dropped_no_slot {}, state {:?}",
        submitted.len(),
        d.dropped_no_slot,
        d.state
    );
    for (fruc, output) in submitted {
        if fruc {
            assert!(output < OUTPUT_SLOTS);
        }
    }
}

#[test]
fn occasional_duplicates_never_stall_the_picture() {
    // Regression: a 30.00 FPS card beating against a 29.97 FPS game yields a
    // duplicate now and then. Each one must be skipped and motion continue.
    let mut h = Harness::new(true, |seq| {
        if seq % 25 == 0 {
            Behaviour::Duplicate
        } else {
            Behaviour::Interpolate
        }
    });
    h.run(30.0, 1, 10.0, 0.0);
    let d = h.controller.diagnostics(h.at(10.0));
    assert!(d.duplicates >= 10, "duplicates {}", d.duplicates);
    assert!(d.unique_fps > 25.0, "unique {}", d.unique_fps);
    assert!(d.displayed_fps > 50.0, "displayed {}", d.displayed_fps);
    let last = h.presents.last().map(|(t, _)| *t).unwrap_or(0.0);
    assert!(last > 9.9, "last present at {last}");
}

#[test]
fn startup_stall_with_bunched_frames_still_generates() {
    // What happened on the RTX 5080: right after start-up the event loop
    // stalls and queued frames arrive bunched; afterwards a steady 30 FPS.
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    // t=0 starts the backend; the next two frames reach the controller only
    // 8 ms apart because the UI thread was busy.
    for t in [0.0, 0.058, 0.066, 0.1, 0.133] {
        let now = h.at(t);
        h.shared.lock().unwrap().now = now;
        h.controller.on_frame_uploaded(&mut h.renderer, now);
        h.controller.process_events(&mut h.renderer, now);
    }
    let now = h.at(0.2);
    h.shared.lock().unwrap().now = now;
    h.controller.process_events(&mut h.renderer, now);
    h.run(30.0, 1, 5.0, 0.2);
    let d = h.controller.diagnostics(h.at(5.2));
    assert!(d.generated_fps > 25.0, "generated {}", d.generated_fps);
    assert!(d.displayed_fps > 55.0, "displayed {}", d.displayed_fps);
    assert!(
        d.source_period_ms > 32.0 && d.source_period_ms < 34.5,
        "period {}",
        d.source_period_ms
    );
    let intervals = h.intervals_after(2.0);
    let (mean, max_dev) = mean_and_max_dev(&intervals);
    assert!((mean - 1.0 / 60.0).abs() < 0.0005, "mean {mean}");
    assert!(max_dev < 0.0015, "max deviation {max_dev}");
}

#[test]
fn scene_cuts_skip_the_generated_frame_but_keep_pacing() {
    let mut h = Harness::new(true, |seq| {
        if seq % 10 == 5 {
            Behaviour::BigChange
        } else {
            Behaviour::Interpolate
        }
    });
    h.run(30.0, 1, 6.0, 0.0);
    let d = h.controller.diagnostics(h.at(6.0));
    assert!(d.transition_skips >= 15, "skips {}", d.transition_skips);
    // 1 in 10 generated frames skipped: ~27 generated, ~57 displayed.
    assert!(
        d.generated_fps > 24.0 && d.generated_fps < 29.0,
        "generated {}",
        d.generated_fps
    );
    assert!(d.unique_fps > 29.0, "unique {}", d.unique_fps);
    assert_eq!(d.late_generated, 0);
}

#[test]
fn slow_fruc_still_shows_generated_frames() {
    // FRUC at ~20 ms (GPU clocked down) is longer than half a 30 FPS
    // frame; the pipeline delay must grow instead of dropping frames.
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    h.shared.lock().unwrap().processing = Duration::from_millis(20);
    h.run(30.0, 1, 6.0, 0.0);
    let d = h.controller.diagnostics(h.at(6.0));
    assert!(d.generated_fps > 28.0, "generated {}", d.generated_fps);
    assert!(d.late_generated <= 3, "late {}", d.late_generated);
    let (mean, max_dev) = mean_and_max_dev(&h.intervals_after(3.0));
    assert!((mean - 1.0 / 60.0).abs() < 0.0005, "mean {mean}");
    assert!(max_dev < 0.0015, "max deviation {max_dev}");
}

#[test]
fn fast_camera_motion_is_interpolated_not_skipped() {
    // Regression for the v4 "fast motion" whole-frame skip, which turned
    // every quick camera turn into 30 FPS. A pan that camera motion fully
    // explains must be interpolated (corrections are per pixel).
    let mut h = Harness::new(true, |seq| {
        if seq % 10 < 3 {
            Behaviour::FastPan
        } else {
            Behaviour::Interpolate
        }
    });
    h.run(30.0, 1, 6.0, 0.0);
    let d = h.controller.diagnostics(h.at(6.0));
    assert_eq!(d.transition_skips, 0);
    assert!(d.generated_fps > 28.0, "generated {}", d.generated_fps);
    assert!(d.displayed_fps > 57.0, "displayed {}", d.displayed_fps);
}

#[test]
fn transition_hysteresis_does_not_flicker() {
    // A menu wipe: a big unexplained change followed by smaller ones. Once
    // in a transition, half the threshold keeps it there, so skipped and
    // interpolated frames do not alternate.
    let mut c = Harness::new(true, |_| Behaviour::Interpolate).controller;
    let stats = |mean: f32| DiffStats {
        sum_fixed: (mean * 16.0 * 1000.0) as u32,
        changed: 900,
        samples: 1000,
        max_fixed: 255 * 16,
    };
    let cut = super::motion::MotionEstimate {
        shift: [0.0, 0.0],
        residual: 0.9,
    };
    let pan = super::motion::MotionEstimate {
        shift: [200.0, 0.0],
        residual: 0.05,
    };
    assert!(c.is_transition(Some(stats(60.0)), Some(cut)));
    assert!(
        c.is_transition(Some(stats(20.0)), Some(cut)),
        "held by hysteresis"
    );
    assert!(!c.is_transition(Some(stats(10.0)), Some(cut)), "released");
    assert!(
        !c.is_transition(Some(stats(20.0)), Some(cut)),
        "below entry threshold"
    );
    // Big difference fully explained by camera motion is not a cut.
    assert!(!c.is_transition(Some(stats(60.0)), Some(pan)));
}

#[test]
fn hitches_are_attributed_to_their_cause() {
    // Duplicate source frames every 15th frame: each one leaves a hole in
    // the 60 FPS cadence, and the summary must say so (not just "hitches").
    let mut h = Harness::new(true, |seq| {
        if seq % 15 == 0 {
            Behaviour::Duplicate
        } else {
            Behaviour::Interpolate
        }
    });
    h.run(30.0, 1, 6.0, 0.0);
    let d = h.controller.diagnostics(h.at(6.0));
    assert!(d.hitches > 0, "expected hitches from duplicates");
    let attributed: u64 = d
        .hitch_causes
        .iter()
        .filter(|(c, _)| *c == "source-duplicate" || *c == "source-gap")
        .map(|(_, n)| n)
        .sum();
    assert_eq!(attributed, d.hitches, "causes: {:?}", d.hitch_causes);
}

#[test]
fn clean_run_has_no_hitches() {
    let mut h = Harness::new(true, |_| Behaviour::Interpolate);
    h.run(30.0, 1, 6.0, 0.0);
    let d = h.controller.diagnostics(h.at(6.0));
    assert_eq!(d.hitches, 0, "causes: {:?}", d.hitch_causes);
    assert!(d.hitch_causes.is_empty());
}
