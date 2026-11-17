//! FFI declarations for NVIDIA Optical Flow SDK 5.0.7 `NvOFFRUC.h`.
//!
//! Transcribed by hand from the header shipped with the SDK (no NVIDIA source
//! is copied into this program). The library is loaded dynamically at run
//! time; nothing here links against it.
//!
//! All structs are `#[repr(C)]` and their sizes are pinned by tests to the
//! values the x64 MSVC compiler produces for the header, so a transcription
//! mistake fails the build's test run instead of corrupting memory.

#![allow(dead_code)]

use std::ffi::c_void;

pub const NVOFFRUC_MAX_RESOURCE: usize = 10;
pub const NVOFFRUC_MIN_RESOURCE: usize = 3;

pub const CREATE_PROC: &[u8] = b"NvOFFRUCCreate\0";
pub const REGISTER_PROC: &[u8] = b"NvOFFRUCRegisterResource\0";
pub const UNREGISTER_PROC: &[u8] = b"NvOFFRUCUnregisterResource\0";
pub const PROCESS_PROC: &[u8] = b"NvOFFRUCProcess\0";
pub const DESTROY_PROC: &[u8] = b"NvOFFRUCDestroy\0";

/// `NvOFFRUCCUDAResourceType`
pub const CUDA_RESOURCE_CU_DEVICE_PTR: i32 = 0;
pub const CUDA_RESOURCE_CU_ARRAY: i32 = 1;
/// `NvOFFRUCResourceType`
pub const RESOURCE_TYPE_CUDA: i32 = 0;
pub const RESOURCE_TYPE_DIRECTX11: i32 = 1;
/// `NvOFFRUCSurfaceFormat`. "ARGB" is R8G8B8A8_UNORM in memory order
/// (the SDK sample creates DXGI_FORMAT_R8G8B8A8_UNORM textures for it).
pub const SURFACE_NV12: i32 = 0;
pub const SURFACE_ARGB: i32 = 1;

pub type NvOFFRUCHandle = *mut c_void;

/// `NvOFFRUC_STATUS`
pub type NvOFFRUCStatus = i32;
pub const NVOFFRUC_SUCCESS: NvOFFRUCStatus = 0;

/// Names for `NvOFFRUC_STATUS` values, in header order.
pub fn status_name(status: NvOFFRUCStatus) -> &'static str {
    match status {
        0 => "NvOFFRUC_SUCCESS",
        1 => "NvOFFRUC_ERR_NvOFFRUC_NOT_SUPPORTED (optical-flow hardware not supported)",
        2 => "NvOFFRUC_ERR_INVALID_PTR",
        3 => "NvOFFRUC_ERR_INVALID_PARAM",
        4 => "NvOFFRUC_ERR_INVALID_HANDLE",
        5 => "NvOFFRUC_ERR_OUT_OF_SYSTEM_MEMORY",
        6 => "NvOFFRUC_ERR_OUT_OF_VIDEO_MEMORY",
        7 => "NvOFFRUC_ERR_OPENCV_NOT_AVAILABLE",
        8 => "NvOFFRUC_ERR_UNIMPLEMENTED",
        9 => "NvOFFRUC_ERR_OF_FAILURE (optical flow could not be generated)",
        10 => "NvOFFRUC_ERR_DUPLICATE_RESOURCE",
        11 => "NvOFFRUC_ERR_UNREGISTERED_RESOURCE",
        12 => "NvOFFRUC_ERR_INCORRECT_API_SEQUENCE",
        13 => "NvOFFRUC_ERR_WRITE_TODISK_FAILED",
        14 => "NvOFFRUC_ERR_PIPELINE_EXECUTION_FAILURE",
        15 => "NvOFFRUC_ERR_SYNC_WRITE_FAILED",
        16 => "NvOFFRUC_ERR_GENERIC",
        _ => "unknown NvOFFRUC status",
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvOFFRUCCreateParam {
    pub ui_width: u32,
    pub ui_height: u32,
    pub p_device: *mut c_void,
    pub e_resource_type: i32,
    pub e_surface_format: i32,
    pub e_cuda_resource_type: i32,
    pub ui_reserved: [u32; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvOFFRUCFrameData {
    /// For CUDA device pointers: the address of the registered `CUdeviceptr`.
    pub p_frame: *mut c_void,
    pub n_time_stamp: f64,
    /// Pitch for CUDA pitch allocations; 0 means tightly packed.
    pub n_cu_surface_pitch: usize,
    pub b_has_frame_repetition_occurred: *mut bool,
    pub ui_reserved: [u32; 32],
}

/// `SyncWait` / `SyncSignal` unions: two u64s (only used for D3D11).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SyncValues {
    pub value0: u64,
    pub value1: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvOFFRUCProcessInParams {
    pub st_frame_data_input: NvOFFRUCFrameData,
    /// `uint32_t bSkipWarp : 1;` — bit 0 of this word.
    pub flags: u32,
    pub u_sync_wait: SyncValues,
    pub ui_reserved: [u32; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvOFFRUCProcessOutParams {
    pub st_frame_data_output: NvOFFRUCFrameData,
    pub u_sync_signal: SyncValues,
    pub ui_reserved: [u32; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvOFFRUCRegisterResourceParam {
    pub p_arr_resource: [*mut c_void; NVOFFRUC_MAX_RESOURCE],
    pub p_d3d11_fence_obj: *mut c_void,
    pub ui_count: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvOFFRUCUnregisterResourceParam {
    pub p_arr_resource: [*mut c_void; NVOFFRUC_MAX_RESOURCE],
    pub ui_count: u32,
}

macro_rules! zeroed_ctor {
    ($($t:ty),*) => {$(
        impl $t {
            pub fn zeroed() -> Self {
                // SAFETY: all fields are integers, floats, bools-by-pointer or
                // raw pointers, for which all-zero is a valid value.
                unsafe { std::mem::zeroed() }
            }
        }
    )*};
}
zeroed_ctor!(
    NvOFFRUCCreateParam,
    NvOFFRUCFrameData,
    NvOFFRUCProcessInParams,
    NvOFFRUCProcessOutParams,
    NvOFFRUCRegisterResourceParam,
    NvOFFRUCUnregisterResourceParam
);

pub const SKIP_WARP_BIT: u32 = 1;

pub type PfnCreate =
    unsafe extern "system" fn(*const NvOFFRUCCreateParam, *mut NvOFFRUCHandle) -> NvOFFRUCStatus;
pub type PfnRegister = unsafe extern "system" fn(
    NvOFFRUCHandle,
    *const NvOFFRUCRegisterResourceParam,
) -> NvOFFRUCStatus;
pub type PfnUnregister = unsafe extern "system" fn(
    NvOFFRUCHandle,
    *const NvOFFRUCUnregisterResourceParam,
) -> NvOFFRUCStatus;
pub type PfnProcess = unsafe extern "system" fn(
    NvOFFRUCHandle,
    *const NvOFFRUCProcessInParams,
    *const NvOFFRUCProcessOutParams,
) -> NvOFFRUCStatus;
pub type PfnDestroy = unsafe extern "system" fn(NvOFFRUCHandle) -> NvOFFRUCStatus;

#[cfg(all(test, target_pointer_width = "64"))]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    // Expected values computed from NvOFFRUC.h for x64 (enums are 4 bytes).
    #[test]
    fn create_param_layout() {
        assert_eq!(offset_of!(NvOFFRUCCreateParam, p_device), 8);
        assert_eq!(offset_of!(NvOFFRUCCreateParam, e_resource_type), 16);
        assert_eq!(offset_of!(NvOFFRUCCreateParam, e_cuda_resource_type), 24);
        assert_eq!(offset_of!(NvOFFRUCCreateParam, ui_reserved), 28);
        assert_eq!(size_of::<NvOFFRUCCreateParam>(), 160);
    }

    #[test]
    fn frame_data_layout() {
        assert_eq!(offset_of!(NvOFFRUCFrameData, n_time_stamp), 8);
        assert_eq!(offset_of!(NvOFFRUCFrameData, n_cu_surface_pitch), 16);
        assert_eq!(
            offset_of!(NvOFFRUCFrameData, b_has_frame_repetition_occurred),
            24
        );
        assert_eq!(size_of::<NvOFFRUCFrameData>(), 160);
        assert_eq!(align_of::<NvOFFRUCFrameData>(), 8);
    }

    #[test]
    fn process_params_layout() {
        assert_eq!(offset_of!(NvOFFRUCProcessInParams, flags), 160);
        assert_eq!(offset_of!(NvOFFRUCProcessInParams, u_sync_wait), 168);
        assert_eq!(size_of::<NvOFFRUCProcessInParams>(), 312);
        assert_eq!(offset_of!(NvOFFRUCProcessOutParams, u_sync_signal), 160);
        assert_eq!(size_of::<NvOFFRUCProcessOutParams>(), 304);
    }

    #[test]
    fn register_params_layout() {
        assert_eq!(
            offset_of!(NvOFFRUCRegisterResourceParam, p_d3d11_fence_obj),
            80
        );
        assert_eq!(offset_of!(NvOFFRUCRegisterResourceParam, ui_count), 88);
        assert_eq!(size_of::<NvOFFRUCRegisterResourceParam>(), 96);
        assert_eq!(offset_of!(NvOFFRUCUnregisterResourceParam, ui_count), 80);
        assert_eq!(size_of::<NvOFFRUCUnregisterResourceParam>(), 88);
    }

    #[test]
    fn status_names() {
        assert_eq!(status_name(0), "NvOFFRUC_SUCCESS");
        assert!(status_name(1).contains("NOT_SUPPORTED"));
        assert!(status_name(16).contains("GENERIC"));
        assert_eq!(status_name(99), "unknown NvOFFRUC status");
    }
}
