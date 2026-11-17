//! Rolling statistics for frame-generation diagnostics.
//!
//! All collectors are bounded and allocation-free after warm-up, so they can
//! run every frame for hours without growing.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Events per second over a sliding one-second window.
#[derive(Debug, Clone)]
pub struct RateCounter {
    events: VecDeque<Instant>,
    window: Duration,
    total: u64,
}

impl Default for RateCounter {
    fn default() -> Self {
        Self::new(Duration::from_secs(1))
    }
}

impl RateCounter {
    pub fn new(window: Duration) -> Self {
        Self {
            events: VecDeque::with_capacity(512),
            window,
            total: 0,
        }
    }

    pub fn record(&mut self, now: Instant) {
        self.total += 1;
        self.events.push_back(now);
        self.trim(now);
    }

    pub fn rate(&mut self, now: Instant) -> f32 {
        self.trim(now);
        self.events.len() as f32 / self.window.as_secs_f32()
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    fn trim(&mut self, now: Instant) {
        while self
            .events
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > self.window)
        {
            self.events.pop_front();
        }
        // Hard bound in case of an event storm.
        while self.events.len() > 2048 {
            self.events.pop_front();
        }
    }
}

/// Summary of a set of durations, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct DurationSummary {
    pub count: usize,
    pub mean_ms: f32,
    pub p50_ms: f32,
    pub p95_ms: f32,
    pub p99_ms: f32,
    pub max_ms: f32,
    pub stddev_ms: f32,
}

/// Bounded ring of duration samples.
#[derive(Debug, Clone)]
pub struct DurationWindow {
    samples: VecDeque<f32>,
    capacity: usize,
}

impl DurationWindow {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn record(&mut self, value: Duration) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(value.as_secs_f32() * 1000.0);
    }

    pub fn summary(&self) -> DurationSummary {
        summarize(self.samples.iter().copied())
    }

    pub fn values(&self) -> impl Iterator<Item = f32> + '_ {
        self.samples.iter().copied()
    }
}

pub fn summarize(values: impl Iterator<Item = f32>) -> DurationSummary {
    let mut sorted: Vec<f32> = values.collect();
    if sorted.is_empty() {
        return DurationSummary::default();
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let count = sorted.len();
    let mean = sorted.iter().sum::<f32>() / count as f32;
    let var = sorted.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / count as f32;
    let pick = |q: f32| sorted[(((count - 1) as f32) * q).round() as usize];
    DurationSummary {
        count,
        mean_ms: mean,
        p50_ms: pick(0.50),
        p95_ms: pick(0.95),
        p99_ms: pick(0.99),
        max_ms: *sorted.last().expect("non-empty"),
        stddev_ms: var.sqrt(),
    }
}

/// Presentation-interval tracking: how evenly new images reach the screen.
#[derive(Debug, Clone)]
pub struct PresentTimer {
    last: Option<Instant>,
    intervals: DurationWindow,
    hitches: u64,
}

impl Default for PresentTimer {
    fn default() -> Self {
        Self {
            last: None,
            intervals: DurationWindow::new(600),
            hitches: 0,
        }
    }
}

impl PresentTimer {
    /// Record a present of a *new* image; `expected` is the ideal interval.
    /// Returns true when this present ended a hitch (interval > 1.5x).
    pub fn record(&mut self, now: Instant, expected: Option<Duration>) -> bool {
        let mut hitch = false;
        if let Some(last) = self.last {
            let interval = now.saturating_duration_since(last);
            // Long pauses (menu, source stall) are not pacing errors.
            if interval < Duration::from_millis(250) {
                self.intervals.record(interval);
                if let Some(expected) = expected {
                    if interval.as_secs_f64() > expected.as_secs_f64() * 1.5 {
                        self.hitches += 1;
                        hitch = true;
                    }
                }
            }
        }
        self.last = Some(now);
        hitch
    }

    pub fn break_continuity(&mut self) {
        self.last = None;
    }

    pub fn summary(&self) -> DurationSummary {
        self.intervals.summary()
    }

    pub fn hitches(&self) -> u64 {
        self.hitches
    }

    pub fn intervals(&self) -> &DurationWindow {
        &self.intervals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_counter_counts_last_second() {
        let start = Instant::now();
        let mut rate = RateCounter::default();
        for i in 0..90 {
            rate.record(start + Duration::from_secs_f64(i as f64 / 60.0));
        }
        let now = start + Duration::from_secs_f64(89.0 / 60.0);
        let fps = rate.rate(now);
        assert!((59.0..=61.0).contains(&fps), "fps {fps}");
        assert_eq!(rate.total(), 90);
    }

    #[test]
    fn summary_percentiles() {
        let mut window = DurationWindow::new(100);
        for i in 1..=100 {
            window.record(Duration::from_millis(i));
        }
        let s = window.summary();
        assert_eq!(s.count, 100);
        assert!((s.mean_ms - 50.5).abs() < 0.01);
        assert!((s.p95_ms - 95.0).abs() <= 1.0);
        assert_eq!(s.max_ms, 100.0);
        // Ring is bounded.
        window.record(Duration::from_millis(1000));
        assert_eq!(window.summary().count, 100);
    }

    #[test]
    fn present_timer_flags_hitches_but_not_pauses() {
        let start = Instant::now();
        let mut timer = PresentTimer::default();
        let expected = Some(Duration::from_secs_f64(1.0 / 60.0));
        let mut t = start;
        for _ in 0..10 {
            timer.record(t, expected);
            t += Duration::from_secs_f64(1.0 / 60.0);
        }
        t += Duration::from_millis(17); // one doubled frame
        timer.record(t, expected);
        t += Duration::from_secs(2); // pause, not a hitch
        timer.record(t, expected);
        assert_eq!(timer.hitches(), 1);
        assert_eq!(timer.summary().count, 10);
    }
}
