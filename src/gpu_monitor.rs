#![cfg(all(target_os = "windows", feature = "gpu-decode"))]

use libloading::{Library, Symbol};
use std::ffi::CStr;
use tracing::{info, warn};

type NvmlReturn = u32;
const NVML_SUCCESS: NvmlReturn = 0;
const NVML_TEMPERATURE_GPU: u32 = 0;

#[repr(C)]
#[derive(Default)]
pub struct NvmlUtilization {
    pub gpu: u32,
    pub memory: u32,
}

pub struct GpuMonitor {
    _lib: Library,
    device: usize, // nvmlDevice_t is an opaque pointer
    get_temp: unsafe extern "system" fn(usize, u32, *mut u32) -> NvmlReturn,
    get_util: unsafe extern "system" fn(usize, *mut NvmlUtilization) -> NvmlReturn,
}

impl GpuMonitor {
    pub fn try_new() -> Option<Self> {
        unsafe {
            let lib = Library::new("nvml.dll")
                .or_else(|_| {
                    // Try common driver paths
                    Library::new("C:\\Windows\\System32\\nvml.dll")
                })
                .ok()?;

            let init: Symbol<unsafe extern "system" fn() -> NvmlReturn> =
                lib.get(b"nvmlInit_v2\0").ok()?;
            if init() != NVML_SUCCESS {
                warn!("nvmlInit_v2 failed");
                return None;
            }

            let get_handle: Symbol<unsafe extern "system" fn(u32, *mut usize) -> NvmlReturn> =
                lib.get(b"nvmlDeviceGetHandleByIndex_v2\0").ok()?;
            let mut device: usize = 0;
            if get_handle(0, &mut device) != NVML_SUCCESS {
                warn!("nvmlDeviceGetHandleByIndex failed");
                return None;
            }

            // Log GPU name
            let get_name: Result<
                Symbol<unsafe extern "system" fn(usize, *mut u8, u32) -> NvmlReturn>,
                _,
            > = lib.get(b"nvmlDeviceGetName\0");
            if let Ok(get_name) = get_name {
                let mut name_buf = [0u8; 128];
                if get_name(device, name_buf.as_mut_ptr(), 128) == NVML_SUCCESS {
                    if let Ok(name) = CStr::from_ptr(name_buf.as_ptr() as *const _).to_str() {
                        info!("NVML GPU: {}", name);
                    }
                }
            }

            let get_temp_fn: unsafe extern "system" fn(usize, u32, *mut u32) -> NvmlReturn =
                *lib.get(b"nvmlDeviceGetTemperature\0").ok()?;
            let get_util_fn: unsafe extern "system" fn(usize, *mut NvmlUtilization) -> NvmlReturn =
                *lib.get(b"nvmlDeviceGetUtilizationRates\0").ok()?;

            // Read initial values to verify they work
            let mut temp: u32 = 0;
            if (get_temp_fn)(device, NVML_TEMPERATURE_GPU, &mut temp) != NVML_SUCCESS {
                warn!("nvmlDeviceGetTemperature failed");
                return None;
            }

            info!("NVML initialized: GPU temp={}C", temp);

            Some(Self {
                _lib: lib,
                device,
                get_temp: get_temp_fn,
                get_util: get_util_fn,
            })
        }
    }

    pub fn snapshot(&self) -> GpuSnapshot {
        unsafe {
            let mut temp: u32 = 0;
            let mut util = NvmlUtilization::default();

            let temp_ok =
                (self.get_temp)(self.device, NVML_TEMPERATURE_GPU, &mut temp) == NVML_SUCCESS;
            let util_ok = (self.get_util)(self.device, &mut util) == NVML_SUCCESS;

            GpuSnapshot {
                temp_c: if temp_ok { Some(temp) } else { None },
                gpu_util: if util_ok { Some(util.gpu) } else { None },
                mem_util: if util_ok { Some(util.memory) } else { None },
            }
        }
    }
}

pub struct GpuSnapshot {
    pub temp_c: Option<u32>,
    pub gpu_util: Option<u32>,
    pub mem_util: Option<u32>,
}

impl std::fmt::Display for GpuSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GPU")?;
        if let Some(temp) = self.temp_c {
            write!(f, " {}C", temp)?;
        }
        if let Some(util) = self.gpu_util {
            write!(f, " {}% util", util)?;
        }
        if let Some(mem) = self.mem_util {
            write!(f, " {}% mem", mem)?;
        }
        Ok(())
    }
}
