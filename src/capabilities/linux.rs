use super::CaptureMode;
use crate::frame_rate::FrameRate;
use crate::settings::VideoFormat;
use std::ffi::{CStr, CString};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativeMode {
    fourcc: u32,
    width: u32,
    height: u32,
    numerator: u32,
    denominator: u32,
}
unsafe extern "C" {
    fn tc_v4l2_modes(
        path: *const std::ffi::c_char,
        modes: *mut NativeMode,
        capacity: usize,
        error: *mut std::ffi::c_char,
        error_size: usize,
    ) -> i32;
}

pub fn query(device: &str) -> Result<Vec<CaptureMode>, String> {
    let path = crate::capture::resolve_v4l2_node(device);
    let path = CString::new(path).map_err(|_| "Invalid capture-device path")?;
    let mut native = vec![NativeMode::default(); 16384];
    let mut error = [0i8; 512];
    let count = unsafe {
        tc_v4l2_modes(
            path.as_ptr(),
            native.as_mut_ptr(),
            native.len(),
            error.as_mut_ptr(),
            error.len(),
        )
    };
    if count < 0 {
        return Err(unsafe { CStr::from_ptr(error.as_ptr()) }
            .to_string_lossy()
            .into_owned());
    }
    let mut modes = Vec::new();
    for mode in native.into_iter().take(count as usize) {
        let format = match mode.fourcc {
            0x3231564e => VideoFormat::Nv12,
            0x47504a4d => VideoFormat::Mjpeg,
            0x56595559 => VideoFormat::Yuy2,
            0x59565955 => VideoFormat::Uyvy,
            0x32315559 => VideoFormat::Yuv420p,
            _ => continue,
        };
        if mode.numerator == 0 || mode.denominator == 0 {
            continue;
        }
        // V4L2 stores time-per-frame, the reciprocal of frames-per-second.
        modes.push(CaptureMode {
            format,
            width: mode.width,
            height: mode.height,
            rate: FrameRate::new(mode.denominator, mode.numerator),
        });
    }
    Ok(modes)
}
