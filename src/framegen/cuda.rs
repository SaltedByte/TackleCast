//! Minimal CUDA driver API loader for frame generation (Windows).
//!
//! Loaded dynamically from the NVIDIA driver (`nvcuda.dll`), so no CUDA SDK
//! is needed to build. Everything runs on the frame-generation worker thread
//! with the device's *primary* context current, which is also the context the
//! CUDA runtime inside NvOFFRUC.dll uses.

use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::Arc;

pub type CuResult = i32;
pub type CuDevice = i32;
pub type CuContext = *mut c_void;
pub type CuDevicePtr = u64;
pub type CuExternalMemory = *mut c_void;

pub const CUDA_SUCCESS: CuResult = 0;
const CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR: i32 = 75;
const CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR: i32 = 76;
const CU_EXTERNAL_MEMORY_HANDLE_TYPE_D3D12_RESOURCE: i32 = 0x02;
const CUDA_EXTERNAL_MEMORY_DEDICATED: u32 = 0x01;

#[repr(C)]
pub struct ExternalMemoryHandleDesc {
    typ: i32,
    handle_win32_handle: *mut c_void,
    handle_win32_name: *const c_void,
    size: u64,
    flags: u32,
    reserved: [u32; 16],
}

#[repr(C)]
pub struct ExternalMemoryBufferDesc {
    offset: u64,
    size: u64,
    flags: u32,
    reserved: [u32; 16],
}

macro_rules! cuda_fns {
    ($($field:ident : $sym:literal => fn($($arg:ty),*);)*) => {
        pub struct CudaApi {
            _lib: libloading::Library,
            $(pub $field: unsafe extern "system" fn($($arg),*) -> CuResult,)*
            get_error_name: Option<unsafe extern "system" fn(CuResult, *mut *const c_char) -> CuResult>,
            device_get_luid: Option<unsafe extern "system" fn(*mut c_char, *mut u32, CuDevice) -> CuResult>,
        }

        impl CudaApi {
            pub fn load() -> Result<Self, String> {
                // SAFETY: nvcuda.dll is the NVIDIA driver's CUDA library; its
                // exported symbols have the signatures declared here.
                unsafe {
                    let lib = libloading::Library::new("nvcuda.dll").map_err(|e| {
                        format!("nvcuda.dll could not be loaded ({e}); an NVIDIA display driver is required")
                    })?;
                    $(
                        let $field = *lib
                            .get::<unsafe extern "system" fn($($arg),*) -> CuResult>($sym)
                            .map_err(|e| format!("nvcuda.dll lacks {}: {e}", String::from_utf8_lossy(&$sym[..$sym.len() - 1])))?;
                    )*
                    let get_error_name = lib.get(b"cuGetErrorName\0").ok().map(|s| *s);
                    let device_get_luid = lib.get(b"cuDeviceGetLuid\0").ok().map(|s| *s);
                    Ok(Self { _lib: lib, $($field,)* get_error_name, device_get_luid })
                }
            }
        }
    };
}

cuda_fns! {
    init: b"cuInit\0" => fn(u32);
    driver_get_version: b"cuDriverGetVersion\0" => fn(*mut i32);
    device_get_count: b"cuDeviceGetCount\0" => fn(*mut i32);
    device_get: b"cuDeviceGet\0" => fn(*mut CuDevice, i32);
    device_get_name: b"cuDeviceGetName\0" => fn(*mut c_char, i32, CuDevice);
    device_get_attribute: b"cuDeviceGetAttribute\0" => fn(*mut i32, i32, CuDevice);
    primary_ctx_retain: b"cuDevicePrimaryCtxRetain\0" => fn(*mut CuContext, CuDevice);
    primary_ctx_release: b"cuDevicePrimaryCtxRelease_v2\0" => fn(CuDevice);
    ctx_set_current: b"cuCtxSetCurrent\0" => fn(CuContext);
    ctx_synchronize: b"cuCtxSynchronize\0" => fn();
    mem_alloc: b"cuMemAlloc_v2\0" => fn(*mut CuDevicePtr, usize);
    mem_free: b"cuMemFree_v2\0" => fn(CuDevicePtr);
    memcpy_htod: b"cuMemcpyHtoD_v2\0" => fn(CuDevicePtr, *const c_void, usize);
    memcpy_dtoh: b"cuMemcpyDtoH_v2\0" => fn(*mut c_void, CuDevicePtr, usize);
    memset_d8: b"cuMemsetD8_v2\0" => fn(CuDevicePtr, u8, usize);
    import_external_memory: b"cuImportExternalMemory\0" => fn(*mut CuExternalMemory, *const ExternalMemoryHandleDesc);
    external_memory_get_mapped_buffer: b"cuExternalMemoryGetMappedBuffer\0" => fn(*mut CuDevicePtr, CuExternalMemory, *const ExternalMemoryBufferDesc);
    destroy_external_memory: b"cuDestroyExternalMemory\0" => fn(CuExternalMemory);
}

impl CudaApi {
    pub fn error_name(&self, code: CuResult) -> String {
        if let Some(get) = self.get_error_name {
            let mut name: *const c_char = ptr::null();
            // SAFETY: cuGetErrorName writes a pointer to a static string.
            if unsafe { get(code, &mut name) } == CUDA_SUCCESS && !name.is_null() {
                // SAFETY: CUDA returns a NUL-terminated static string.
                return unsafe { CStr::from_ptr(name) }
                    .to_string_lossy()
                    .into_owned();
            }
        }
        format!("CUDA error {code}")
    }

    pub fn check(&self, what: &str, code: CuResult) -> Result<(), String> {
        if code == CUDA_SUCCESS {
            Ok(())
        } else {
            Err(format!("{what} failed: {}", self.error_name(code)))
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub ordinal: i32,
    pub name: String,
    pub compute_major: i32,
    pub compute_minor: i32,
    pub driver_version: i32,
    pub luid: Option<[u8; 8]>,
}

impl DeviceInfo {
    pub fn driver_version_label(&self) -> String {
        format!(
            "CUDA driver API {}.{}",
            self.driver_version / 1000,
            (self.driver_version % 1000) / 10
        )
    }

    pub fn compute_label(&self) -> String {
        format!("sm_{}{}", self.compute_major, self.compute_minor)
    }

    pub fn identity_label(&self) -> String {
        match self.luid {
            Some(luid) => format!(
                "CUDA device {} (LUID {})",
                self.ordinal,
                luid.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ),
            None => format!("CUDA device {}", self.ordinal),
        }
    }
}

/// A CUDA device with its primary context retained and current on the
/// calling thread. Released on drop.
pub struct CudaDevice {
    pub api: Arc<CudaApi>,
    pub info: DeviceInfo,
    device: CuDevice,
    context: CuContext,
}

impl CudaDevice {
    /// Open the CUDA device matching `adapter_luid` (the DXGI adapter wgpu
    /// renders on), or device 0 when no LUID is given.
    pub fn open(adapter_luid: Option<[u8; 8]>) -> Result<Self, String> {
        let api = Arc::new(CudaApi::load()?);
        // SAFETY: plain CUDA driver calls with valid out-pointers.
        unsafe {
            api.check("cuInit", (api.init)(0))?;
            let mut driver_version = 0;
            api.check(
                "cuDriverGetVersion",
                (api.driver_get_version)(&mut driver_version),
            )?;
            let mut count = 0;
            api.check("cuDeviceGetCount", (api.device_get_count)(&mut count))?;
            if count == 0 {
                return Err("no CUDA-capable NVIDIA GPU found".into());
            }

            let mut chosen: Option<(CuDevice, i32, Option<[u8; 8]>)> = None;
            for ordinal in 0..count {
                let mut device = 0;
                api.check("cuDeviceGet", (api.device_get)(&mut device, ordinal))?;
                let luid = api.device_get_luid.and_then(|get| {
                    let mut bytes = [0_i8; 8];
                    let mut mask = 0_u32;
                    (get(bytes.as_mut_ptr() as *mut c_char, &mut mask, device) == CUDA_SUCCESS)
                        .then(|| bytes.map(|b| b as u8))
                });
                let matches = match (adapter_luid, luid) {
                    (Some(want), Some(have)) => want == have,
                    (None, _) => ordinal == 0,
                    (Some(_), None) => ordinal == 0,
                };
                if matches {
                    chosen = Some((device, ordinal, luid));
                    break;
                }
            }
            let Some((device, ordinal, luid)) = chosen else {
                return Err(
                    "the renderer is not running on an NVIDIA GPU (no CUDA device matches the \
                     DirectX 12 adapter). Connect the monitor to the NVIDIA card or select it as \
                     the high-performance GPU for TackleCast in Windows graphics settings"
                        .into(),
                );
            };

            let mut name_buf = [0 as c_char; 128];
            api.check(
                "cuDeviceGetName",
                (api.device_get_name)(name_buf.as_mut_ptr(), name_buf.len() as i32, device),
            )?;
            let name = CStr::from_ptr(name_buf.as_ptr())
                .to_string_lossy()
                .into_owned();
            let mut major = 0;
            let mut minor = 0;
            api.check(
                "cuDeviceGetAttribute",
                (api.device_get_attribute)(
                    &mut major,
                    CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR,
                    device,
                ),
            )?;
            api.check(
                "cuDeviceGetAttribute",
                (api.device_get_attribute)(
                    &mut minor,
                    CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR,
                    device,
                ),
            )?;

            let mut context: CuContext = ptr::null_mut();
            api.check(
                "cuDevicePrimaryCtxRetain",
                (api.primary_ctx_retain)(&mut context, device),
            )?;
            let result = (api.ctx_set_current)(context);
            if result != CUDA_SUCCESS {
                (api.primary_ctx_release)(device);
                return Err(format!(
                    "cuCtxSetCurrent failed: {}",
                    api.error_name(result)
                ));
            }

            Ok(Self {
                api,
                info: DeviceInfo {
                    ordinal,
                    name,
                    compute_major: major,
                    compute_minor: minor,
                    driver_version,
                    luid,
                },
                device,
                context,
            })
        }
    }

    pub fn make_current(&self) -> Result<(), String> {
        // SAFETY: the context is retained for the lifetime of self.
        self.api.check("cuCtxSetCurrent", unsafe {
            (self.api.ctx_set_current)(self.context)
        })
    }

    pub fn synchronize(&self) -> Result<(), String> {
        // SAFETY: plain driver call on the current context.
        self.api
            .check("cuCtxSynchronize", unsafe { (self.api.ctx_synchronize)() })
    }

    /// Import a D3D12 committed resource shared via an NT handle. The handle
    /// stays owned by the caller (CUDA duplicates its reference).
    pub fn import_d3d12_buffer(
        &self,
        handle: *mut c_void,
        size: u64,
    ) -> Result<ImportedBuffer, String> {
        let desc = ExternalMemoryHandleDesc {
            typ: CU_EXTERNAL_MEMORY_HANDLE_TYPE_D3D12_RESOURCE,
            handle_win32_handle: handle,
            handle_win32_name: ptr::null(),
            size,
            flags: CUDA_EXTERNAL_MEMORY_DEDICATED,
            reserved: [0; 16],
        };
        let mut memory: CuExternalMemory = ptr::null_mut();
        // SAFETY: desc is valid for the call; memory is an out-pointer.
        unsafe {
            self.api.check(
                "cuImportExternalMemory",
                (self.api.import_external_memory)(&mut memory, &desc),
            )?;
            let buffer_desc = ExternalMemoryBufferDesc {
                offset: 0,
                size,
                flags: 0,
                reserved: [0; 16],
            };
            let mut device_ptr: CuDevicePtr = 0;
            let result =
                (self.api.external_memory_get_mapped_buffer)(&mut device_ptr, memory, &buffer_desc);
            if result != CUDA_SUCCESS {
                (self.api.destroy_external_memory)(memory);
                return Err(format!(
                    "cuExternalMemoryGetMappedBuffer failed: {}",
                    self.api.error_name(result)
                ));
            }
            Ok(ImportedBuffer {
                api: self.api.clone(),
                memory,
                ptr: device_ptr,
            })
        }
    }

    pub fn alloc(&self, size: usize) -> Result<OwnedBuffer, String> {
        let mut device_ptr: CuDevicePtr = 0;
        // SAFETY: out-pointer is valid.
        self.api.check("cuMemAlloc", unsafe {
            (self.api.mem_alloc)(&mut device_ptr, size)
        })?;
        Ok(OwnedBuffer {
            api: self.api.clone(),
            ptr: device_ptr,
        })
    }

    pub fn upload(&self, dst: CuDevicePtr, data: &[u8]) -> Result<(), String> {
        // SAFETY: data is a valid host slice; dst must hold data.len() bytes.
        self.api.check("cuMemcpyHtoD", unsafe {
            (self.api.memcpy_htod)(dst, data.as_ptr() as *const c_void, data.len())
        })
    }

    pub fn download(&self, src: CuDevicePtr, data: &mut [u8]) -> Result<(), String> {
        // SAFETY: data is a valid host slice; src must hold data.len() bytes.
        self.api.check("cuMemcpyDtoH", unsafe {
            (self.api.memcpy_dtoh)(data.as_mut_ptr() as *mut c_void, src, data.len())
        })
    }

    pub fn fill(&self, dst: CuDevicePtr, value: u8, size: usize) -> Result<(), String> {
        // SAFETY: dst must hold `size` bytes.
        self.api.check("cuMemsetD8", unsafe {
            (self.api.memset_d8)(dst, value, size)
        })
    }
}

impl Drop for CudaDevice {
    fn drop(&mut self) {
        // SAFETY: balances the retain in open().
        unsafe {
            (self.api.primary_ctx_release)(self.device);
        }
    }
}

/// A D3D12 buffer mapped into CUDA. Unmapped and released on drop; drop
/// these before the owning [`CudaDevice`].
pub struct ImportedBuffer {
    api: Arc<CudaApi>,
    memory: CuExternalMemory,
    pub ptr: CuDevicePtr,
}

impl Drop for ImportedBuffer {
    fn drop(&mut self) {
        // SAFETY: the mapping and memory object were created in import.
        unsafe {
            (self.api.mem_free)(self.ptr);
            (self.api.destroy_external_memory)(self.memory);
        }
    }
}

/// A plain `cuMemAlloc` buffer (used by the self-test).
pub struct OwnedBuffer {
    api: Arc<CudaApi>,
    pub ptr: CuDevicePtr,
}

impl Drop for OwnedBuffer {
    fn drop(&mut self) {
        // SAFETY: allocated by cuMemAlloc in alloc().
        unsafe {
            (self.api.mem_free)(self.ptr);
        }
    }
}
