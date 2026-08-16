use std::fmt::{Display, Formatter};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const FPS_MODE_30: &str = "30";
pub const FPS_MODE_60: &str = "60";
pub const FPS_MODE_120: &str = "120";
#[allow(dead_code)]
pub const FPS_MODE_CUSTOM: &str = "custom";
pub const MIN_FPS: u32 = 30;
pub const MAX_FPS: u32 = 240;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub video_device: String,
    #[serde(default = "default_scaling_filter")]
    pub scaling_filter: ScaleFilter,
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
    #[serde(default = "default_show_overlay")]
    pub show_overlay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub pixel_format: &'static str,
    pub decode_threads: usize,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ScaleFilter { Bilinear, Bicubic, Lanczos }
impl ScaleFilter {
    pub(crate) fn as_u32(self) -> u32 {
        match self { Self::Bilinear => 0, Self::Bicubic => 1, Self::Lanczos => 2 }
    }
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

impl Default for Settings {
    fn default() -> Self {
        Self {
            video_device: String::new(),
            scaling_filter: default_scaling_filter(),
            audio_input: default_audio_index(),
            audio_output: default_audio_index(),
            resolution: default_resolution(),
            fps_mode: default_fps_mode(),
            custom_fps: default_custom_fps(),
            volume: default_volume(),
            show_overlay: default_show_overlay(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let path = settings_path();
        let Ok(raw) = fs::read_to_string(path) else {
            return Self::default();
        };

        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(self)
            .expect("settings serialization should not fail");
        fs::write(path, json)
    }

    pub fn get_fps(&self) -> u32 {
        match self.fps_mode.as_str() {
            FPS_MODE_30 => 30,
            FPS_MODE_60 => 60,
            FPS_MODE_120 => 120,
            _ => self.custom_fps.clamp(MIN_FPS, MAX_FPS),
        }
    }

    /// Update resolution and fps_mode to reflect what the capture device
    /// actually negotiated (e.g. after fallback to a lower resolution/fps).
    pub fn apply_negotiated(&mut self, width: u32, height: u32, fps: u32) {
        let new_resolution = match (width, height) {
            (3840, 2160) => "4K",
            (2560, 1440) => "1440p",
            (1280, 720) => "720p",
            _ => "1080p",
        };
        let new_fps_mode = match fps {
            30 => FPS_MODE_30,
            120 => FPS_MODE_120,
            60 => FPS_MODE_60,
            other => {
                self.custom_fps = other;
                FPS_MODE_CUSTOM
            }
        };

        self.resolution = new_resolution.to_string();
        self.fps_mode = new_fps_mode.to_string();
    }
}

pub fn get_capture_config(resolution: &str, fps: u32) -> CaptureConfig {
    let (width, height) = match resolution {
        "720p" => (1280, 720),
        "1440p" => (2560, 1440),
        "4K" => (3840, 2160),
        _ => (1920, 1080),
    };

    if fps <= 60 {
        CaptureConfig {
            width,
            height,
            fps,
            pixel_format: "nv12",
            decode_threads: 1,
        }
    } else {
        CaptureConfig {
            width,
            height,
            fps,
            pixel_format: "mjpeg",
            decode_threads: 4,
        }
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

fn default_scaling_filter() -> ScaleFilter {
    ScaleFilter::Bilinear
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
        assert_eq!(settings.audio_input, 15);
        assert_eq!(settings.audio_output, 12);
        assert_eq!(settings.resolution, "1440p");
        assert_eq!(settings.fps_mode, "120");
        assert_eq!(settings.get_fps(), 120);
        assert!(settings.show_overlay);
    }

    #[test]
    fn capture_config_matches_python_logic() {
        let nv12 = get_capture_config("1080p", 60);
        assert_eq!(nv12.pixel_format, "nv12");
        assert_eq!(nv12.decode_threads, 1);

        let mjpeg = get_capture_config("1440p", 120);
        assert_eq!(mjpeg.width, 2560);
        assert_eq!(mjpeg.height, 1440);
        assert_eq!(mjpeg.pixel_format, "mjpeg");
        assert_eq!(mjpeg.decode_threads, 4);
    }
}
