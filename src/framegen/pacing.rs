//! Presentation pacing for 2x frame generation.
//!
//! Capture-card frames arrive with USB/driver jitter, and a 29.97 FPS source
//! drifts slowly against a nominal 30 FPS clock. Presenting each frame the
//! moment it arrives would copy that jitter straight onto the screen, and
//! interpolated frames would land at uneven offsets between originals.
//!
//! This module keeps a smoothed *source clock*: a second-order phase-locked
//! loop over unique-frame arrivals. Every unique frame `n` gets a smoothed
//! arrival time `phase_n`. With a pipeline delay `D` (long enough for the
//! interpolation result to be ready) the output timeline is
//!
//! ```text
//!   interpolated(n - 1/2)  at  phase_n + D
//!   original(n)            at  phase_n + D + T/2
//! ```
//!
//! so outputs are spaced by `T/2` regardless of arrival jitter. The added
//! latency for originals is `D + T/2` (roughly 20 ms at 30 FPS); this is the
//! inherent cost of interpolating between two real frames and is reported in
//! the diagnostics rather than hidden.
//!
//! Everything here is pure logic over `Instant`s, so it is unit-tested with
//! simulated 30 and 29.97 FPS sources, jitter, drops, and late results.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Shortest source period the pacer accepts (240 FPS).
pub const MIN_PERIOD: f64 = 1.0 / 240.0;
/// Longest source period the pacer accepts (10 FPS). Slower sources are
/// treated as stalls: interpolating over 100 ms gaps produces poor motion.
pub const MAX_PERIOD: f64 = 1.0 / 10.0;

const INTERVAL_WINDOW: usize = 32;
/// Phase correction gain. Small values reject jitter; large values follow
/// real timing changes faster.
const PHASE_GAIN: f64 = 0.10;
/// Frequency correction gain (second-order term), tracks 30 vs 29.97 drift.
const FREQUENCY_GAIN: f64 = 0.004;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrivalKind {
    /// Continuous with the previous unique frame: interpolation is valid.
    Continuous,
    /// First frame, a gap (dropped/stalled frames), or a large timing jump.
    /// The interpolator state must be refreshed without generating a frame.
    Resync,
}

#[derive(Debug, Clone, Copy)]
pub struct Arrival {
    pub kind: ArrivalKind,
    /// Smoothed arrival time of this frame on the source clock.
    pub phase: Instant,
    /// Current source period estimate.
    pub period: Duration,
}

#[derive(Debug, Clone)]
pub struct SourceClock {
    period: Option<f64>,
    intervals: VecDeque<f64>,
    last_arrival: Option<Instant>,
    phase: Option<Instant>,
    resyncs: u64,
    gaps: u64,
}

impl Default for SourceClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceClock {
    pub fn new() -> Self {
        Self {
            period: None,
            intervals: VecDeque::with_capacity(INTERVAL_WINDOW),
            last_arrival: None,
            phase: None,
            resyncs: 0,
            gaps: 0,
        }
    }

    /// Forget timing continuity (capture restart, minimise, toggle). The
    /// period estimate is kept because the source rate rarely changes.
    pub fn break_continuity(&mut self) {
        self.last_arrival = None;
        self.phase = None;
    }

    /// Full reset, including the period estimate (resolution/source change).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn period(&self) -> Option<Duration> {
        self.period.map(Duration::from_secs_f64)
    }

    /// Frames counted as gaps (an interval of roughly 2x or more).
    pub fn gaps(&self) -> u64 {
        self.gaps
    }

    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Feed the arrival time of a unique (non-duplicate) source frame.
    ///
    /// Only the first frame and real stalls (> [`MAX_PERIOD`]) break
    /// continuity. A missing or duplicated source frame (an interval of ~2T)
    /// is counted as a gap but stays continuous: FRUC interpolates from the
    /// two images it was given, so the in-between frame is still valid.
    ///
    /// The period is the median of recent intervals, seeded with the hint,
    /// so one outlier (for example frames bunched up behind a busy event
    /// loop) can never move it. Arrival-time jitter re-anchors the phase
    /// without discarding frames.
    pub fn on_unique_frame(&mut self, arrival: Instant, hint_period: Option<f64>) -> Arrival {
        if self.period.is_none() {
            let seed = hint_period
                .unwrap_or(1.0 / 30.0)
                .clamp(MIN_PERIOD, MAX_PERIOD);
            self.period = Some(seed);
            self.intervals.clear();
            for _ in 0..8 {
                self.intervals.push_back(seed);
            }
        }
        let period = self.period.expect("seeded above");
        let Some(last) = self.last_arrival else {
            self.last_arrival = Some(arrival);
            return self.resync(arrival);
        };
        self.last_arrival = Some(arrival);
        let interval = arrival.saturating_duration_since(last).as_secs_f64();

        // Too slow to interpolate meaningfully: a stall, not a frame rate.
        if interval > MAX_PERIOD {
            return self.resync(arrival);
        }

        // Learn the period from whole-frame intervals. A ~2T or ~3T interval
        // (missing frame) contributes interval/k; anything that is not near a
        // whole number of periods is jitter and only affects the phase.
        let steps = (interval / period).round().max(1.0);
        let per_step = interval / steps;
        if steps >= 2.0 {
            self.gaps += 1;
        }
        if (per_step - period).abs() < 0.35 * period && steps <= 3.0 {
            self.push_interval(per_step);
        } else if steps == 1.0 {
            // Sustained rate changes (30 -> 60 FPS menus) still get learnt:
            // they fill the window and move the median.
            self.push_interval(interval);
        }
        let median = self
            .median_interval()
            .unwrap_or(period)
            .clamp(MIN_PERIOD, MAX_PERIOD);

        let phase = self.phase.unwrap_or(arrival);
        let predicted = phase + Duration::from_secs_f64(period * steps);
        let error = signed_secs(arrival, predicted);
        let (new_phase, mut new_period) = if error.abs() > 0.5 * period {
            // Large timing jump: re-anchor on the real arrival.
            (arrival, median)
        } else {
            (
                offset(predicted, PHASE_GAIN * error),
                period + FREQUENCY_GAIN * error / steps,
            )
        };
        // Stay near the robust median; 3% covers 30 vs 29.97 and clock skew.
        new_period = new_period.clamp(median * 0.97, median * 1.03);
        self.period = Some(new_period);
        self.phase = Some(new_phase);
        Arrival {
            kind: ArrivalKind::Continuous,
            phase: new_phase,
            period: Duration::from_secs_f64(new_period),
        }
    }

    fn resync(&mut self, arrival: Instant) -> Arrival {
        self.resyncs += 1;
        self.phase = Some(arrival);
        Arrival {
            kind: ArrivalKind::Resync,
            phase: arrival,
            period: Duration::from_secs_f64(self.period.unwrap_or(1.0 / 30.0)),
        }
    }

    fn push_interval(&mut self, interval: f64) {
        if self.intervals.len() == INTERVAL_WINDOW {
            self.intervals.pop_front();
        }
        self.intervals.push_back(interval);
    }

    fn median_interval(&self) -> Option<f64> {
        if self.intervals.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.intervals.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(sorted[sorted.len() / 2])
    }
}

/// Signed `a - b` in seconds.
pub fn signed_secs(a: Instant, b: Instant) -> f64 {
    if a >= b {
        a.duration_since(b).as_secs_f64()
    } else {
        -b.duration_since(a).as_secs_f64()
    }
}

/// `t + seconds`, where `seconds` may be negative.
pub fn offset(t: Instant, seconds: f64) -> Instant {
    if seconds >= 0.0 {
        t + Duration::from_secs_f64(seconds)
    } else {
        t.checked_sub(Duration::from_secs_f64(-seconds))
            .unwrap_or(t)
    }
}

/// Adaptive pipeline delay `D`: how long after a frame's (smoothed) arrival
/// its interpolated predecessor is shown. It must cover the time to capture,
/// classify and interpolate, otherwise interpolated frames arrive late.
///
/// Rises quickly when results get slower (to stop late frames) and falls
/// slowly (so one fast frame does not shift the whole timeline).
#[derive(Debug, Clone)]
pub struct PipelineDelay {
    current: f64,
    samples: VecDeque<f64>,
    margin: f64,
    min: f64,
}

impl PipelineDelay {
    pub fn new(initial: Duration) -> Self {
        Self {
            current: initial.as_secs_f64(),
            samples: VecDeque::with_capacity(64),
            margin: 0.0015,
            min: 0.002,
        }
    }

    pub fn current(&self) -> Duration {
        Duration::from_secs_f64(self.current)
    }

    /// Record how long a result took after its source frame's arrival.
    pub fn observe(&mut self, latency: Duration, period: Duration) {
        if self.samples.len() == 64 {
            self.samples.pop_front();
        }
        self.samples.push_back(latency.as_secs_f64());
        let mut sorted: Vec<f64> = self.samples.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p90 = sorted[((sorted.len() - 1) as f64 * 0.9).round() as usize];
        // Generated frames show at phase + D and originals at
        // phase + D + T/2, so any D keeps them evenly spaced; the cap only
        // bounds the added latency (FRUC can take > T/2 when the GPU is
        // clocked down).
        let max = (period.as_secs_f64() * 0.9).max(self.min);
        let target = (p90 + self.margin).clamp(self.min, max);
        if target > self.current {
            // Move up fast: three quarters of the way per sample.
            self.current += (target - self.current) * 0.75;
        } else {
            // Move down slowly: about 0.05 ms per frame.
            self.current = (self.current - 0.00005).max(target);
        }
        self.current = self.current.clamp(self.min, max);
    }
}

/// Items waiting for their presentation time.
#[derive(Debug)]
pub struct PresentQueue<T> {
    items: VecDeque<(Instant, T)>,
}

impl<T> Default for PresentQueue<T> {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
        }
    }
}

/// Result of a queue poll: the item to show now (the newest due item) and
/// any older due items that were superseded and never displayed.
pub struct Due<T> {
    pub show: Option<T>,
    /// Scheduled time of `show`.
    pub show_at: Option<Instant>,
    pub skipped: Vec<T>,
}

impl<T> PresentQueue<T> {
    /// Insert keeping presentation order. Items normally arrive in order;
    /// an out-of-order item (late interpolation result) is placed correctly.
    pub fn push(&mut self, at: Instant, item: T) {
        let index = self
            .items
            .iter()
            .position(|(time, _)| *time > at)
            .unwrap_or(self.items.len());
        self.items.insert(index, (at, item));
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.items.front().map(|(time, _)| *time)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Pop everything due by `now + tolerance`.
    pub fn take_due(&mut self, now: Instant, tolerance: Duration) -> Due<T> {
        let limit = now + tolerance;
        let mut due = Vec::new();
        while self.items.front().is_some_and(|(time, _)| *time <= limit) {
            due.push(self.items.pop_front().expect("front exists"));
        }
        let last = due.pop();
        Due {
            show_at: last.as_ref().map(|(t, _)| *t),
            show: last.map(|(_, item)| item),
            skipped: due.into_iter().map(|(_, item)| item).collect(),
        }
    }

    /// Remove all pending items (returned so their resources can be freed).
    pub fn drain(&mut self) -> Vec<T> {
        self.items.drain(..).map(|(_, item)| item).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random jitter in [-amp, amp] seconds.
    fn jitter(seed: &mut u64, amp: f64) -> f64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        let unit = (*seed % 10_000) as f64 / 10_000.0;
        (unit * 2.0 - 1.0) * amp
    }

    /// Simulate a source at `fps` with arrival jitter and return the phases.
    fn simulate(fps: f64, frames: usize, jitter_amp: f64) -> Vec<Arrival> {
        let start = Instant::now();
        let mut clock = SourceClock::new();
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        (0..frames)
            .map(|n| {
                let ideal = n as f64 / fps;
                let t = offset(start, ideal + 0.002 + jitter(&mut seed, jitter_amp));
                clock.on_unique_frame(t, None)
            })
            .collect()
    }

    fn output_intervals(arrivals: &[Arrival], delay: f64) -> Vec<f64> {
        // Build the interleaved output timeline for continuous frames.
        let mut times = Vec::new();
        for a in arrivals {
            let half = a.period.as_secs_f64() * 0.5;
            if a.kind == ArrivalKind::Continuous {
                times.push(offset(a.phase, delay));
            }
            times.push(offset(a.phase, delay + half));
        }
        times.windows(2).map(|w| signed_secs(w[1], w[0])).collect()
    }

    fn stats(values: &[f64]) -> (f64, f64, f64) {
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
        let max_dev = values
            .iter()
            .map(|v| (v - mean).abs())
            .fold(0.0_f64, f64::max);
        (mean, var.sqrt(), max_dev)
    }

    #[test]
    fn locks_to_30_fps_and_paces_outputs_evenly() {
        let arrivals = simulate(30.0, 900, 0.003);
        let settled = &arrivals[120..];
        assert!(settled.iter().all(|a| a.kind == ArrivalKind::Continuous));
        let period = settled.last().unwrap().period.as_secs_f64();
        assert!((period - 1.0 / 30.0).abs() < 0.0002, "period {period}");
        let intervals = output_intervals(settled, 0.004);
        let (mean, stddev, max_dev) = stats(&intervals);
        assert!((mean - 1.0 / 60.0).abs() < 0.0002, "mean {mean}");
        // ±3 ms arrival jitter must be reduced well below 1 ms of output jitter.
        assert!(stddev < 0.0008, "stddev {stddev}");
        assert!(max_dev < 0.0025, "max deviation {max_dev}");
        assert!(
            intervals.iter().all(|v| *v > 0.0),
            "timeline must be monotonic"
        );
    }

    #[test]
    fn tracks_29_97_fps_without_resyncs() {
        let fps = 30_000.0 / 1001.0;
        let arrivals = simulate(fps, 1800, 0.002);
        let settled = &arrivals[150..];
        assert!(settled.iter().all(|a| a.kind == ArrivalKind::Continuous));
        let period = settled.last().unwrap().period.as_secs_f64();
        assert!((period - 1.0 / fps).abs() < 0.0002, "period {period}");
        let intervals = output_intervals(settled, 0.004);
        let (mean, stddev, _) = stats(&intervals);
        assert!((mean - 0.5 / fps).abs() < 0.0002, "mean {mean}");
        assert!(stddev < 0.0008, "stddev {stddev}");
    }

    #[test]
    fn distinguishes_29_97_from_30_over_long_runs() {
        // 60 s of each: the phase must not drift away from real arrivals.
        for fps in [30.0, 30_000.0 / 1001.0] {
            let start = Instant::now();
            let mut clock = SourceClock::new();
            let mut worst = 0.0_f64;
            for n in 0..1800 {
                let t = offset(start, n as f64 / fps);
                let a = clock.on_unique_frame(t, Some(1.0 / 30.0));
                if n > 200 {
                    worst = worst.max(signed_secs(a.phase, t).abs());
                }
            }
            assert!(worst < 0.0005, "fps {fps}: phase error {worst}");
        }
    }

    #[test]
    fn dropped_frame_is_a_gap_but_stays_continuous() {
        let start = Instant::now();
        let mut clock = SourceClock::new();
        let mut kinds = Vec::new();
        for n in 0..120 {
            if n == 60 {
                continue; // dropped by the card / duplicate removed
            }
            let t = offset(start, n as f64 / 30.0);
            kinds.push(clock.on_unique_frame(t, None).kind);
        }
        assert_eq!(kinds[0], ArrivalKind::Resync);
        assert!(kinds[1..].iter().all(|k| *k == ArrivalKind::Continuous));
        assert_eq!(clock.gaps(), 1);
        let period = clock.period().unwrap().as_secs_f64();
        assert!((period - 1.0 / 30.0).abs() < 0.0003, "period {period}");
    }

    #[test]
    fn bunched_frames_cannot_collapse_the_period() {
        // Regression: after a ~150 ms event-loop stall, queued frames arrive
        // bunched (58 ms, 8 ms, ...). One short interval used to become the
        // period, after which every normal frame looked like a gap.
        let start = Instant::now();
        let mut clock = SourceClock::new();
        let mut t = 0.0;
        let mut arrivals = Vec::new();
        for interval in [0.0, 0.150, 0.008, 0.058, 0.008, 0.033, 0.012, 0.054] {
            t += interval;
            arrivals.push(clock.on_unique_frame(offset(start, t), Some(1.0 / 30.0)));
        }
        let mut continuous = 0;
        for _ in 0..300 {
            t += 1.0 / 30.0;
            if clock
                .on_unique_frame(offset(start, t), Some(1.0 / 30.0))
                .kind
                == ArrivalKind::Continuous
            {
                continuous += 1;
            }
        }
        assert_eq!(continuous, 300);
        let period = clock.period().unwrap().as_secs_f64();
        assert!((period - 1.0 / 30.0).abs() < 0.0005, "period {period}");
        let gaps_before = clock.gaps();
        for _ in 0..300 {
            t += 1.0 / 30.0;
            clock.on_unique_frame(offset(start, t), None);
        }
        assert_eq!(
            clock.gaps(),
            gaps_before,
            "steady 30 FPS must not count gaps"
        );
    }

    #[test]
    fn long_stall_resyncs() {
        let start = Instant::now();
        let mut clock = SourceClock::new();
        for n in 0..30 {
            clock.on_unique_frame(offset(start, n as f64 / 30.0), None);
        }
        let after = clock.on_unique_frame(offset(start, 3.0), None);
        assert_eq!(after.kind, ArrivalKind::Resync);
        let next = clock.on_unique_frame(offset(start, 3.0 + 1.0 / 30.0), None);
        assert_eq!(next.kind, ArrivalKind::Continuous);
    }

    #[test]
    fn learns_a_rate_change() {
        let start = Instant::now();
        let mut clock = SourceClock::new();
        let mut t = 0.0;
        for _ in 0..60 {
            clock.on_unique_frame(offset(start, t), None);
            t += 1.0 / 30.0;
        }
        for _ in 0..120 {
            clock.on_unique_frame(offset(start, t), None);
            t += 1.0 / 60.0;
        }
        let period = clock.period().unwrap().as_secs_f64();
        assert!((period - 1.0 / 60.0).abs() < 0.001, "period {period}");
    }

    #[test]
    fn pipeline_delay_rises_fast_and_falls_slowly() {
        let period = Duration::from_secs_f64(1.0 / 30.0);
        let mut delay = PipelineDelay::new(Duration::from_millis(4));
        for _ in 0..20 {
            delay.observe(Duration::from_millis(9), period);
        }
        let high = delay.current().as_secs_f64();
        assert!(high > 0.0100 && high < 0.0110, "high {high}");
        delay.observe(Duration::from_millis(2), period);
        assert!(delay.current().as_secs_f64() > high - 0.0002);
        // A slow GPU (FRUC ~20 ms) is covered: D may exceed T/2.
        for _ in 0..64 {
            delay.observe(Duration::from_millis(20), period);
        }
        let slow = delay.current().as_secs_f64();
        assert!(slow > 0.020 && slow < 0.023, "slow {slow}");
        // Capped below one source frame to bound the added latency.
        for _ in 0..64 {
            delay.observe(Duration::from_millis(80), period);
        }
        assert!(delay.current().as_secs_f64() <= period.as_secs_f64() * 0.9 + 1e-9);
    }

    #[test]
    fn present_queue_orders_and_skips_superseded() {
        let start = Instant::now();
        let mut queue = PresentQueue::default();
        queue.push(start + Duration::from_millis(20), "b");
        queue.push(start + Duration::from_millis(10), "a");
        queue.push(start + Duration::from_millis(30), "c");
        assert_eq!(
            queue.next_deadline(),
            Some(start + Duration::from_millis(10))
        );
        let due = queue.take_due(start + Duration::from_millis(5), Duration::ZERO);
        assert!(due.show.is_none());
        let due = queue.take_due(start + Duration::from_millis(25), Duration::ZERO);
        assert_eq!(due.show, Some("b"));
        assert_eq!(due.skipped, vec!["a"]);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.drain(), vec!["c"]);
    }
}
