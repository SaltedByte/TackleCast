//! NVIDIA FRUC library loader and safe wrapper (Windows).
//!
//! NvOFFRUC.dll and its CUDA runtime (cudart64_110.dll) are part of the
//! NVIDIA Optical Flow SDK. The SDK licence does not allow redistributing
//! those binaries, so TackleCast-FrameGen never ships them: it loads them
//! from the user's own SDK folder (or a folder the user points it at).
//!
//! Before loading, both DLLs must carry a valid Authenticode signature from
//! NVIDIA Corporation (the SDK recommends loading only NVIDIA-signed
//! libraries). The DLL is loaded by full path with
//! `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR`, so its cudart dependency resolves from
//! the same verified folder rather than the current directory or PATH.

use super::cuda::CuDevicePtr;
use super::ffi::*;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub const FRUC_DLL: &str = "NvOFFRUC.dll";
pub const FRUC_CUDART_DLL: &str = "cudart64_110.dll";
/// Where the SDK sample keeps the Windows x64 binaries, relative to the SDK root.
pub const SDK_BIN_SUBDIR: &str = r"NvOFFRUC\NvOFFRUCSample\bin\win64";

const LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: u32 = 0x0000_0100;
const LOAD_LIBRARY_SEARCH_DEFAULT_DIRS: u32 = 0x0000_1000;

/// Folders searched for NvOFFRUC.dll, in priority order.
pub fn candidate_dirs(configured: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let configured = configured.trim();
    if !configured.is_empty() {
        let path = PathBuf::from(configured);
        // Accept either the SDK root or the folder holding the DLL.
        dirs.push(path.join(SDK_BIN_SUBDIR));
        dirs.push(path);
    }
    if let Ok(env_dir) = std::env::var("TACKLECAST_FRUC_DIR") {
        if !env_dir.trim().is_empty() {
            dirs.push(PathBuf::from(env_dir.trim()));
        }
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        dirs.push(exe_dir.join("fruc-runtime"));
        dirs.push(exe_dir.clone());
        // An SDK folder next to the app folder or one or two levels up.
        for up in [1_usize, 2, 3] {
            let mut root = exe_dir.clone();
            for _ in 0..up {
                root.pop();
            }
            dirs.push(root.join("Optical_Flow_SDK_5.0.7").join(SDK_BIN_SUBDIR));
        }
    }
    let mut unique = Vec::new();
    for dir in dirs {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    unique
}

pub fn locate(configured: &str) -> Result<PathBuf, String> {
    let dirs = candidate_dirs(configured);
    for dir in &dirs {
        if dir.join(FRUC_DLL).is_file() {
            return Ok(dir.clone());
        }
    }
    let searched: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    Err(format!(
        "{FRUC_DLL} was not found. Set the Frame Generation runtime folder to your NVIDIA \
         Optical Flow SDK 5.0.7 folder. Searched: {}",
        searched.join("; ")
    ))
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Verify the Authenticode signature of `path` and return the signer's name.
pub fn verify_signature(path: &Path) -> Result<String, String> {
    use windows::core::{GUID, PCWSTR};
    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::Security::Cryptography::{
        CertGetNameStringW, CERT_NAME_SIMPLE_DISPLAY_TYPE,
    };
    use windows::Win32::Security::WinTrust::*;

    let path_w = wide(path);
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(path_w.as_ptr()),
        hFile: HANDLE::default(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    // SAFETY: zero is a valid initial value for this plain C struct.
    let mut data: WINTRUST_DATA = unsafe { std::mem::zeroed() };
    data.cbStruct = std::mem::size_of::<WINTRUST_DATA>() as u32;
    data.dwUIChoice = WTD_UI_NONE;
    // Offline-friendly: the chain must be valid, but revocation is not
    // fetched from the network (the app may run with no connectivity).
    data.fdwRevocationChecks = WTD_REVOKE_NONE;
    data.dwProvFlags = WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_NONE;
    data.dwUnionChoice = WTD_CHOICE_FILE;
    data.Anonymous.pFile = &mut file_info;
    data.dwStateAction = WTD_STATEACTION_VERIFY;
    let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;

    // SAFETY: data/file_info/path_w outlive both WinVerifyTrust calls.
    let status = unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            &mut data as *mut _ as *mut c_void,
        )
    };
    let signer = if status == 0 {
        // SAFETY: state data is valid between VERIFY and CLOSE.
        unsafe {
            let provider = WTHelperProvDataFromStateData(data.hWVTStateData);
            let sgnr = if provider.is_null() {
                std::ptr::null_mut()
            } else {
                WTHelperGetProvSignerFromChain(provider, 0, false, 0)
            };
            if sgnr.is_null() || (*sgnr).csCertChain == 0 || (*sgnr).pasCertChain.is_null() {
                None
            } else {
                let cert = (*(*sgnr).pasCertChain).pCert;
                let mut buf = [0_u16; 256];
                let len = CertGetNameStringW(
                    cert,
                    CERT_NAME_SIMPLE_DISPLAY_TYPE,
                    0,
                    None,
                    Some(&mut buf),
                );
                Some(String::from_utf16_lossy(
                    &buf[..(len as usize).saturating_sub(1)],
                ))
            }
        }
    } else {
        None
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: releases the state allocated by the VERIFY call.
    unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            &mut data as *mut _ as *mut c_void,
        );
    }

    if status != 0 {
        return Err(format!(
            "{} has no valid digital signature (WinVerifyTrust 0x{:08X})",
            path.display(),
            status as u32
        ));
    }
    let signer = signer.unwrap_or_default();
    if !signer.to_ascii_lowercase().contains("nvidia") {
        return Err(format!(
            "{} is signed by '{signer}', not NVIDIA Corporation",
            path.display()
        ));
    }
    Ok(signer)
}

/// The loaded FRUC library and its entry points.
pub struct FrucLibrary {
    _lib: libloading::Library,
    pub dir: PathBuf,
    pub signer: String,
    create: PfnCreate,
    register: PfnRegister,
    unregister: PfnUnregister,
    process: PfnProcess,
    destroy: PfnDestroy,
}

impl FrucLibrary {
    pub fn load(configured_dir: &str, require_signature: bool) -> Result<Self, String> {
        // LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR requires an absolute path.
        let dir = locate(configured_dir)?;
        let dir = std::path::absolute(&dir).unwrap_or(dir);
        let dll = dir.join(FRUC_DLL);
        let cudart = dir.join(FRUC_CUDART_DLL);
        if !cudart.is_file() {
            return Err(format!(
                "{FRUC_CUDART_DLL} is missing next to {} (it is shipped with the SDK sample binaries)",
                dll.display()
            ));
        }
        let mut signer = String::from("not checked");
        for file in [&dll, &cudart] {
            match verify_signature(file) {
                Ok(name) => signer = name,
                Err(e) if require_signature => return Err(e),
                Err(e) => warn!("FRUC signature check skipped by settings: {e}"),
            }
        }

        // SAFETY: loading a verified NVIDIA library by absolute path; the
        // flags restrict dependency search to its folder and system folders.
        let lib = unsafe {
            libloading::os::windows::Library::load_with_flags(
                &dll,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        }
        .map_err(|e| {
            format!(
                "{} could not be loaded ({e}). If the error mentions a missing module, install \
                 the Microsoft Visual C++ 2015-2022 x64 Redistributable",
                dll.display()
            )
        })?;
        let lib: libloading::Library = lib.into();

        // SAFETY: symbol types match NvOFFRUC.h (see ffi.rs layout tests).
        unsafe {
            let get = |name: &[u8]| -> Result<*const c_void, String> {
                lib.get::<*const c_void>(name)
                    .map(|s| *s)
                    .map_err(|e| format!("{FRUC_DLL} is missing an export: {e}"))
            };
            let create = std::mem::transmute::<*const c_void, PfnCreate>(get(CREATE_PROC)?);
            let register = std::mem::transmute::<*const c_void, PfnRegister>(get(REGISTER_PROC)?);
            let unregister =
                std::mem::transmute::<*const c_void, PfnUnregister>(get(UNREGISTER_PROC)?);
            let process = std::mem::transmute::<*const c_void, PfnProcess>(get(PROCESS_PROC)?);
            let destroy = std::mem::transmute::<*const c_void, PfnDestroy>(get(DESTROY_PROC)?);
            info!("loaded {} (signed by {signer})", dll.display());
            Ok(Self {
                _lib: lib,
                dir,
                signer,
                create,
                register,
                unregister,
                process,
                destroy,
            })
        }
    }
}

fn check(what: &str, status: NvOFFRUCStatus) -> Result<(), String> {
    if status == NVOFFRUC_SUCCESS {
        Ok(())
    } else {
        Err(format!("{what} failed: {} ({status})", status_name(status)))
    }
}

/// One FRUC instance for RGBA8 CUDA device-pointer surfaces.
///
/// The registered `CUdeviceptr` values live in a boxed array whose addresses
/// stay fixed: FRUC identifies CUDA resources by the address of the
/// registered pointer, so `process` passes those same addresses.
pub struct FrucSession<'lib> {
    lib: &'lib FrucLibrary,
    handle: NvOFFRUCHandle,
    resources: Box<[CuDevicePtr; NVOFFRUC_MAX_RESOURCE]>,
    count: usize,
    registered: bool,
    pitch: usize,
}

pub struct ProcessOutcome {
    /// FRUC returned the previous frame because the interpolated one did
    /// not meet its quality bar.
    pub repeated: bool,
}

impl<'lib> FrucSession<'lib> {
    /// `resources` are device pointers to RGBA8 surfaces of `pitch * height`
    /// bytes. `pitch == width * 4` is passed as 0 (packed), matching the SDK.
    pub fn new(
        lib: &'lib FrucLibrary,
        width: u32,
        height: u32,
        pitch: usize,
        resources: &[CuDevicePtr],
    ) -> Result<Self, String> {
        if resources.len() < NVOFFRUC_MIN_RESOURCE || resources.len() > NVOFFRUC_MAX_RESOURCE {
            return Err(format!(
                "FRUC needs {NVOFFRUC_MIN_RESOURCE}..={NVOFFRUC_MAX_RESOURCE} resources, got {}",
                resources.len()
            ));
        }
        let mut create = NvOFFRUCCreateParam::zeroed();
        create.ui_width = width;
        create.ui_height = height;
        create.p_device = std::ptr::null_mut();
        create.e_resource_type = RESOURCE_TYPE_CUDA;
        create.e_surface_format = SURFACE_ARGB;
        create.e_cuda_resource_type = CUDA_RESOURCE_CU_DEVICE_PTR;
        let mut handle: NvOFFRUCHandle = std::ptr::null_mut();
        // SAFETY: create params are fully initialised; handle is an out-ptr.
        check("NvOFFRUCCreate", unsafe {
            (lib.create)(&create, &mut handle)
        })?;
        if handle.is_null() {
            return Err("NvOFFRUCCreate returned a null handle".into());
        }

        let mut boxed = Box::new([0 as CuDevicePtr; NVOFFRUC_MAX_RESOURCE]);
        boxed[..resources.len()].copy_from_slice(resources);
        let mut session = Self {
            lib,
            handle,
            resources: boxed,
            count: resources.len(),
            registered: false,
            pitch: if pitch == width as usize * 4 {
                0
            } else {
                pitch
            },
        };
        let mut register = NvOFFRUCRegisterResourceParam::zeroed();
        for i in 0..session.count {
            register.p_arr_resource[i] = session.resource_ptr(i);
        }
        register.ui_count = session.count as u32;
        // SAFETY: handle is live; the pointer array addresses stay valid
        // until unregister (they point into the boxed array).
        check("NvOFFRUCRegisterResource", unsafe {
            (lib.register)(session.handle, &register)
        })?;
        session.registered = true;
        Ok(session)
    }

    fn resource_ptr(&mut self, index: usize) -> *mut c_void {
        (&mut self.resources[index]) as *mut CuDevicePtr as *mut c_void
    }

    /// Feed resource `input` (timestamp `in_ts`) and write the frame at
    /// `out_ts` into resource `output`. With `skip_warp`, only the internal
    /// state is updated (used after discontinuities).
    pub fn process(
        &mut self,
        input: usize,
        output: usize,
        in_ts: f64,
        out_ts: f64,
        skip_warp: bool,
    ) -> Result<ProcessOutcome, String> {
        if input >= self.count || output >= self.count || input == output {
            return Err(format!("invalid FRUC resource indices {input}/{output}"));
        }
        let mut repeated = false;
        let mut in_params = NvOFFRUCProcessInParams::zeroed();
        in_params.st_frame_data_input.p_frame = self.resource_ptr(input);
        in_params.st_frame_data_input.n_time_stamp = in_ts;
        in_params.st_frame_data_input.n_cu_surface_pitch = self.pitch;
        in_params.flags = if skip_warp { SKIP_WARP_BIT } else { 0 };
        let mut out_params = NvOFFRUCProcessOutParams::zeroed();
        out_params.st_frame_data_output.p_frame = self.resource_ptr(output);
        out_params.st_frame_data_output.n_time_stamp = out_ts;
        out_params.st_frame_data_output.n_cu_surface_pitch = self.pitch;
        out_params
            .st_frame_data_output
            .b_has_frame_repetition_occurred = &mut repeated;
        // SAFETY: registered resources, valid params; `repeated` outlives the
        // call (the CUDA path is synchronous).
        check("NvOFFRUCProcess", unsafe {
            (self.lib.process)(self.handle, &in_params, &out_params)
        })?;
        Ok(ProcessOutcome { repeated })
    }
}

impl Drop for FrucSession<'_> {
    fn drop(&mut self) {
        // SAFETY: tears down what new() created, in reverse order.
        unsafe {
            if self.registered {
                let mut unregister = NvOFFRUCUnregisterResourceParam::zeroed();
                for i in 0..self.count {
                    unregister.p_arr_resource[i] = self.resource_ptr(i);
                }
                unregister.ui_count = self.count as u32;
                let status = (self.lib.unregister)(self.handle, &unregister);
                if status != NVOFFRUC_SUCCESS {
                    warn!("NvOFFRUCUnregisterResource: {}", status_name(status));
                }
            }
            let status = (self.lib.destroy)(self.handle);
            if status != NVOFFRUC_SUCCESS {
                warn!("NvOFFRUCDestroy: {}", status_name(status));
            }
        }
    }
}
