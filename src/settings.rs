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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
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
    #[serde(alias = "fsr1")]
    Bilinear,
    Bicubic,
    Lanczos,
}

impl ScaleFilter {
    /// The `filter_mode` value the shader branches on. Must stay in step with
    /// the `filter_mode` comparisons in `VIDEO_SHADER`.
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Bilinear => 0,
            Self::Bicubic => 1,
            Self::Lanczos => 2,
        }
    }

    /// Every variant, in menu order.
    pub const ALL: [Self; 3] = [Self::Bilinear, Self::Bicubic, Self::Lanczos];
}

impl Display for ScaleFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bilinear => f.write_str("Bilinear"),
            Self::Bicubic => f.write_str("Bicubic"),
            Self::Lanczos => f.write_str("Lanczos"),
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
            video_device: String::new(),
            video_format: default_video_format(),
            scaling_filter: default_scaling_filter(),
            color_space: default_color_space(),
            color_range: default_color_range(),
            audio_input: default_audio_index(),
            audio_output: default_audio_index(),
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
        }
    }
}

impl Settings {
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
        self.volume = if self.volume.is_finite() {
            self.volume.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.custom_fps = self.custom_fps.clamp(MIN_FPS, MAX_FPS);
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(restored.scaling_filter, ScaleFilter::Bilinear);
        assert_eq!(restored.brightness, 0.1);
    }
}
