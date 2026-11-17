//! Global camera-motion estimate from luma projections.
//!
//! The GPU reduces each frame to two 1-D profiles: the mean luma of every
//! column (horizontal profile) and of every row (vertical profile). A camera
//! pan or turn shifts the horizontal profile sideways and a tilt shifts the
//! vertical one, so the shift that best aligns the previous profile with the
//! current one estimates the screen-space motion in pixels per source frame.
//!
//! Frame generation uses it to skip interpolation when the picture moves
//! further than optical flow can follow (fast camera turns at 30 FPS), which
//! is when the leading edge of the image breaks up into blocks.

/// Fraction of the profile length searched in each direction.
const MAX_SHIFT_FRACTION: f32 = 0.25;
/// Coarse search works on profiles downsampled by this factor.
const COARSE: usize = 4;

/// Global motion between two frames.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MotionEstimate {
    /// Screen-space motion in pixels per source frame (content moved by
    /// `[dx, dy]`: `current(x) ≈ previous(x - d)`).
    pub shift: [f32; 2],
    /// How much of the profile change a pure translation leaves
    /// unexplained, 0 (perfect) .. ~1 (no better than unrelated images).
    /// High values mean a scene cut or full-screen content change rather
    /// than camera motion.
    pub residual: f32,
}

/// Shift `s` such that `current[x] ≈ previous[x - s]` (content moved by `s`
/// pixels towards larger x), or `None` when the profile has too little
/// structure to tell (flat sky, black screen).
#[cfg_attr(not(test), allow(dead_code))]
pub fn estimate_shift(previous: &[f32], current: &[f32]) -> Option<f32> {
    estimate_shift_residual(previous, current).map(|(s, _)| s)
}

/// Like [`estimate_shift`], also returning the normalised residual.
pub fn estimate_shift_residual(previous: &[f32], current: &[f32]) -> Option<(f32, f32)> {
    let n = previous.len().min(current.len());
    if n < 32 {
        return None;
    }
    let (prev, cur) = (&previous[..n], &current[..n]);
    // Not enough detail: any shift matches about equally well.
    let spread = |p: &[f32]| {
        let mean = p.iter().sum::<f32>() / p.len() as f32;
        p.iter().map(|v| (v - mean).abs()).sum::<f32>() / p.len() as f32
    };
    let (spread_prev, spread_cur) = (spread(prev), spread(cur));
    if spread_prev < 0.5 || spread_cur < 0.5 {
        return None;
    }

    let coarse_prev = downsample(prev, COARSE);
    let coarse_cur = downsample(cur, COARSE);
    let coarse_max = ((coarse_prev.len() as f32 * MAX_SHIFT_FRACTION) as i64).max(1);
    let (coarse, _) = best_shift(&coarse_prev, &coarse_cur, -coarse_max, coarse_max)?;

    let max = (n as f32 * MAX_SHIFT_FRACTION) as i64;
    let centre = coarse * COARSE as i64;
    let lo = (centre - COARSE as i64 * 2).max(-max);
    let hi = (centre + COARSE as i64 * 2).min(max);
    let (shift, sad) = best_shift(prev, cur, lo, hi)?;
    // Two unrelated profiles differ by about the sum of their spreads.
    let residual = (sad / (spread_prev + spread_cur)).min(1.5);
    Some((shift as f32, residual))
}

/// Camera motion in pixels per source frame from the two profile pairs.
pub fn estimate_motion(
    prev_x: &[f32],
    cur_x: &[f32],
    prev_y: &[f32],
    cur_y: &[f32],
) -> Option<MotionEstimate> {
    let x = estimate_shift_residual(prev_x, cur_x);
    let y = estimate_shift_residual(prev_y, cur_y);
    if x.is_none() && y.is_none() {
        return None;
    }
    let residual = x
        .map(|v| v.1)
        .unwrap_or(0.0)
        .max(y.map(|v| v.1).unwrap_or(0.0));
    Some(MotionEstimate {
        shift: [x.map(|v| v.0).unwrap_or(0.0), y.map(|v| v.0).unwrap_or(0.0)],
        residual,
    })
}

fn downsample(p: &[f32], factor: usize) -> Vec<f32> {
    p.chunks(factor)
        .map(|c| c.iter().sum::<f32>() / c.len() as f32)
        .collect()
}

/// Shift in `lo..=hi` with the smallest mean absolute difference between
/// the mean-removed profiles over their overlap (at least half the length).
fn best_shift(prev: &[f32], cur: &[f32], lo: i64, hi: i64) -> Option<(i64, f32)> {
    let n = prev.len() as i64;
    let mut best: Option<(f32, i64)> = None;
    for s in lo..=hi {
        // cur[x] vs prev[x - s] for x in [max(0, s), min(n, n + s)).
        let start = s.max(0);
        let end = (n + s).min(n);
        let overlap = end - start;
        if overlap < n / 2 {
            continue;
        }
        let range = start as usize..end as usize;
        let shifted = (start - s) as usize..(end - s) as usize;
        let mean_c = cur[range.clone()].iter().sum::<f32>() / overlap as f32;
        let mean_p = prev[shifted.clone()].iter().sum::<f32>() / overlap as f32;
        let sad = cur[range]
            .iter()
            .zip(&prev[shifted])
            .map(|(c, p)| ((c - mean_c) - (p - mean_p)).abs())
            .sum::<f32>()
            / overlap as f32;
        // Prefer the smaller shift on ties (static or ambiguous content).
        let better = match best {
            None => true,
            Some((b, bs)) => sad < b - 1e-4 || ((sad - b).abs() <= 1e-4 && s.abs() < bs.abs()),
        };
        if better {
            best = Some((sad, s));
        }
    }
    best.map(|(sad, s)| (s, sad))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A detailed 1-D "landscape" sampled at `x + offset`.
    fn landscape(len: usize, offset: f32) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let x = i as f32 - offset;
                100.0
                    + 40.0 * (x * 0.013).sin()
                    + 25.0 * (x * 0.071).sin()
                    + 12.0 * (x * 0.23).cos()
                    + 6.0 * (x * 0.61).sin()
            })
            .collect()
    }

    #[test]
    fn finds_pans_in_both_directions() {
        for shift in [-300.0, -97.0, -8.0, 0.0, 5.0, 64.0, 151.0, 420.0] {
            let prev = landscape(2560, 0.0);
            let cur = landscape(2560, shift);
            let got = estimate_shift(&prev, &cur).unwrap();
            assert!((got - shift).abs() <= 1.0, "shift {shift} estimated {got}");
        }
    }

    #[test]
    fn tolerates_exposure_change_and_noise() {
        let prev = landscape(1440, 0.0);
        let cur: Vec<f32> = landscape(1440, 40.0)
            .iter()
            .enumerate()
            .map(|(i, v)| v + 12.0 + ((i * 7919 % 13) as f32 - 6.0) * 0.4)
            .collect();
        let got = estimate_shift(&prev, &cur).unwrap();
        assert!((got - 40.0).abs() <= 1.0, "estimated {got}");
    }

    #[test]
    fn flat_profiles_give_no_estimate() {
        let flat = vec![128.0; 2560];
        assert_eq!(estimate_shift(&flat, &flat), None);
        assert_eq!(estimate_motion(&flat, &flat, &flat, &flat), None);
    }

    #[test]
    fn motion_combines_axes() {
        let m = estimate_motion(
            &landscape(2560, 0.0),
            &landscape(2560, -120.0),
            &landscape(1440, 0.0),
            &landscape(1440, 30.0),
        )
        .unwrap();
        let s = m.shift;
        assert!(
            (s[0] + 120.0).abs() <= 1.0 && (s[1] - 30.0).abs() <= 1.0,
            "{m:?}"
        );
        assert!(m.residual < 0.1, "{m:?}");
    }

    #[test]
    fn scene_cut_has_high_residual_and_pan_does_not() {
        let a = landscape(2560, 0.0);
        let pan = landscape(2560, 80.0);
        // A different "scene": unrelated frequencies.
        let cut: Vec<f32> = (0..2560)
            .map(|i| 90.0 + 50.0 * (i as f32 * 0.031 + 1.7).cos() + 20.0 * (i as f32 * 0.17).sin())
            .collect();
        let (_, pan_res) = estimate_shift_residual(&a, &pan).unwrap();
        let (_, cut_res) = estimate_shift_residual(&a, &cut).unwrap();
        assert!(pan_res < 0.1, "pan residual {pan_res}");
        assert!(cut_res > 0.35, "cut residual {cut_res}");
    }
}
