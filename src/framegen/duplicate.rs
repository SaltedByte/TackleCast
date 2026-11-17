//! Duplicate source-frame detection.
//!
//! A 30 FPS game on a 60 Hz HDMI signal repeats every frame; a capture card
//! running at 30.00 FPS against a 29.97 FPS game repeats one roughly every
//! 33 seconds. Interpolating between a frame and its copy produces no motion
//! and then a double-size jump: the periodic stutter. Duplicates are therefore
//! detected on the GPU *before* the interpolator sees them, and skipped.
//!
//! The GPU compares the new frame with the last unique frame on a sparse grid
//! (every 4th pixel in each direction) and reports, in 8-bit luma levels:
//! the summed absolute difference, how many samples changed by more than
//! [`CHANGED_LEVEL`], and the maximum difference. A frame is a duplicate only
//! when the mean difference is tiny *and* almost no samples changed, so slow
//! camera pans (small mean, many changed samples) still count as motion.

use bytemuck::{Pod, Zeroable};

/// Grid stride in pixels for the comparison.
pub const SAMPLE_STRIDE: u32 = 4;
/// A sample "changed" when luma differs by more than this many 8-bit levels.
/// Mirrors the shader constant (a test keeps them identical).
#[cfg_attr(not(test), allow(dead_code))]
pub const CHANGED_LEVEL: f32 = 6.0;
/// Fixed-point scale for the summed difference (1/16 level resolution).
pub const SUM_SCALE: f32 = 16.0;

/// Layout of the GPU result buffer (`DIFF_SHADER`'s `DiffResult`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct DiffStats {
    /// Sum over samples of |Δluma| * SUM_SCALE (8-bit levels).
    pub sum_fixed: u32,
    /// Samples whose |Δluma| exceeded CHANGED_LEVEL.
    pub changed: u32,
    /// Number of samples compared.
    pub samples: u32,
    /// Max |Δluma| * SUM_SCALE.
    pub max_fixed: u32,
}

impl DiffStats {
    pub fn mean_levels(&self) -> f32 {
        if self.samples == 0 {
            return 0.0;
        }
        self.sum_fixed as f32 / SUM_SCALE / self.samples as f32
    }

    pub fn changed_fraction(&self) -> f32 {
        if self.samples == 0 {
            return 0.0;
        }
        self.changed as f32 / self.samples as f32
    }

    pub fn max_levels(&self) -> f32 {
        self.max_fixed as f32 / SUM_SCALE
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DuplicateThresholds {
    /// Mean |Δluma| (8-bit levels) below which a frame may be a duplicate.
    pub mean_levels: f32,
    /// Fraction of changed samples below which a frame may be a duplicate.
    pub changed_fraction: f32,
    /// Largest |Δluma| (8-bit levels) any sample may show in a duplicate.
    /// A repeated HDMI frame is a bit-exact (or near-exact) copy; a frame
    /// with even one clearly changed spot (cursor, breathing character,
    /// distant motion) is new content and must be shown.
    pub max_levels: f32,
}

impl Default for DuplicateThresholds {
    fn default() -> Self {
        Self {
            mean_levels: 0.6,
            changed_fraction: 0.002,
            max_levels: 4.0,
        }
    }
}

impl DuplicateThresholds {
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        Self {
            mean_levels: if self.mean_levels.is_finite() {
                self.mean_levels.clamp(0.0, 8.0)
            } else {
                d.mean_levels
            },
            changed_fraction: if self.changed_fraction.is_finite() {
                self.changed_fraction.clamp(0.0, 0.1)
            } else {
                d.changed_fraction
            },
            max_levels: if self.max_levels.is_finite() {
                self.max_levels.clamp(0.0, 32.0)
            } else {
                d.max_levels
            },
        }
    }
}

pub fn is_duplicate(stats: &DiffStats, thresholds: &DuplicateThresholds) -> bool {
    if stats.samples == 0 {
        // Nothing was compared (no reference): never call it a duplicate.
        return false;
    }
    stats.mean_levels() <= thresholds.mean_levels
        && stats.changed_fraction() <= thresholds.changed_fraction
        && stats.max_levels() <= thresholds.max_levels
}

/// Compute shader comparing two RGBA8 frames on a sparse grid.
///
/// One invocation per grid point; each workgroup reduces in shared memory and
/// issues a single set of global atomics, so the cost is a few microseconds
/// at 1440p.
pub const DIFF_SHADER: &str = r#"
struct DiffResult {
    sum_fixed: atomic<u32>,
    changed: atomic<u32>,
    samples: atomic<u32>,
    max_fixed: atomic<u32>,
};

@group(0) @binding(0) var current_tex: texture_2d<f32>;
@group(0) @binding(1) var reference_tex: texture_2d<f32>;
@group(0) @binding(2) var<storage, read_write> result: DiffResult;

const STRIDE: u32 = 4u;
const CHANGED_LEVEL: f32 = 6.0;
const SUM_SCALE: f32 = 16.0;

var<workgroup> wg_sum: atomic<u32>;
var<workgroup> wg_changed: atomic<u32>;
var<workgroup> wg_samples: atomic<u32>;
var<workgroup> wg_max: atomic<u32>;

fn luma(c: vec4<f32>) -> f32 {
    return dot(c.rgb, vec3<f32>(0.299, 0.587, 0.114)) * 255.0;
}

@compute @workgroup_size(16, 16, 1)
fn cs_main(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_index) lidx: u32,
) {
    if lidx == 0u {
        atomicStore(&wg_sum, 0u);
        atomicStore(&wg_changed, 0u);
        atomicStore(&wg_samples, 0u);
        atomicStore(&wg_max, 0u);
    }
    workgroupBarrier();

    let dims = textureDimensions(current_tex);
    let pos = gid.xy * STRIDE + vec2<u32>(STRIDE / 2u);
    if pos.x < dims.x && pos.y < dims.y {
        let a = luma(textureLoad(current_tex, vec2<i32>(pos), 0));
        let b = luma(textureLoad(reference_tex, vec2<i32>(pos), 0));
        let d = abs(a - b);
        let fixed = u32(round(d * SUM_SCALE));
        atomicAdd(&wg_sum, fixed);
        atomicAdd(&wg_samples, 1u);
        atomicMax(&wg_max, fixed);
        if d > CHANGED_LEVEL {
            atomicAdd(&wg_changed, 1u);
        }
    }
    workgroupBarrier();

    if lidx == 0u {
        atomicAdd(&result.sum_fixed, atomicLoad(&wg_sum));
        atomicAdd(&result.changed, atomicLoad(&wg_changed));
        atomicAdd(&result.samples, atomicLoad(&wg_samples));
        atomicMax(&result.max_fixed, atomicLoad(&wg_max));
    }
}
"#;

/// Workgroup counts for the diff dispatch at a given frame size.
pub fn diff_dispatch_size(width: u32, height: u32) -> (u32, u32) {
    let grid_w = width.div_ceil(SAMPLE_STRIDE);
    let grid_h = height.div_ceil(SAMPLE_STRIDE);
    (grid_w.div_ceil(16), grid_h.div_ceil(16))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(mean: f32, changed_fraction: f32) -> DiffStats {
        stats_max(mean, changed_fraction, 0.0)
    }

    fn stats_max(mean: f32, changed_fraction: f32, max: f32) -> DiffStats {
        let samples = 230_400; // 1440p on a 4 px grid
        DiffStats {
            sum_fixed: (mean * SUM_SCALE * samples as f32) as u32,
            changed: (changed_fraction * samples as f32) as u32,
            samples,
            max_fixed: (max * SUM_SCALE) as u32,
        }
    }

    #[test]
    fn small_real_changes_are_not_duplicates() {
        // Regression (RDR2 at camp: 478 of 900 frames in 30 s were dropped
        // as "duplicates", freezing the picture for up to 232 ms): low-motion
        // frames with a little real change.
        let t = DuplicateThresholds::default();
        // A 40x40 px cursor moving in a menu: 100 changed samples.
        assert!(!is_duplicate(
            &stats_max(0.05, 100.0 / 230_400.0, 180.0),
            &t
        ));
        // Character breathing / grass: tiny mean, few changed samples.
        assert!(!is_duplicate(&stats_max(0.3, 0.001, 35.0), &t));
        // One blinking 8 px icon.
        assert!(!is_duplicate(&stats_max(0.001, 4.0 / 230_400.0, 120.0), &t));
    }

    #[test]
    fn old_settings_files_get_the_strict_check() {
        // Settings saved by earlier builds lack `max_levels`.
        let t: DuplicateThresholds =
            serde_json::from_str(r#"{"mean_levels":0.6,"changed_fraction":0.002}"#).unwrap();
        assert_eq!(t.max_levels, 4.0);
    }

    #[test]
    fn exact_copy_is_duplicate() {
        assert!(is_duplicate(
            &stats(0.0, 0.0),
            &DuplicateThresholds::default()
        ));
    }

    #[test]
    fn compression_noise_is_duplicate() {
        assert!(is_duplicate(
            &stats_max(0.4, 0.0, 3.0),
            &DuplicateThresholds::default()
        ));
    }

    #[test]
    fn slow_pan_is_not_duplicate() {
        // Small mean difference but many changed edges.
        assert!(!is_duplicate(
            &stats(0.5, 0.03),
            &DuplicateThresholds::default()
        ));
    }

    #[test]
    fn normal_motion_is_not_duplicate() {
        assert!(!is_duplicate(
            &stats(6.0, 0.2),
            &DuplicateThresholds::default()
        ));
    }

    #[test]
    fn no_samples_is_never_duplicate() {
        assert!(!is_duplicate(
            &DiffStats::default(),
            &DuplicateThresholds::default()
        ));
    }

    #[test]
    fn layout_matches_shader() {
        assert_eq!(std::mem::size_of::<DiffStats>(), 16);
    }

    #[test]
    fn shader_constants_match_rust() {
        assert!(DIFF_SHADER.contains(&format!("const STRIDE: u32 = {SAMPLE_STRIDE}u;")));
        assert!(DIFF_SHADER.contains(&format!("const CHANGED_LEVEL: f32 = {CHANGED_LEVEL:.1};")));
        assert!(DIFF_SHADER.contains(&format!("const SUM_SCALE: f32 = {SUM_SCALE:.1};")));
    }

    #[test]
    fn dispatch_covers_frame() {
        assert_eq!(diff_dispatch_size(2560, 1440), (40, 23));
        assert_eq!(diff_dispatch_size(1920, 1080), (30, 17));
    }

    #[test]
    fn thresholds_sanitize() {
        let t = DuplicateThresholds {
            mean_levels: f32::NAN,
            changed_fraction: 5.0,
            max_levels: f32::INFINITY,
        }
        .sanitized();
        assert_eq!(t.mean_levels, 0.6);
        assert_eq!(t.changed_fraction, 0.1);
    }
}
