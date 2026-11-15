//! Opt-in issue #25 telemetry. Callback methods do fixed atomic bookkeeping only.
//! All timestamps are host Instant proxies; none measure HDMI or speaker/display time.
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::audio::common::AudioRingBuffer;

static ENABLED: OnceLock<bool> = OnceLock::new();
static MONITOR: OnceLock<Arc<Monitor>> = OnceLock::new();
const PERIOD: Duration = Duration::from_secs(2);

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var("TACKLECAST_DIAGNOSTICS").as_deref() == Ok("1"))
}

struct Monitor {
    origin: Instant,
    next_id: AtomicU64,
    video: [Marker; 4],
    tx: mpsc::Sender<Command>,
}

enum Command {
    Start(Arc<AudioRingBuffer>),
    Stop(u64, mpsc::Sender<()>),
}

#[derive(Default)]
pub struct Marker {
    count: AtomicU64,
    last_us: AtomicU64,
    max_gap_us: AtomicU64,
}

impl Marker {
    // One writer per marker. No allocation, logging, locks, or retry loops.
    fn record(&self, now: u64) {
        let previous = self.last_us.swap(now, Relaxed);
        if previous != 0 {
            self.max_gap_us
                .fetch_max(now.saturating_sub(previous), Relaxed);
        }
        self.count.fetch_add(1, Relaxed);
    }

    fn summary(&self) -> (u64, u64, u64) {
        (
            self.count.load(Relaxed),
            self.last_us.load(Relaxed),
            self.max_gap_us.swap(0, Relaxed),
        )
    }
}

pub struct AudioTrace {
    pub id: u64,
    pub in_rate: u32,
    pub in_channels: u16,
    pub out_rate: u32,
    pub out_channels: u16,
    pub received: AtomicU64, // interleaved capture samples, before channel conversion
    offered: AtomicU64,      // interleaved playback samples, after resampling
    accepted: AtomicU64,
    requested: AtomicU64, // ring pop requests (sum across output callback chunks)
    real: AtomicU64,
    silence: AtomicU64,
    dropped: AtomicU64,
    shortfalls: AtomicU64, // any partial or empty ring pop
    empty_pops: AtomicU64, // v2.2.0 underrun semantics: empty pop only
    overruns: AtomicU64,   // partially/fully rejected ring pushes
    fill_min: AtomicU64,
    fill_max: AtomicU64,
    pub input: Marker,
    pub output: Marker,
    pub input_errors: AtomicU64,
    pub output_errors: AtomicU64,
    input_error_us: AtomicU64,
    output_error_us: AtomicU64,
}

impl AudioTrace {
    fn new(id: u64, in_rate: u32, in_channels: u16, out_rate: u32, out_channels: u16) -> Self {
        Self {
            id,
            in_rate,
            in_channels,
            out_rate,
            out_channels,
            received: AtomicU64::new(0),
            offered: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            requested: AtomicU64::new(0),
            real: AtomicU64::new(0),
            silence: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            shortfalls: AtomicU64::new(0),
            empty_pops: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            fill_min: AtomicU64::new(u64::MAX),
            fill_max: AtomicU64::new(0),
            input: Marker::default(),
            output: Marker::default(),
            input_errors: AtomicU64::new(0),
            output_errors: AtomicU64::new(0),
            input_error_us: AtomicU64::new(0),
            output_error_us: AtomicU64::new(0),
        }
    }

    pub fn capture(&self, samples: usize) {
        self.received.fetch_add(samples as u64, Relaxed);
        self.input.record(elapsed_us());
    }

    pub fn output_entry(&self) {
        self.output.record(elapsed_us());
    }

    pub fn stream_error(&self, input: bool) {
        let (count, time) = if input {
            (&self.input_errors, &self.input_error_us)
        } else {
            (&self.output_errors, &self.output_error_us)
        };
        count.fetch_add(1, Relaxed);
        time.store(elapsed_us(), Relaxed);
    }

    pub fn push(&self, offered: usize, accepted: usize, fill: usize) {
        self.offered.fetch_add(offered as u64, Relaxed);
        self.accepted.fetch_add(accepted as u64, Relaxed);
        self.dropped.fetch_add((offered - accepted) as u64, Relaxed);
        if accepted < offered {
            self.overruns.fetch_add(1, Relaxed);
        }
        self.fill(fill);
    }

    pub fn pop(&self, requested: usize, real: usize, fill: usize) {
        self.requested.fetch_add(requested as u64, Relaxed);
        self.real.fetch_add(real as u64, Relaxed);
        self.silence.fetch_add((requested - real) as u64, Relaxed);
        if real < requested {
            self.shortfalls.fetch_add(1, Relaxed);
            if real == 0 {
                self.empty_pops.fetch_add(1, Relaxed);
            }
        }
        self.fill(fill);
    }

    fn fill(&self, samples: usize) {
        self.fill_min.fetch_min(samples as u64, Relaxed);
        self.fill_max.fetch_max(samples as u64, Relaxed);
    }

    fn totals(&self) -> [u64; 11] {
        [
            self.received.load(Relaxed),
            self.offered.load(Relaxed),
            self.accepted.load(Relaxed),
            self.requested.load(Relaxed),
            self.real.load(Relaxed),
            self.silence.load(Relaxed),
            self.dropped.load(Relaxed),
            self.shortfalls.load(Relaxed),
            self.empty_pops.load(Relaxed),
            self.overruns.load(Relaxed),
            self.input_errors.load(Relaxed) + self.output_errors.load(Relaxed),
        ]
    }
}

pub fn init() {
    if !enabled() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let monitor = Arc::new(Monitor {
        origin: Instant::now(),
        next_id: AtomicU64::new(1),
        video: std::array::from_fn(|_| Marker::default()),
        tx,
    });
    if MONITOR.set(monitor.clone()).is_err() {
        return;
    }
    tracing::info!("drift startup build={} version={} platform={} period_ms=2000 clock=host_Instant_monotonic units=frames_per_channel; audio totals scoped to stream ID; video totals scoped to process; legacy logs suppressed for privacy",
        env!("TACKLECAST_BUILD_ID"), env!("CARGO_PKG_VERSION"), std::env::consts::OS);
    tracing::info!("drift limits: callback-entry, FFmpeg packet-return, decoded-publish, UI-upload and present-call are host-side proxies; no hardware timestamps, actual speaker playback, actual display presentation, content correlation or A/V offset; CPAL timestamps intentionally not mixed with host Instant; no PipeWire graph/hardware clock visibility");
    std::thread::Builder::new()
        .name("drift-summary".into())
        .spawn(move || worker(monitor, rx))
        .expect("diagnostic summary thread");
}

pub fn elapsed_us() -> u64 {
    MONITOR
        .get()
        .map_or(0, |m| m.origin.elapsed().as_micros() as u64)
}

// These events are called only from non-real-time control/capture/UI threads.
pub fn event(message: std::fmt::Arguments<'_>) {
    if MONITOR.get().is_some() {
        tracing::info!("drift event t_us={} {}", elapsed_us(), message);
    }
}

pub fn new_audio(
    in_rate: u32,
    in_channels: u16,
    out_rate: u32,
    out_channels: u16,
) -> Option<Arc<AudioTrace>> {
    MONITOR.get().map(|m| {
        Arc::new(AudioTrace::new(
            m.next_id.fetch_add(1, Relaxed),
            in_rate,
            in_channels,
            out_rate,
            out_channels,
        ))
    })
}

pub fn start_audio(ring: Arc<AudioRingBuffer>) {
    if let Some(m) = MONITOR.get() {
        let _ = m.tx.send(Command::Start(ring));
    }
}

pub fn stop_audio(id: u64) {
    if let Some(m) = MONITOR.get() {
        // Control thread only; finish the final snapshot before process exit.
        let (tx, rx) = mpsc::channel();
        if m.tx.send(Command::Stop(id, tx)).is_ok() {
            let _ = rx.recv();
        }
    }
}

// Stage IDs: 0 FFmpeg packet return, 1 decoded frame published, 2 UI upload,
// 3 wgpu SurfaceTexture::present returned. Independent counts, no frame pairing.
pub fn video(stage: usize) {
    if let Some(m) = MONITOR.get() {
        m.video[stage].record(elapsed_us());
    }
}

fn worker(m: Arc<Monitor>, rx: mpsc::Receiver<Command>) {
    let mut active: Option<Arc<AudioRingBuffer>> = None;
    let mut previous = [0; 11];
    let mut last_audio = Instant::now();
    let mut deadline = Instant::now() + PERIOD;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Command::Start(ring)) => {
                if let Some(old) = active.take() {
                    summarize(&old, &mut previous, &mut last_audio, "replaced");
                }
                previous = [0; 11];
                last_audio = Instant::now();
                active = Some(ring);
            }
            Ok(Command::Stop(id, done)) => {
                if active
                    .as_ref()
                    .and_then(|r| r.trace.as_ref())
                    .is_some_and(|t| t.id == id)
                {
                    summarize(
                        &active.take().unwrap(),
                        &mut previous,
                        &mut last_audio,
                        "final",
                    );
                }
                let _ = done.send(());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= deadline {
            if let Some(ring) = &active {
                summarize(ring, &mut previous, &mut last_audio, "interval");
            }
            let stages = m.video.each_ref().map(Marker::summary);
            tracing::info!("drift video t_us={} clock=host_Instant stages=(total_frames,last_host_us,interval_max_gap_us) packet_return={:?} decoded_publish={:?} ui_upload={:?} present_return={:?}", elapsed_us(), stages[0], stages[1], stages[2], stages[3]);
            deadline = Instant::now() + PERIOD;
        }
    }
}

fn frames(samples: u64, channels: u16) -> f64 {
    samples as f64 / channels as f64
}

fn summarize(ring: &AudioRingBuffer, previous: &mut [u64; 11], last: &mut Instant, kind: &str) {
    let Some(t) = &ring.trace else {
        return;
    };
    let totals = t.totals();
    let delta = std::array::from_fn::<_, 11, _>(|i| totals[i].saturating_sub(previous[i]));
    let seconds = last.elapsed().as_secs_f64().max(0.000001);
    let input_frames = |s| frames(s, t.in_channels);
    let output_frames = |s| frames(s, t.out_channels);
    // Separate relaxed atomic loads can straddle callbacks. Clamp only the
    // displayed derived quantities; never feed observations back into playback.
    let values = |v: [u64; 11]| {
        [
            input_frames(v[0]),
            output_frames(v[1]),
            output_frames(v[2]),
            output_frames(v[3]),
            output_frames(v[4]),
            output_frames(v[5]),
            output_frames(v[6]),
        ]
    };
    let fill = ring.diagnostic_fill() as u64;
    let min = t.fill_min.swap(u64::MAX, Relaxed).min(fill);
    let max = t.fill_max.swap(0, Relaxed).max(fill);
    tracing::info!("drift audio t_us={} id={} kind={} dt_s={:.3} capture_hz={} playback_hz={} frames_order=(capture_received,post_resample_offered,ring_accepted,output_requested,output_real,output_silence,ring_dropped) total={:?} interval={:?} interval_rates_fps=(capture:{:.2},post_resample:{:.2},requested:{:.2}) ring_fill_frames=(now:{:.1},min:{:.1},max:{:.1}) ring_fill_ms={:.2} events_order=(short_pop,empty_pop_legacy_underrun,rejected_push_overrun,stream_error) total_events={:?} interval_events={:?} callbacks=(count,last_host_us,interval_max_gap_us) input={:?} output={:?} errors_in_out=({},{}),last_error_host_us=({},{}); approximate_concurrent_snapshot",
        elapsed_us(), t.id, kind, seconds, t.in_rate, t.out_rate, values(totals), values(delta),
        input_frames(delta[0]) / seconds, output_frames(delta[1]) / seconds, output_frames(delta[3]) / seconds,
        output_frames(fill), output_frames(min), output_frames(max), output_frames(fill) * 1000.0 / t.out_rate as f64,
        &totals[7..], &delta[7..], t.input.summary(), t.output.summary(), t.input_errors.load(Relaxed), t.output_errors.load(Relaxed), t.input_error_us.load(Relaxed), t.output_error_us.load(Relaxed));
    *previous = totals;
    *last = Instant::now();
}

/// Hardware-free packaging check, explicitly requested on the CLI only. This
/// exercises the production ring, conversion and summary paths, not CPAL or a
/// physical capture device. Normal startup never enters this path.
pub fn audio_fixture() {
    use crate::audio::common::{AudioDiagnostics, AudioResampler};
    for (scenario, in_rate, out_rate) in [
        ("steady", 48000, 48000),
        ("rate_mismatch", 48000, 48000),
        ("forced_underrun", 48000, 48000),
        ("resampled", 44100, 48000),
    ] {
        let trace = new_audio(in_rate, 2, out_rate, 2).expect("diagnostics enabled");
        let id = trace.id;
        let ring = Arc::new(
            AudioRingBuffer::new(192000, Arc::new(AudioDiagnostics::new()))
                .with_trace(Some(trace.clone())),
        );
        event(format_args!("fixture scenario={} id={} backend=synthetic_no_CPAL input_hz={} input_channels=2 output_hz={} output_channels=2 format=F32 ring_capacity_playback_frames=96000; accelerated_host_schedule_not_device_rate", scenario, id, in_rate, out_rate));
        start_audio(ring.clone());
        // 200ms headroom; the mismatch adds 5 playback frames per 10ms block.
        let seed = vec![0.25; 9600 * 2];
        ring.push_samples(&seed);
        let input = vec![0.25; (in_rate / 100 * 2) as usize];
        let mut resampler = AudioResampler::new(in_rate, out_rate, 2);
        let mut converted = Vec::with_capacity(1024);
        let mut output = vec![
            0.0;
            if scenario == "rate_mismatch" {
                475 * 2
            } else {
                480 * 2
            }
        ];
        for tick in 0..400 {
            if !(scenario == "forced_underrun" && (100..170).contains(&tick)) {
                trace.capture(input.len());
                converted.clear();
                resampler.process(&input, &mut converted);
                ring.push_samples(&converted);
            }
            trace.output_entry();
            ring.pop_samples(&mut output);
            std::thread::sleep(Duration::from_millis(10));
        }
        stop_audio(id);
    }
    event(format_args!("fixture complete"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_and_empty_pops_account_for_silence_including_split_frames() {
        let t = AudioTrace::new(1, 44100, 2, 48000, 3);
        t.pop(8, 5, 0);
        t.pop(1, 0, 0);
        let v = t.totals();
        assert_eq!((v[3], v[4], v[5], v[7], v[8]), (9, 5, 4, 2, 1));
        assert_eq!(frames(v[3], 3), 3.0);
        assert_eq!(frames(v[4], 3) + frames(v[3] - v[4], 3), 3.0);
    }

    #[test]
    fn rejected_pushes_count_samples_and_events_separately() {
        let t = AudioTrace::new(1, 48000, 2, 48000, 2);
        t.push(10, 7, 7);
        t.push(4, 0, 7);
        let v = t.totals();
        assert_eq!((v[6], v[9]), (7, 2));
    }

    #[test]
    fn real_ring_telemetry_conserves_audio_across_overflow_and_partial_underrun() {
        use crate::audio::common::AudioDiagnostics;
        let t = Arc::new(AudioTrace::new(1, 44100, 2, 48000, 2));
        let ring =
            AudioRingBuffer::new(8, Arc::new(AudioDiagnostics::new())).with_trace(Some(t.clone()));
        let accepted = ring.push_samples(&[0.5; 10]);
        let mut out = [1.0; 12];
        let real = ring.pop_samples(&mut out);
        let v = t.totals();
        assert_eq!(accepted, real);
        assert_eq!(
            out,
            [0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.0, 0.0, 0.0, 0.0]
        );
        assert_eq!(v[1], v[2] + v[6]);
        assert_eq!(v[3], v[4] + v[5]);
        assert_eq!(v[2] - v[4], ring.available() as u64);
        assert_eq!((v[7], v[8], v[9]), (1, 0, 1)); // partial != legacy empty underrun
    }

    #[test]
    fn resampler_rate_and_channel_units_are_distinct_from_capture() {
        use crate::audio::common::{convert_channels, AudioDiagnostics, AudioResampler};
        let t = Arc::new(AudioTrace::new(1, 44100, 1, 48000, 2));
        let ring = AudioRingBuffer::new(20000, Arc::new(AudioDiagnostics::new()))
            .with_trace(Some(t.clone()));
        let input = vec![0.25; 4410];
        let mut stereo = Vec::new();
        let mut resampled = Vec::new();
        convert_channels(&input, 1, 2, &mut stereo);
        AudioResampler::new(44100, 48000, 2).process(&stereo, &mut resampled);
        t.capture(input.len());
        ring.push_samples(&resampled);
        let v = t.totals();
        assert_eq!(frames(v[0], 1), 4410.0);
        let output_frames = frames(v[1], 2);
        assert!((output_frames - 4800.0).abs() <= 3.0);
        assert!((frames(v[0], 1) / 44100.0 - output_frames / 48000.0).abs() < 0.0001);
        let mut output = vec![0.0; resampled.len()];
        ring.pop_samples(&mut output);
        assert_eq!(t.totals()[5], 0);
    }

    #[test]
    fn marker_gap_uses_monotonic_relative_host_time() {
        let m = Marker::default();
        m.record(100);
        m.record(120);
        m.record(200);
        assert_eq!(m.summary(), (3, 200, 80));
        assert_eq!(m.summary(), (3, 200, 0));
    }
}
