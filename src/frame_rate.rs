use std::fmt::{Display, Formatter};
use std::str::FromStr;

/// A nominal capture rate. Keep the ratio through device negotiation; measured
/// delivery FPS is a separate statistic and must never overwrite this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FrameRate {
    pub numerator: u32,
    pub denominator: u32,
}

impl FrameRate {
    pub const fn new(numerator: u32, denominator: u32) -> Self {
        assert!(numerator > 0 && denominator > 0);
        Self {
            numerator,
            denominator,
        }
    }

    pub fn as_f64(self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }

    pub fn interval_100ns(self) -> i64 {
        (10_000_000.0 / self.as_f64()).round() as i64
    }

    #[cfg(any(target_os = "windows", test))]
    pub fn from_interval_100ns(interval: i64) -> Option<Self> {
        if interval <= 0 {
            return None;
        }
        // DirectShow stores integer 100 ns durations. Recognize standard rates
        // within one duration tick, never by +/- 1 FPS.
        for rate in Self::common() {
            if (rate.interval_100ns() - interval).abs() <= 1 {
                return Some(rate);
            }
        }
        let denominator = u32::try_from(interval).ok()?;
        Some(Self::new(10_000_000, denominator).reduced())
    }

    pub fn equivalent(self, other: Self) -> bool {
        (self.interval_100ns() - other.interval_100ns()).abs() <= 1
    }

    /// Express standard rates canonically despite driver duration rounding.
    pub fn normalized(self) -> Self {
        Self::common()
            .into_iter()
            .find(|rate| rate.equivalent(self))
            .unwrap_or_else(|| self.reduced())
    }

    pub fn key(self) -> String {
        let rate = self.reduced();
        if rate.denominator == 1 {
            rate.numerator.to_string()
        } else {
            format!("{}/{}", rate.numerator, rate.denominator)
        }
    }

    pub fn common() -> Vec<Self> {
        let mut rates: Vec<_> = [
            24, 25, 30, 50, 60, 75, 90, 100, 120, 144, 165, 180, 200, 240,
        ]
        .into_iter()
        .map(Self::from)
        .collect();
        rates.extend(
            [24_000, 30_000, 60_000, 120_000, 240_000]
                .into_iter()
                .map(|n| Self::new(n, 1001)),
        );
        rates.sort_by(|a, b| a.as_f64().total_cmp(&b.as_f64()));
        rates
    }

    fn reduced(self) -> Self {
        let (mut a, mut b) = (self.numerator, self.denominator);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        Self::new(self.numerator / a, self.denominator / a)
    }
}

impl From<u32> for FrameRate {
    fn from(value: u32) -> Self {
        Self::new(value.max(1), 1)
    }
}

impl Display for FrameRate {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.numerator.is_multiple_of(self.denominator) {
            write!(f, "{}", self.numerator / self.denominator)
        } else {
            let text = format!("{:.3}", self.as_f64());
            f.write_str(text.trim_end_matches('0').trim_end_matches('.'))
        }
    }
}

impl FromStr for FrameRate {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some((n, d)) = value.split_once('/') {
            let n: u32 = n.parse().map_err(|_| "Invalid frame-rate numerator")?;
            let d: u32 = d.parse().map_err(|_| "Invalid frame-rate denominator")?;
            if n == 0 || d == 0 {
                return Err("Frame rate must be positive".into());
            }
            let rate = Self::new(n, d).reduced();
            if !(1.0..=240.0).contains(&rate.as_f64()) {
                return Err("Frame rate must be between 1 and 240".into());
            }
            return Ok(rate);
        }
        let rate: f64 = value.parse().map_err(|_| "Invalid frame rate")?;
        if !rate.is_finite() || !(1.0..=240.0).contains(&rate) {
            return Err("Frame rate must be between 1 and 240".into());
        }
        Ok(Self::new((rate * 1_000_000.0).round() as u32, 1_000_000).reduced())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fractional_modes_do_not_become_integer_requests() {
        let ntsc = FrameRate::from_interval_100ns(166_833).unwrap();
        assert_eq!(ntsc.key(), "60000/1001");
        assert!(!ntsc.equivalent(FrameRate::from(60)));
        assert_eq!(FrameRate::from_interval_100ns(166_667).unwrap().key(), "60");
        assert_eq!(FrameRate::new(10_000_000, 166_667).normalized().key(), "60");
        assert_eq!(ntsc.to_string(), "59.94");
        assert!("0/0".parse::<FrameRate>().is_err());
        assert!("NaN".parse::<FrameRate>().is_err());
    }
}
