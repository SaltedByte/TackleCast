use std::time::{Duration, Instant};

/// A short, bounded recovery burst. A stable session earns a fresh budget;
/// repeated failures require an explicit retry instead of reopening forever.
#[derive(Default)]
pub struct RecoveryBackoff {
    attempts: usize,
    next_attempt: Option<Instant>,
    healthy_since: Option<Instant>,
}

impl RecoveryBackoff {
    const DELAYS: [Duration; 3] = [
        Duration::from_secs(3),
        Duration::from_secs(6),
        Duration::from_secs(12),
    ];

    pub fn failed(&mut self, now: Instant) {
        self.healthy_since = None;
        if self.next_attempt.is_none() {
            self.next_attempt = Self::DELAYS.get(self.attempts).map(|delay| now + *delay);
        }
    }

    pub fn take_ready(&mut self, now: Instant) -> bool {
        if self.next_attempt.is_some_and(|at| now >= at) {
            self.next_attempt = None;
            self.attempts += 1;
            return true;
        }
        false
    }

    pub fn healthy(&mut self, now: Instant) {
        self.next_attempt = None;
        let since = self.healthy_since.get_or_insert(now);
        if now.duration_since(*since) >= Duration::from_secs(60) {
            self.attempts = 0;
        }
    }

    pub fn exhausted(&self) -> bool {
        self.attempts >= Self::DELAYS.len() && self.next_attempt.is_none()
    }

    pub fn attempts(&self) -> usize {
        self.attempts
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_are_delayed_bounded_and_repeated_polling_does_not_postpone_them() {
        let now = Instant::now();
        let mut recovery = RecoveryBackoff::default();
        let mut elapsed = Duration::ZERO;
        for delay in RecoveryBackoff::DELAYS {
            recovery.failed(now + elapsed);
            recovery.failed(now + elapsed + Duration::from_secs(1));
            assert!(!recovery.take_ready(now + elapsed + delay - Duration::from_millis(1)));
            elapsed += delay;
            assert!(recovery.take_ready(now + elapsed));
            assert!(!recovery.take_ready(now + elapsed));
        }
        recovery.failed(now + elapsed);
        assert!(recovery.exhausted());
        assert!(!recovery.take_ready(now + elapsed + Duration::from_secs(1000)));
    }

    #[test]
    fn only_sustained_health_or_manual_reset_restores_budget() {
        let now = Instant::now();
        let mut recovery = RecoveryBackoff::default();
        recovery.failed(now);
        assert!(recovery.take_ready(now + Duration::from_secs(3)));
        recovery.healthy(now + Duration::from_secs(4));
        recovery.failed(now + Duration::from_secs(5));
        assert!(!recovery.take_ready(now + Duration::from_secs(8)));
        assert!(recovery.take_ready(now + Duration::from_secs(11)));
        recovery.healthy(now + Duration::from_secs(12));
        recovery.healthy(now + Duration::from_secs(72));
        assert_eq!(recovery.attempts(), 0);
        recovery.failed(now + Duration::from_secs(73));
        recovery.reset();
        assert!(!recovery.take_ready(now + Duration::from_secs(100)));
        assert_eq!(recovery.attempts(), 0);
    }
}
