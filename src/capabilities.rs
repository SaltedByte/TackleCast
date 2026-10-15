use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::frame_rate::FrameRate;
use crate::settings::{Settings, VideoFormat};

#[cfg(target_os = "windows")]
#[path = "capabilities/windows.rs"]
mod platform;
#[cfg(target_os = "windows")]
pub use platform::handle_probe_args;
#[cfg(target_os = "linux")]
#[path = "capabilities/linux.rs"]
mod platform;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureMode {
    pub format: VideoFormat,
    pub width: u32,
    pub height: u32,
    pub rate: FrameRate,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceCapabilities {
    pub modes: Vec<CaptureMode>,
    pub message: Option<String>,
}

type Cache = Mutex<HashMap<String, Arc<DeviceCapabilities>>>;
static CACHE: OnceLock<Cache> = OnceLock::new();

pub fn query_device_capabilities(device: &str) -> DeviceCapabilities {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    let result = platform::query(device);
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let result: Result<Vec<CaptureMode>, String> =
        Err("Mode discovery is unavailable on this platform".into());
    match result {
        Ok(mut modes) => {
            modes.retain(|mode| {
                (1.0..=240.0).contains(&mode.rate.as_f64()) && mode.width > 0 && mode.height > 0
            });
            modes.sort_by(|a, b| {
                (a.width, a.height)
                    .cmp(&(b.width, b.height))
                    .then_with(|| a.rate.as_f64().total_cmp(&b.rate.as_f64()))
                    .then_with(|| a.format.ffmpeg_name().cmp(b.format.ffmpeg_name()))
            });
            modes.dedup();
            let message = modes.is_empty().then(|| "The device did not report supported capture modes. Selections cannot be verified.".to_string());
            DeviceCapabilities { modes, message }
        }
        Err(message) => DeviceCapabilities {
            modes: Vec::new(),
            message: Some(message),
        },
    }
}

/// Prime the selected card before capture opens it. Other cards are queried on
/// background threads so a slow driver cannot stall the menu or active video.
pub fn prime_device_capabilities(device: &str) {
    let caps = Arc::new(query_device_capabilities(device));
    CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(device.to_string(), caps);
}

pub fn get_device_capabilities(device: &str) -> Arc<DeviceCapabilities> {
    if device.is_empty() {
        return Arc::new(test_pattern_capabilities());
    }
    let cache = CACHE.get_or_init(Default::default);
    let mut guard = cache.lock().unwrap();
    if let Some(caps) = guard.get(device) {
        return caps.clone();
    }
    let pending = Arc::new(DeviceCapabilities {
        modes: vec![],
        message: Some("Reading device capture modes…".into()),
    });
    guard.insert(device.to_string(), pending.clone());
    let device = device.to_string();
    std::thread::spawn(move || prime_device_capabilities(&device));
    pending
}

pub fn refresh(device: &str) {
    CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .remove(device);
}

pub fn resolution_dimensions(resolution: &str) -> (u32, u32) {
    match resolution {
        "720p" => (1280, 720),
        "1080p" => (1920, 1080),
        "1440p" => (2560, 1440),
        "4K" => (3840, 2160),
        other => other
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            .filter(|(w, h)| *w > 0 && *h > 0)
            .unwrap_or((1920, 1080)),
    }
}

pub fn resolution_label(width: u32, height: u32) -> String {
    match (width, height) {
        (1280, 720) => "720p".into(),
        (1920, 1080) => "1080p".into(),
        (2560, 1440) => "1440p".into(),
        (3840, 2160) => "4K".into(),
        _ => format!("{width}x{height}"),
    }
}

pub fn supported_resolutions(caps: &DeviceCapabilities) -> Vec<String> {
    let mut values = Vec::new();
    for mode in &caps.modes {
        let value = resolution_label(mode.width, mode.height);
        if !values.contains(&value) {
            values.push(value);
        }
    }
    values
}

pub fn supported_rates(
    caps: &DeviceCapabilities,
    resolution: &str,
    format: VideoFormat,
) -> Vec<FrameRate> {
    let (w, h) = resolution_dimensions(resolution);
    let mut rates = Vec::new();
    for mode in &caps.modes {
        if mode.width == w
            && mode.height == h
            && (format == VideoFormat::Auto || mode.format == format)
            && !rates.iter().any(|r: &FrameRate| r.equivalent(mode.rate))
        {
            rates.push(mode.rate);
        }
    }
    rates.sort_by(|a, b| a.as_f64().total_cmp(&b.as_f64()));
    rates
}

pub fn supported_video_formats(
    caps: &DeviceCapabilities,
    resolution: &str,
    rate: FrameRate,
) -> Vec<VideoFormat> {
    let (w, h) = resolution_dimensions(resolution);
    let mut formats = vec![VideoFormat::Auto];
    for format in [
        VideoFormat::Nv12,
        VideoFormat::Mjpeg,
        VideoFormat::Yuy2,
        VideoFormat::Uyvy,
        VideoFormat::Yuv420p,
    ] {
        if caps.modes.iter().any(|mode| {
            mode.width == w
                && mode.height == h
                && mode.format == format
                && mode.rate.equivalent(rate)
        }) {
            formats.push(format);
        }
    }
    formats
}

pub fn sanitize_draft_settings(draft: &mut Settings, caps: &DeviceCapabilities) {
    if caps.modes.is_empty() {
        return;
    }
    let resolutions = supported_resolutions(caps);
    if !resolutions.contains(&draft.resolution) {
        draft.resolution = resolutions.last().unwrap().clone();
    }
    // Choose from real modes. Never merge disjoint intervals into a made-up range.
    let rates = supported_rates(caps, &draft.resolution, VideoFormat::Auto);
    let wanted = draft.get_fps();
    if !rates.iter().any(|rate| rate.equivalent(wanted)) {
        if let Some(rate) = rates.iter().min_by(|a, b| {
            (a.as_f64() - wanted.as_f64())
                .abs()
                .total_cmp(&(b.as_f64() - wanted.as_f64()).abs())
        }) {
            draft.fps_mode = rate.key();
        }
    }
    if !supported_video_formats(caps, &draft.resolution, draft.get_fps())
        .contains(&draft.video_format)
    {
        draft.video_format = VideoFormat::Auto;
    }
}

fn test_pattern_capabilities() -> DeviceCapabilities {
    let mut modes = Vec::new();
    for (width, height) in [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)] {
        for rate in FrameRate::common() {
            for format in [VideoFormat::Nv12, VideoFormat::Mjpeg] {
                modes.push(CaptureMode {
                    width,
                    height,
                    rate,
                    format,
                });
            }
        }
    }
    DeviceCapabilities {
        modes,
        message: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mode(format: VideoFormat, fps: u32) -> CaptureMode {
        CaptureMode {
            format,
            width: 1920,
            height: 1080,
            rate: fps.into(),
        }
    }
    #[test]
    fn no_rates_are_invented_between_disjoint_modes() {
        let caps = DeviceCapabilities {
            modes: vec![mode(VideoFormat::Nv12, 30), mode(VideoFormat::Mjpeg, 120)],
            message: None,
        };
        assert_eq!(
            supported_rates(&caps, "1080p", VideoFormat::Auto),
            vec![30.into(), 120.into()]
        );
        assert_eq!(
            supported_video_formats(&caps, "1080p", 60.into()),
            vec![VideoFormat::Auto]
        );
        assert_eq!(
            supported_video_formats(&caps, "1080p", 120.into()),
            vec![VideoFormat::Auto, VideoFormat::Mjpeg]
        );
    }
    #[test]
    fn lower_bound_and_fractional_modes_are_respected() {
        let mut exact = mode(VideoFormat::Nv12, 60);
        exact.rate = FrameRate::new(60_000, 1001);
        let caps = DeviceCapabilities {
            modes: vec![exact],
            message: None,
        };
        assert_eq!(
            supported_video_formats(&caps, "1080p", 30.into()),
            vec![VideoFormat::Auto]
        );
        assert_eq!(
            supported_video_formats(&caps, "1080p", 60.into()),
            vec![VideoFormat::Auto]
        );
        assert_eq!(
            supported_rates(&caps, "1080p", VideoFormat::Nv12)[0].key(),
            "60000/1001"
        );
    }
    #[test]
    fn switching_resolution_selects_an_existing_mode() {
        let caps = DeviceCapabilities {
            modes: vec![CaptureMode {
                width: 3840,
                height: 2160,
                ..mode(VideoFormat::Mjpeg, 60)
            }],
            message: None,
        };
        let mut draft = Settings {
            resolution: "1080p".into(),
            fps_mode: "120".into(),
            video_format: VideoFormat::Nv12,
            ..Settings::default()
        };
        sanitize_draft_settings(&mut draft, &caps);
        assert_eq!(draft.resolution, "4K");
        assert_eq!(draft.fps_mode, "60");
        assert_eq!(draft.video_format, VideoFormat::Auto);
    }
}
