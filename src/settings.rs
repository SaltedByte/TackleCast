use std::fmt::{Display, Formatter};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::frame_rate::FrameRate;

pub const FPS_MODE_60: &str = "60";
#[allow(dead_code)]
pub const FPS_MODE_CUSTOM: &str = "custom";
pub const MIN_FPS: u32 = 1;
pub const MAX_FPS: u32 = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Vice,
    Classic,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// None means an older settings file; migrate detailed_overlay once.
    #[serde(default)]
    pub overlay: Option<crate::overlay::OverlayLayout>,
    /// None migrates the prior master switch without changing the custom layout.
    #[serde(default)]
    pub overlay_mode: Option<crate::overlay::Mode>,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub video_device: String,
    #[serde(default = "default_video_format")]
    pub video_format: VideoFormat,
    #[serde(default = "default_scaling_filter")]
    pub scaling_filter: ScaleFilter,
    #[serde(default = "default_color_space")]
    pub color_space: ColorSpace,
    #[serde(default = "default_color_range")]
    pub color_range: ColorRange,
    #[serde(default = "default_audio_index")]
    pub audio_input: i32,
    #[serde(default = "default_audio_index")]
    pub audio_output: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_input_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_output_name: Option<String>,
    #[serde(default = "default_resolution")]
    pub resolution: String,
    #[serde(default = "default_fps_mode")]
    pub fps_mode: String,
    #[serde(default = "default_custom_fps")]
    pub custom_fps: u32,
    #[serde(default = "default_volume")]
    pub volume: f64,
    #[serde(default)]
    pub audio_muted: bool,
    #[serde(default = "default_show_overlay")]
    pub show_overlay: bool,
    /// When set, the overlay also reports the active scaling filter, stacked
    /// over three lines. Off by default, keeping the overlay to one line.
    #[serde(default)]
    pub detailed_overlay: bool,
    #[serde(default = "default_brightness")]
    pub brightness: f32,
    #[serde(default = "default_contrast")]
    pub contrast: f32,
    #[serde(default = "default_saturation")]
    pub saturation: f32,
    #[serde(default = "default_gamma")]
    pub gamma: f32,
    #[serde(default = "default_sharpness")]
    pub sharpness: f32,
    /// Frame generation master switch. Off by default until enabled by hand.
    #[serde(default)]
    pub framegen_enabled: bool,
    #[serde(default)]
    pub framegen_backend: FrameGenBackend,
    #[serde(default, deserialize_with = "lenient_rife_model")]
    pub framegen_rife_model: RifeModel,
    #[serde(default)]
    pub framegen_mode: FrameGenMode,
    #[serde(default)]
    pub framegen_target: FrameGenTarget,
    /// Show the frame-generation diagnostics overlay.
    #[serde(default)]
    pub framegen_overlay: bool,
    /// Folder holding NvOFFRUC.dll (or the Optical Flow SDK root). Empty:
    /// search next to the executable and the default SDK location.
    #[serde(default)]
    pub framegen_runtime_dir: String,
    /// Require NVIDIA Authenticode signatures on the FRUC DLLs.
    #[serde(default = "default_true")]
    pub framegen_require_signature: bool,
    /// Duplicate-frame detection thresholds (advanced; JSON only).
    #[serde(default)]
    pub framegen_duplicates: crate::framegen::duplicate::DuplicateThresholds,
    /// Scene-cut detection (transition stabilisation): mean luma difference
    /// (8-bit levels) between consecutive frames that, together with a
    /// failed camera-motion match, marks a cut / full-screen change. JSON
    /// only; 0 disables cut detection.
    #[serde(default = "default_framegen_skip_difference")]
    pub framegen_skip_difference: f32,
    /// Artifact reduction applied to generated frames (Off = raw FRUC).
    #[serde(default)]
    pub framegen_artifact_mode: FrameGenArtifactMode,
    /// Dynamic: correct pixels that are static in both real frames (HUD,
    /// still scenery) but were warped by interpolation.
    #[serde(default = "default_true")]
    pub framegen_fix_static: bool,
    /// Dynamic: where the scenery follows the measured camera motion but the
    /// generated pixel disagrees (HUD ghosts, smearing), use a
    /// motion-compensated reconstruction from the two real frames.
    #[serde(default = "default_true")]
    pub framegen_fix_motion: bool,
    /// Dynamic: content entering the screen during camera motion is taken
    /// from the one real frame that contains it, shifted to the midpoint.
    #[serde(default = "default_true")]
    pub framegen_fix_edges: bool,
    /// Dynamic: in-place fades (UI fading in/out) and scene cuts.
    #[serde(default = "default_true")]
    pub framegen_fix_transitions: bool,
    /// HUD guard (any mode): crosshair, icons and text that stay put while
    /// the camera moves are kept out of interpolation and drawn from the
    /// real frame, so they do not flicker or leave dragged copies.
    #[serde(default = "default_true")]
    pub framegen_hud_guard: bool,
    /// Debug view of generated frames.
    #[serde(default)]
    pub framegen_debug_view: FrameGenDebugView,
    /// Optional manual regions always shown from the real frame (bit 0
    /// top-left, 1 top-right, 2 bottom-left, 3 bottom-right). Off by
    /// default: these regions update at the source rate.
    #[serde(default)]
    pub framegen_manual_zones: u32,
    #[serde(default = "default_framegen_zone_width")]
    pub framegen_manual_zone_width: f32,
    #[serde(default = "default_framegen_zone_height")]
    pub framegen_manual_zone_height: f32,
}

/// Frame-generation backend (interpolation engine).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FrameGenBackend {
    /// NVIDIA Optical Flow FRUC (NvOFFRUC.dll from the Optical Flow SDK,
    /// not redistributable, so the user supplies it).
    NvidiaFruc,
    /// RIFE neural interpolation on the GPU (ONNX Runtime + DirectML,
    /// shipped in the `rife` folder next to the executable).
    #[default]
    Rife,
}

impl Display for FrameGenBackend {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NvidiaFruc => f.write_str("NVIDIA FRUC (Optical Flow)"),
            Self::Rife => f.write_str("RIFE (DirectML)"),
        }
    }
}

/// RIFE network used by the RIFE backend (Practical-RIFE weights, MIT).
/// Each is an ONNX file `rife/models/<stem>_<W>x<H>.onnx` that outputs
/// motion + blend mask; the renderer builds the frame from the real frames.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum RifeModel {
    /// RIFE 4.26 run on the frames scaled down 2x. At 1440p the network
    /// misses thin objects (rifle barrel, hat brim) in fast camera turns;
    /// at half resolution it keeps them, and it is ~10 ms on an RTX 5080.
    #[default]
    #[serde(rename = "4.26_half")]
    V426Half,
    /// RIFE 4.6 at half resolution: fastest (~5 ms).
    #[serde(rename = "4.6_half")]
    V46Half,
    /// RIFE 4.6 at full resolution (~18 ms).
    #[serde(rename = "4.6_sharp")]
    V46Full,
}

impl RifeModel {
    pub const ALL: [Self; 3] = [Self::V426Half, Self::V46Half, Self::V46Full];

    /// File-name stem of the ONNX model.
    pub fn stem(self) -> &'static str {
        match self {
            Self::V426Half => "rife426fmh",
            Self::V46Half => "rife46fmh",
            Self::V46Full => "rife46fm",
        }
    }

    pub fn from_stem(stem: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.stem() == stem)
    }
}

impl Display for RifeModel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::V426Half => f.write_str("RIFE 4.26, half-res motion (recommended)"),
            Self::V46Half => f.write_str("RIFE 4.6, half-res motion (fastest)"),
            Self::V46Full => f.write_str("RIFE 4.6, full-res motion"),
        }
    }
}

/// Unknown model names (from older test builds) fall back to the default
/// instead of resetting every setting.
fn lenient_rife_model<'de, D: serde::Deserializer<'de>>(d: D) -> Result<RifeModel, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FrameGenMode {
    Off,
    /// One interpolated frame between every pair of unique source frames.
    #[default]
    #[serde(rename = "2x")]
    Double,
}

impl Display for FrameGenMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("Off"),
            Self::Double => f.write_str("2\u{00D7}"),
        }
    }
}

/// Artifact reduction for generated frames.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FrameGenArtifactMode {
    /// Raw NVIDIA FRUC output, full screen, no extra processing.
    #[default]
    Off,
    /// Content-aware correction of unreliable generated pixels.
    Dynamic,
    /// Dynamic, plus hiding detected static HUD from FRUC's input.
    Advanced,
}

impl Display for FrameGenArtifactMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("Off (engine output as is)"),
            Self::Dynamic => f.write_str("Dynamic"),
            Self::Advanced => f.write_str("Advanced (Dynamic + HUD hidden from FRUC)"),
        }
    }
}

/// What is shown in place of generated frames (debugging).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FrameGenDebugView {
    /// Final output (raw FRUC + corrections).
    #[default]
    Final,
    /// Raw FRUC output, ignoring corrections (they are still measured).
    RawFruc,
    /// Final output with corrected pixels tinted by correction type.
    CorrectionOverlay,
}

impl Display for FrameGenDebugView {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Final => f.write_str("Final output"),
            Self::RawFruc => f.write_str("Raw engine output"),
            Self::CorrectionOverlay => f.write_str("Correction overlay"),
        }
    }
}

/// What output rate frame generation aims for.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FrameGenTarget {
    /// 60 FPS: interpolate 30 / 29.97 FPS sources; pause automatically when
    /// the game itself already runs faster than ~45 FPS.
    #[default]
    #[serde(rename = "60")]
    Fps60,
    /// Always double the unique source rate.
    #[serde(rename = "double")]
    AlwaysDouble,
}

impl Display for FrameGenTarget {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fps60 => f.write_str("60 FPS (30 / 29.97 FPS sources)"),
            Self::AlwaysDouble => f.write_str("Always 2\u{00D7} source rate"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub fps: FrameRate,
    pub pixel_format: &'static str,
    pub decode_threads: usize,
}

/// Video / pixel format requested from the capture device.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum VideoFormat {
    #[default]
    Auto,
    #[serde(rename = "mjpeg", alias = "mjpg")]
    Mjpeg,
    #[serde(rename = "nv12")]
    Nv12,
    #[serde(rename = "yuy2", alias = "yuyv", alias = "yuyv422")]
    Yuy2,
    #[serde(rename = "uyvy", alias = "uyvy422")]
    Uyvy,
    #[serde(rename = "yuv420p", alias = "yuv12", alias = "yv12", alias = "i420")]
    Yuv420p,
}

impl VideoFormat {
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Nv12 => "nv12",
            Self::Mjpeg => "mjpeg",
            Self::Yuy2 => "yuyv422",
            Self::Uyvy => "uyvy422",
            Self::Yuv420p => "yuv420p",
        }
    }
    /// Every variant, in menu order.
    pub const ALL: [Self; 6] = [
        Self::Auto,
        Self::Mjpeg,
        Self::Nv12,
        Self::Yuy2,
        Self::Uyvy,
        Self::Yuv420p,
    ];
}

impl Display for VideoFormat {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("Auto"),
            Self::Mjpeg => f.write_str("MJPEG"),
            Self::Nv12 => f.write_str("NV12"),
            Self::Yuy2 => f.write_str("YUY2"),
            Self::Uyvy => f.write_str("UYVY"),
            Self::Yuv420p => f.write_str("YUV420P (I420)"),
        }
    }
}

/// Upscaling filter applied to the video planes in the fragment shader.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScaleFilter {
    Bilinear,
    Bicubic,
    Lanczos,
    Fsr1,
    #[serde(rename = "rtx_vsr", alias = "vsr")]
    RtxVsr,
}

impl ScaleFilter {
    /// The `filter_mode` value the shader branches on. Must stay in step with
    /// the `filter_mode` comparisons in `VIDEO_SHADER`.
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Bilinear => 0,
            Self::Bicubic => 1,
            Self::Lanczos => 2,
            Self::Fsr1 => 3,
            Self::RtxVsr => 4,
        }
    }

    /// Every variant, in menu order.
    pub const ALL: [Self; 5] = [
        Self::Bilinear,
        Self::Bicubic,
        Self::Lanczos,
        Self::Fsr1,
        Self::RtxVsr,
    ];
}

impl Display for ScaleFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bilinear => f.write_str("Bilinear"),
            Self::Bicubic => f.write_str("Bicubic"),
            Self::Lanczos => f.write_str("Lanczos"),
            Self::Fsr1 => f.write_str("FSR 1.0"),
            Self::RtxVsr => f.write_str("RTX VSR"),
        }
    }
}

/// Color space matrix applied in the fragment shader.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColorSpace {
    Auto,
    #[serde(rename = "rec709")]
    Rec709,
    #[serde(rename = "bt601")]
    Bt601,
    #[serde(rename = "bt2020")]
    Bt2020,
}

impl ColorSpace {
    /// The `color_space` uniform value the shader branches on.
    /// Must stay in step with `VIDEO_SHADER`.
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Rec709 | Self::Auto => 0,
            Self::Bt601 => 1,
            Self::Bt2020 => 2,
        }
    }

    /// Every variant, in menu order.
    pub const ALL: [Self; 4] = [Self::Auto, Self::Rec709, Self::Bt601, Self::Bt2020];
}

impl Display for ColorSpace {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("Auto"),
            Self::Rec709 => f.write_str("Rec. 709"),
            Self::Bt601 => f.write_str("BT.601"),
            Self::Bt2020 => f.write_str("BT.2020"),
        }
    }
}

/// Color range (quantization range) applied in the fragment shader.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColorRange {
    Auto,
    Limited,
    Full,
}

impl ColorRange {
    /// The `color_range` uniform value the shader branches on.
    /// Must stay in step with `VIDEO_SHADER`.
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Limited | Self::Auto => 0,
            Self::Full => 1,
        }
    }

    /// Every variant, in menu order.
    pub const ALL: [Self; 3] = [Self::Auto, Self::Limited, Self::Full];
}

impl Display for ColorRange {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("Auto"),
            Self::Limited => f.write_str("Limited (16-235)"),
            Self::Full => f.write_str("Full (0-255)"),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            overlay: Some(crate::overlay::OverlayLayout::default()),
            overlay_mode: Some(crate::overlay::Mode::Compact),
            video_device: String::new(),
            video_format: default_video_format(),
            scaling_filter: default_scaling_filter(),
            color_space: default_color_space(),
            color_range: default_color_range(),
            audio_input: default_audio_index(),
            audio_output: default_audio_index(),
            audio_input_name: None,
            audio_output_name: None,
            resolution: default_resolution(),
            fps_mode: default_fps_mode(),
            custom_fps: default_custom_fps(),
            volume: default_volume(),
            audio_muted: false,
            show_overlay: default_show_overlay(),
            detailed_overlay: false,
            brightness: default_brightness(),
            contrast: default_contrast(),
            saturation: default_saturation(),
            gamma: default_gamma(),
            sharpness: default_sharpness(),
            framegen_enabled: false,
            framegen_backend: FrameGenBackend::default(),
            framegen_rife_model: RifeModel::default(),
            framegen_mode: FrameGenMode::default(),
            framegen_target: FrameGenTarget::default(),
            framegen_overlay: false,
            framegen_runtime_dir: String::new(),
            framegen_require_signature: true,
            framegen_duplicates: Default::default(),
            framegen_skip_difference: default_framegen_skip_difference(),
            framegen_artifact_mode: FrameGenArtifactMode::Off,
            framegen_fix_static: true,
            framegen_fix_motion: true,
            framegen_fix_edges: true,
            framegen_fix_transitions: true,
            framegen_hud_guard: true,
            framegen_debug_view: FrameGenDebugView::Final,
            framegen_manual_zones: 0,
            framegen_manual_zone_width: default_framegen_zone_width(),
            framegen_manual_zone_height: default_framegen_zone_height(),
        }
    }
}

impl Settings {
    pub fn overlay_mode(&self) -> crate::overlay::Mode {
        self.overlay_mode.unwrap_or(if self.show_overlay {
            crate::overlay::Mode::Custom
        } else {
            crate::overlay::Mode::Hidden
        })
    }

    pub fn set_overlay_mode(&mut self, mode: crate::overlay::Mode) {
        self.overlay_mode = Some(mode);
        self.show_overlay = mode != crate::overlay::Mode::Hidden;
    }

    pub fn load() -> Self {
        let path = settings_path();
        let Ok(raw) = fs::read_to_string(path) else {
            return Self::default();
        };

        let mut settings: Self = serde_json::from_str(&raw).unwrap_or_default();
        settings.sanitize();
        settings
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json =
            serde_json::to_string_pretty(self).expect("settings serialization should not fail");
        fs::write(path, json)
    }

    pub fn sanitize(&mut self) {
        let mode = self.overlay_mode();
        self.overlay_mode = Some(mode);
        self.show_overlay = mode != crate::overlay::Mode::Hidden;
        if self.overlay.is_none() {
            self.overlay = Some(crate::overlay::OverlayLayout::legacy(self.detailed_overlay));
        }
        if let Some(layout) = &mut self.overlay {
            layout.normalize();
            if self.framegen_overlay {
                use crate::overlay::Stat;
                for row in &mut layout.stats {
                    if matches!(
                        row.stat,
                        Stat::FrameGeneration
                            | Stat::UniqueFps
                            | Stat::GeneratedFps
                            | Stat::PresentedFps
                    ) {
                        row.enabled = true;
                    }
                }
                self.framegen_overlay = false;
            }
        }
        fn finite(value: f32, default: f32, min: f32, max: f32) -> f32 {
            if value.is_finite() {
                value.clamp(min, max)
            } else {
                default
            }
        }
        self.brightness = finite(self.brightness, 0.0, -1.0, 1.0);
        self.contrast = finite(self.contrast, 1.0, 0.0, 2.0);
        self.saturation = finite(self.saturation, 1.0, 0.0, 2.0);
        self.gamma = finite(self.gamma, 1.0, 0.2, 3.0);
        self.sharpness = finite(self.sharpness, 1.0, 0.0, 2.0);
        self.volume = if self.volume.is_finite() {
            self.volume.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.custom_fps = self.custom_fps.clamp(MIN_FPS, MAX_FPS);
        self.framegen_duplicates = self.framegen_duplicates.sanitized();
        self.framegen_manual_zones &= 0xF;
        let pct = |v: f32, d: f32| if v.is_finite() { v.clamp(2.0, 50.0) } else { d };
        self.framegen_manual_zone_width = pct(
            self.framegen_manual_zone_width,
            default_framegen_zone_width(),
        );
        self.framegen_manual_zone_height = pct(
            self.framegen_manual_zone_height,
            default_framegen_zone_height(),
        );
        self.framegen_skip_difference = if self.framegen_skip_difference.is_finite() {
            self.framegen_skip_difference.clamp(0.0, 128.0)
        } else {
            default_framegen_skip_difference()
        };
        self.framegen_runtime_dir = self.framegen_runtime_dir.trim().to_string();
        if self.fps_mode != FPS_MODE_CUSTOM && self.fps_mode.parse::<FrameRate>().is_err() {
            self.fps_mode = default_fps_mode();
        }
    }

    pub fn get_fps(&self) -> FrameRate {
        if self.fps_mode == FPS_MODE_CUSTOM {
            self.custom_fps.clamp(MIN_FPS, MAX_FPS).into()
        } else {
            self.fps_mode.parse().unwrap_or_else(|_| 60.into())
        }
    }

    pub fn apply_negotiated(&mut self, width: u32, height: u32, fps: FrameRate) {
        self.resolution = crate::capabilities::resolution_label(width, height);
        self.fps_mode = fps.key();
    }
}

pub fn get_capture_config(
    resolution: &str,
    fps: FrameRate,
    video_format: VideoFormat,
) -> CaptureConfig {
    let (width, height) = crate::capabilities::resolution_dimensions(resolution);
    CaptureConfig {
        width,
        height,
        fps,
        pixel_format: video_format.ffmpeg_name(),
        decode_threads: if video_format == VideoFormat::Mjpeg {
            4
        } else {
            1
        },
    }
}

pub fn settings_path() -> PathBuf {
    let mut base = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    if cfg!(debug_assertions) {
        if let Ok(current_dir) = std::env::current_dir() {
            base = current_dir;
        }
    }

    base.join("tacklecast_settings.json")
}

fn default_true() -> bool {
    true
}

fn default_framegen_skip_difference() -> f32 {
    32.0
}

fn default_framegen_zone_width() -> f32 {
    15.0
}

fn default_framegen_zone_height() -> f32 {
    25.0
}

fn default_audio_index() -> i32 {
    -1
}

fn default_video_format() -> VideoFormat {
    VideoFormat::Auto
}

fn default_scaling_filter() -> ScaleFilter {
    ScaleFilter::Bilinear
}

fn default_color_space() -> ColorSpace {
    ColorSpace::Auto
}

fn default_color_range() -> ColorRange {
    ColorRange::Auto
}

fn default_resolution() -> String {
    "1080p".to_string()
}

fn default_fps_mode() -> String {
    FPS_MODE_60.to_string()
}

fn default_custom_fps() -> u32 {
    120
}

fn default_volume() -> f64 {
    1.0
}

fn default_show_overlay() -> bool {
    true
}

fn default_brightness() -> f32 {
    0.0
}

fn default_contrast() -> f32 {
    1.0
}

fn default_saturation() -> f32 {
    1.0
}

fn default_gamma() -> f32 {
    1.0
}

fn default_sharpness() -> f32 {
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vi_overlay_and_explicit_routes_survive_framegen_integration() {
        use crate::overlay::{Corner, Stat};
        let mut s: Settings = serde_json::from_str(
            r#"{
            "theme":"classic", "video_device":"chosen capture", "audio_input":3,
            "audio_input_name":"intentional audio", "audio_output":2,
            "overlay":{"corner":"bottom_right", "stats":[
                {"stat":"pixel_format","enabled":true},
                {"stat":"resolution","enabled":false}]}
        }"#,
        )
        .unwrap();
        s.sanitize();
        assert!(!s.framegen_enabled);
        assert_eq!(s.theme, Theme::Classic);
        assert_eq!(s.video_device, "chosen capture");
        assert_eq!((s.audio_input, s.audio_output), (3, 2));
        let layout = s.overlay.as_ref().unwrap();
        assert_eq!(layout.corner, Corner::BottomRight);
        assert_eq!(layout.stats[0].stat, Stat::PixelFormat);
        assert!(layout.stats[0].enabled);
        assert_eq!(layout.stats[1].stat, Stat::Resolution);
        assert!(!layout.stats[1].enabled);
        assert!(layout.stats[2..].iter().all(|r| !r.enabled));
        s.framegen_enabled = true;
        s.framegen_rife_model = RifeModel::V46Half;
        let json = serde_json::to_string(&s).unwrap();
        let mut reloaded: Settings = serde_json::from_str(&json).unwrap();
        reloaded.sanitize();
        assert_eq!(s, reloaded);
    }

    #[test]
    fn pr28_overlay_preference_migrates_once_into_customizable_statistics() {
        use crate::overlay::Stat;
        let mut s: Settings =
            serde_json::from_str(r#"{"framegen_overlay":true,"show_overlay":false}"#).unwrap();
        s.sanitize();
        assert!(!s.show_overlay);
        assert!(!s.framegen_overlay);
        let layout = s.overlay.as_mut().unwrap();
        let row = layout
            .stats
            .iter_mut()
            .find(|r| r.stat == Stat::PresentedFps)
            .unwrap();
        assert!(row.enabled);
        row.enabled = false;
        s.sanitize();
        assert!(
            !s.overlay
                .unwrap()
                .stats
                .iter()
                .find(|r| r.stat == Stat::PresentedFps)
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn overlay_modes_preserve_custom_layout_across_switches_and_old_settings() {
        use crate::overlay::{Corner, Mode};
        let mut s = Settings::default();
        assert_eq!(s.overlay_mode(), Mode::Compact);
        let layout = s.overlay.as_mut().unwrap();
        layout.corner = Corner::BottomRight;
        layout.stats.reverse();
        layout.stats[0].enabled = true;
        let expected = layout.clone();
        for mode in [Mode::Custom, Mode::Hidden, Mode::Compact, Mode::Custom] {
            s.set_overlay_mode(mode);
            let json = serde_json::to_string(&s).unwrap();
            let mut reloaded: Settings = serde_json::from_str(&json).unwrap();
            reloaded.sanitize();
            assert_eq!(reloaded.overlay_mode(), mode);
            assert_eq!(reloaded.show_overlay, mode != Mode::Hidden);
            assert_eq!(reloaded.overlay.as_ref().unwrap(), &expected);
        }
        for (old, mode) in [
            (r#"{"show_overlay":false}"#, Mode::Hidden),
            (r#"{"show_overlay":true}"#, Mode::Custom),
        ] {
            let mut old: Settings = serde_json::from_str(old).unwrap();
            old.sanitize();
            assert_eq!(old.overlay_mode(), mode);
        }
    }

    #[test]
    fn edition_defaults_to_vice_and_persists_classic() {
        let value = serde_json::to_value(Settings::default()).unwrap();
        assert_eq!(value["theme"], "vice");
        let settings: Settings = serde_json::from_str(r#"{"theme":"classic"}"#).unwrap();
        assert_eq!(serde_json::to_value(settings).unwrap()["theme"], "classic");
    }

    #[test]
    fn overlay_legacy_migration_preserves_master_and_detail() {
        let mut compact: Settings = serde_json::from_str(r#"{"show_overlay":false}"#).unwrap();
        compact.sanitize();
        let json = serde_json::to_value(&compact).unwrap();
        assert!(!compact.show_overlay);
        assert_eq!(
            json["overlay"]["stats"].as_array().unwrap().len(),
            crate::overlay::Stat::ALL.len()
        );
        assert_eq!(json["overlay"]["stats"][4]["enabled"], false);
        let mut detailed: Settings = serde_json::from_str(r#"{"detailed_overlay":true}"#).unwrap();
        detailed.sanitize();
        let json = serde_json::to_value(detailed).unwrap();
        assert!(json["overlay"]["stats"]
            .as_array()
            .unwrap()
            .iter()
            .take(7)
            .all(|s| s["enabled"] == true));
    }

    #[test]
    fn overlay_normalizes_duplicates_missing_entries_and_preserves_all_off() {
        let mut s: Settings = serde_json::from_str(r#"{"overlay":{"stats":[{"stat":"pixel_format","enabled":false},{"stat":"pixel_format","enabled":true}]}}"#).unwrap();
        s.sanitize();
        let layout = s.overlay.unwrap();
        assert_eq!(layout.stats.len(), crate::overlay::Stat::ALL.len());
        assert_eq!(layout.stats[0].stat, crate::overlay::Stat::PixelFormat);
        assert!(layout.stats.iter().all(|row| !row.enabled));
        let mut empty: Settings = serde_json::from_str(r#"{"overlay":{"stats":[]}}"#).unwrap();
        empty.sanitize();
        assert!(empty.overlay.unwrap().stats.iter().all(|row| !row.enabled));
    }

    #[test]
    fn default_settings_round_trip() {
        let settings = Settings::default();
        let json = serde_json::to_string_pretty(&settings).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, settings);
    }

    #[test]
    fn picture_settings_serialize_and_defaults() {
        let json = r#"{
            "brightness": 0.1,
            "contrast": 1.2,
            "saturation": 0.8,
            "gamma": 1.5
        }"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(s.brightness, 0.1);
        assert_eq!(s.contrast, 1.2);
        assert_eq!(s.saturation, 0.8);
        assert_eq!(s.gamma, 1.5);
    }

    #[test]
    fn python_settings_shape_deserializes() {
        let json = r#"{
  "video_device": "ShadowCast 3",
  "scaling_filter": "bicubic",
  "audio_input": 15,
  "audio_output": 12,
  "resolution": "1440p",
  "fps_mode": "120",
  "custom_fps": 120,
  "volume": 1.0,
  "show_overlay": true
}"#;

        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.video_device, "ShadowCast 3");
        assert_eq!(settings.scaling_filter, ScaleFilter::Bicubic);
        assert_eq!(settings.video_format, VideoFormat::Auto);
        assert_eq!(settings.color_space, ColorSpace::Auto);
        assert_eq!(settings.color_range, ColorRange::Auto);
        assert_eq!(settings.audio_input, 15);
        assert_eq!(settings.audio_output, 12);
        assert_eq!(settings.resolution, "1440p");
        assert_eq!(settings.fps_mode, "120");
        assert_eq!(settings.get_fps(), 120.into());
        assert!(settings.show_overlay);
        assert!(!settings.audio_muted);
    }

    #[test]
    fn settings_with_audio_muted_deserializes() {
        let json = r#"{
  "video_device": "Cam Link",
  "volume": 0.6,
  "audio_muted": true
}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.video_device, "Cam Link");
        assert!((settings.volume - 0.6).abs() < 1e-4);
        assert!(settings.audio_muted);
    }

    #[test]
    fn video_format_serialization_and_aliases() {
        let settings = Settings {
            video_format: VideoFormat::Yuy2,
            ..Settings::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""video_format":"yuy2""#));

        // Test aliases
        let mjpg: Settings = serde_json::from_str(r#"{"video_format":"mjpg"}"#).unwrap();
        assert_eq!(mjpg.video_format, VideoFormat::Mjpeg);

        let yuyv: Settings = serde_json::from_str(r#"{"video_format":"yuyv"}"#).unwrap();
        assert_eq!(yuyv.video_format, VideoFormat::Yuy2);

        let yuyv422: Settings = serde_json::from_str(r#"{"video_format":"yuyv422"}"#).unwrap();
        assert_eq!(yuyv422.video_format, VideoFormat::Yuy2);

        let yuv12: Settings = serde_json::from_str(r#"{"video_format":"yuv12"}"#).unwrap();
        assert_eq!(yuv12.video_format, VideoFormat::Yuv420p);

        let yv12: Settings = serde_json::from_str(r#"{"video_format":"yv12"}"#).unwrap();
        assert_eq!(yv12.video_format, VideoFormat::Yuv420p);

        let i420: Settings = serde_json::from_str(r#"{"video_format":"i420"}"#).unwrap();
        assert_eq!(i420.video_format, VideoFormat::Yuv420p);
    }

    #[test]
    fn color_space_and_range_serialize() {
        let settings = Settings {
            color_space: ColorSpace::Rec709,
            color_range: ColorRange::Full,
            ..Settings::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""color_space":"rec709""#));
        assert!(json.contains(r#""color_range":"full""#));

        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.color_space, ColorSpace::Rec709);
        assert_eq!(decoded.color_range, ColorRange::Full);
    }

    #[test]
    fn get_capture_config_explicit_formats() {
        let mjpeg = get_capture_config("1080p", 60.into(), VideoFormat::Mjpeg);
        assert_eq!(mjpeg.pixel_format, "mjpeg");
        assert_eq!(mjpeg.decode_threads, 4);

        let nv12 = get_capture_config("1440p", 120.into(), VideoFormat::Nv12);
        assert_eq!(nv12.pixel_format, "nv12");
        assert_eq!(nv12.decode_threads, 1);

        let yuy2 = get_capture_config("1080p", 60.into(), VideoFormat::Yuy2);
        assert_eq!(yuy2.pixel_format, "yuyv422");
        assert_eq!(yuy2.decode_threads, 1);

        let uyvy = get_capture_config("1080p", 60.into(), VideoFormat::Uyvy);
        assert_eq!(uyvy.pixel_format, "uyvy422");

        let yuv420p = get_capture_config("1080p", 60.into(), VideoFormat::Yuv420p);
        assert_eq!(yuv420p.pixel_format, "yuv420p");
    }

    #[test]
    fn invalid_picture_values_are_sanitized_and_defaults_are_neutral() {
        let mut s = Settings {
            brightness: 5.0,
            contrast: -1.0,
            saturation: f32::NAN,
            gamma: 0.0,
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(
            (s.brightness, s.contrast, s.saturation, s.gamma),
            (1.0, 0.0, 1.0, 0.2)
        );
        let neutral = Settings::default();
        assert_eq!(
            (
                neutral.brightness,
                neutral.contrast,
                neutral.saturation,
                neutral.gamma
            ),
            (0.0, 1.0, 1.0, 1.0)
        );
        assert_eq!(
            get_capture_config("4K", 60.into(), VideoFormat::Auto).pixel_format,
            "auto"
        );
        let restored: Settings =
            serde_json::from_str(r#"{"scaling_filter":"fsr1","brightness":0.1}"#).unwrap();
        assert_eq!(restored.scaling_filter, ScaleFilter::Fsr1);
        assert_eq!(restored.brightness, 0.1);
        assert_eq!(restored.sharpness, 1.0);

        for alias in ["rtx_vsr", "vsr"] {
            let s: Settings =
                serde_json::from_str(&format!(r#"{{"scaling_filter":"{alias}"}}"#)).unwrap();
            assert_eq!(s.scaling_filter, ScaleFilter::RtxVsr);
        }
    }
    #[test]
    fn unknown_rife_model_falls_back_to_default() {
        let s: Settings =
            serde_json::from_str(r#"{"framegen_rife_model":"4.26_heavy","volume":0.5}"#).unwrap();
        assert_eq!(s.framegen_rife_model, RifeModel::default());
        assert_eq!(s.volume, 0.5);
        let s: Settings = serde_json::from_str(r#"{"framegen_rife_model":"4.6_half"}"#).unwrap();
        assert_eq!(s.framegen_rife_model, RifeModel::V46Half);
    }
}
