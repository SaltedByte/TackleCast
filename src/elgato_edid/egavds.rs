//! Thin wrapper over Elgato's `EGAVDeviceSupport.dll`, the native library
//! Elgato Studio uses to talk to its capture cards.
//!
//! The DLL exposes SWIG-generated C entry points named
//! `CSharp_ElgatofEGAVDeviceSupport_<name>___`. Conventions that matter here:
//! * objects are opaque pointers created with `new_X` and freed with `delete_X`;
//! * `string` arguments are ANSI `char*`;
//! * returned strings come back through a callback the host registers with
//!   `SWIGRegisterStringCallback_EGAVDS` (we hand back a heap copy and reclaim it);
//! * C++ exceptions are reported through registered exception callbacks;
//! * `bool` crosses the boundary as a 32-bit unsigned int.

use super::win;
use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr, CString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn result_name(r: i32) -> &'static str {
    match r {
        2 => "OKAsync",
        1 => "OkButIncomplete",
        0 => "OK",
        -2 => "ErrInvalidState",
        -3 => "ErrNotAvailable",
        -4 => "ErrInvalidParameter",
        -5 => "ErrNotSupported",
        -6 => "ErrIncompatibleEdid",
        -7 => "ErrReadOnly",
        -8 => "ErrAutoModeActive",
        -9 => "ErrAutoModeRequired",
        -10 => "ErrReconnecting",
        _ => "ErrUnknown",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdidType {
    Default,
    Custom,
    User,
    Passthrough,
    Other(i32),
}

impl EdidType {
    fn from(v: i32) -> Self {
        match v {
            0 => Self::Default,
            1 => Self::Custom,
            2 => Self::User,
            3 => Self::Passthrough,
            v => Self::Other(v),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Custom => "custom",
            Self::User => "user file",
            Self::Passthrough => "pass-through monitor",
            Self::Other(_) => "other",
        }
    }
}

pub fn edid_mode_name(m: i32) -> &'static str {
    match m {
        0 => "Internal",
        1 => "Display (pass-through monitor)",
        2 => "Merged",
        _ => "Unknown",
    }
}

#[derive(Clone, Debug)]
pub struct EdidInfo {
    pub name: String,
    pub file_path: String,
    pub edid_type: EdidType,
    pub sha256: String,
    /// The manifest lists this EDID but its file has not been downloaded yet.
    pub download_required: bool,
}

// ---------------------------------------------------------------------------
// Callbacks
// ---------------------------------------------------------------------------

static LOG_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

thread_local! {
    static PENDING_EXCEPTION: RefCell<Option<String>> = const { RefCell::new(None) };
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

extern "system" fn string_callback(s: *const c_char) -> *mut c_char {
    let owned = unsafe {
        if s.is_null() {
            CString::default()
        } else {
            CStr::from_ptr(s).to_owned()
        }
    };
    owned.into_raw()
}

extern "system" fn exception_callback(msg: *const c_char) {
    let m = unsafe { cstr(msg) };
    PENDING_EXCEPTION.with(|p| *p.borrow_mut() = Some(m));
}

extern "system" fn exception_arg_callback(msg: *const c_char, param: *const c_char) {
    let m = unsafe { format!("{} (parameter '{}')", cstr(msg), cstr(param)) };
    PENDING_EXCEPTION.with(|p| *p.borrow_mut() = Some(m));
}

extern "system" fn log_callback(_ref_con: *mut c_void, level: i32, text: *const c_char, len: i32) {
    if text.is_null() || len <= 0 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(text as *const u8, len as usize) };
    let line = String::from_utf8_lossy(bytes);
    if let Ok(mut file) = LOG_FILE.lock() {
        if let Some(file) = file.as_mut() {
            let _ = writeln!(file, "[{level}] {}", line.trim_end());
        }
    }
}

extern "system" fn event_callback(_ref_con: *mut c_void, _event: *const c_void) {}

// ---------------------------------------------------------------------------
// Function table
// ---------------------------------------------------------------------------

type P = *mut c_void;

macro_rules! api {
    ($( $field:ident : fn($($arg:ty),*) $(-> $ret:ty)? = $sym:literal; )*) => {
        struct Api { $( $field: unsafe extern "system" fn($($arg),*) $(-> $ret)?, )* }
        impl Api {
            fn load(module: win::Hmodule) -> Result<Self, String> {
                Ok(Self { $( $field: unsafe {
                    let sym = concat!("CSharp_ElgatofEGAVDeviceSupport_", $sym, "___");
                    let address = win::proc_address(module, sym).ok_or_else(|| {
                        format!("This Elgato Studio version is not supported (EGAVDeviceSupport.dll has no {sym})")
                    })?;
                    std::mem::transmute::<*mut c_void, unsafe extern "system" fn($($arg),*) $(-> $ret)?>(address)
                }, )* })
            }
        }
    };
}

api! {
    new_init_params: fn() -> P = "new_EGAVDS_INITIALIZE_PARAMS";
    delete_init_params: fn(P) = "delete_EGAVDS_INITIALIZE_PARAMS";
    ip_app_data_dir: fn(P, *const u8) = "EGAVDS_INITIALIZE_PARAMS_appDataDirectoryPath_set";
    ip_tmp_dir: fn(P, *const u8) = "EGAVDS_INITIALIZE_PARAMS_tmpDirectoryPath_set";
    ip_log_dir: fn(P, *const u8) = "EGAVDS_INITIALIZE_PARAMS_logDirectoryPath_set";
    ip_log_filename: fn(P, *const u8) = "EGAVDS_INITIALIZE_PARAMS_logFilename_set";
    ip_firmware_dir: fn(P, *const u8) = "EGAVDS_INITIALIZE_PARAMS_firmwareDirectoryPath_set";
    ip_edid_dir: fn(P, *const u8) = "EGAVDS_INITIALIZE_PARAMS_edidDirectoryPath_set";
    ip_caps_file: fn(P, *const u8) = "EGAVDS_INITIALIZE_PARAMS_deviceCapabilitiesFilePath_set";
    ip_expect_manifest: fn(P, u32) = "EGAVDS_INITIALIZE_PARAMS_expectManagedAssetManifest_set";
    ip_company_short: fn(P, *const u8) = "EGAVDS_INITIALIZE_PARAMS_companyNameShort_set";
    ip_company_long: fn(P, *const u8) = "EGAVDS_INITIALIZE_PARAMS_companyNameLong_set";
    ip_product_short: fn(P, *const u8) = "EGAVDS_INITIALIZE_PARAMS_productNameShort_set";
    ip_product_long: fn(P, *const u8) = "EGAVDS_INITIALIZE_PARAMS_productNameLong_set";
    ip_log_callback: fn(P, P) = "EGAVDS_INITIALIZE_PARAMS_logCallback_set";
    ip_event_callback: fn(P, P) = "EGAVDS_INITIALIZE_PARAMS_eventCallback_set";
    ip_monitoring_set: fn(P, i32) = "EGAVDS_INITIALIZE_PARAMS_deviceMonitoringCategories_set";
    ip_lifetime_set: fn(P, i32) = "EGAVDS_INITIALIZE_PARAMS_deviceLifetimeManagementCategories_set";
    initialize: fn(P) -> i32 = "EGAVDS_Initialize";
    deinitialize: fn() = "EGAVDS_Deinitialize";
    set_asset_manifest: fn(*const u8) -> i32 = "EGAVDS_SetAssetManifest";

    new_device_handle: fn() -> P = "new_EGAVDS_DEVICE_HANDLE";
    delete_device_handle: fn(P) = "delete_EGAVDS_DEVICE_HANDLE";
    open_device: fn(*const u8, P) -> i32 = "OpenDevice";
    close_device: fn(P) = "CloseDevice";

    supports_edid_functions: fn(P) -> u32 = "SupportsEDIDFunctions";
    supports_edid_mode: fn(P) -> u32 = "SupportsEDIDMode";
    get_edid_mode: fn(P, *mut i32) -> i32 = "GetEDIDMode";
    get_edid: fn(P, P) -> i32 = "GetEDID";
    set_edid: fn(P, *const u8) -> i32 = "SetEDID";
    get_default_edid: fn(P, P) -> i32 = "GetDefaultEDID";
    get_custom_edid_count: fn(P, *mut i32) -> i32 = "GetCustomEDIDCount";
    get_custom_edid: fn(P, i32, P) -> i32 = "GetCustomEDID";

    new_edid: fn() -> P = "new_EGAVDS_EDID";
    delete_edid: fn(P) = "delete_EGAVDS_EDID";
    edid_name: fn(P) -> *mut c_char = "EGAVDS_EDID_name_get";
    edid_file_path: fn(P) -> *mut c_char = "EGAVDS_EDID_filePath_get";
    edid_sha256: fn(P) -> *mut c_char = "EGAVDS_EDID_sha256_get";
    edid_type: fn(P) -> i32 = "EGAVDS_EDID_edidType_get";
    edid_download_required: fn(P) -> u32 = "EGAVDS_EDID_isDownloadRequired_get";
}

/// Take ownership of a string returned through our SWIG string callback.
unsafe fn take_string(p: *mut c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CString::from_raw(p).to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------------------
// Library / device
// ---------------------------------------------------------------------------

pub struct Paths {
    /// Private copy of Studio's native libraries (Windows won't load DLLs
    /// from the protected WindowsApps folder into another process).
    pub lib_dir: PathBuf,
    pub data_dir: PathBuf,
    pub edid_dir: PathBuf,
}

pub struct Library {
    api: Api,
}

fn check(what: &str, r: i32) -> Result<i32, String> {
    if let Some(ex) = PENDING_EXCEPTION.with(|p| p.borrow_mut().take()) {
        return Err(format!("{what}: the Elgato library raised: {ex}"));
    }
    if r >= 0 {
        Ok(r)
    } else {
        Err(format!("{what} failed: {} ({r})", result_name(r)))
    }
}

type RegStr = unsafe extern "system" fn(extern "system" fn(*const c_char) -> *mut c_char);
type Ex = extern "system" fn(*const c_char);
type ExA = extern "system" fn(*const c_char, *const c_char);
type RegEx = unsafe extern "system" fn(Ex, Ex, Ex, Ex, Ex, Ex, Ex, Ex, Ex, Ex, Ex);
type RegExA = unsafe extern "system" fn(ExA, ExA, ExA);

fn required(module: win::Hmodule, name: &str) -> Result<*mut c_void, String> {
    win::proc_address(module, name)
        .ok_or_else(|| format!("This Elgato Studio version is not supported (no {name})"))
}

impl Library {
    pub fn init(paths: &Paths, manifest_json: Option<&[u8]>) -> Result<Self, String> {
        let dll = paths.lib_dir.join("EGAVDeviceSupport.dll");
        let module = win::with_stdout_muted(|| win::load_library_from(&dll))?;
        unsafe {
            let reg_str = std::mem::transmute::<*mut c_void, RegStr>(required(
                module,
                "SWIGRegisterStringCallback_EGAVDS",
            )?);
            let reg_ex = std::mem::transmute::<*mut c_void, RegEx>(required(
                module,
                "SWIGRegisterExceptionCallbacks_EGAVDS",
            )?);
            let reg_ex_a = std::mem::transmute::<*mut c_void, RegExA>(required(
                module,
                "SWIGRegisterExceptionArgumentCallbacks_EGAVDS",
            )?);
            reg_str(string_callback);
            let e = exception_callback as Ex;
            reg_ex(e, e, e, e, e, e, e, e, e, e, e);
            let a = exception_arg_callback as ExA;
            reg_ex_a(a, a, a);
        }
        let api = Api::load(module)?;

        let log_dir = paths.data_dir.join("logs");
        let tmp_dir = paths.data_dir.join("tmp");
        let fw_dir = paths.data_dir.join("firmware");
        for dir in [
            &paths.data_dir,
            &paths.edid_dir,
            &log_dir,
            &tmp_dir,
            &fw_dir,
        ] {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("egavds.log"))
        {
            *LOG_FILE.lock().unwrap() = Some(file);
        }

        let s = |p: &Path| win::ansi(p.as_os_str());
        let t = |v: &str| win::ansi(v);
        unsafe {
            let ip = (api.new_init_params)();
            if ip.is_null() {
                return Err("new_EGAVDS_INITIALIZE_PARAMS returned null".into());
            }
            (api.ip_app_data_dir)(ip, s(&paths.data_dir)?.as_ptr());
            (api.ip_tmp_dir)(ip, s(&tmp_dir)?.as_ptr());
            (api.ip_log_dir)(ip, s(&log_dir)?.as_ptr());
            (api.ip_log_filename)(ip, t("egavds-internal.log")?.as_ptr());
            (api.ip_firmware_dir)(ip, s(&fw_dir)?.as_ptr());
            (api.ip_edid_dir)(ip, s(&paths.edid_dir)?.as_ptr());
            (api.ip_caps_file)(
                ip,
                s(&paths.lib_dir.join("ElgatoDeviceCapabilities.json"))?.as_ptr(),
            );
            (api.ip_expect_manifest)(ip, manifest_json.is_some() as u32);
            (api.ip_company_short)(ip, t("Elgato")?.as_ptr());
            (api.ip_company_long)(ip, t("Elgato")?.as_ptr());
            (api.ip_product_short)(ip, t("TackleCast")?.as_ptr());
            (api.ip_product_long)(ip, t("TackleCast")?.as_ptr());
            (api.ip_log_callback)(ip, log_callback as P);
            (api.ip_event_callback)(ip, event_callback as P);
            // Capture devices only (mask 1). Monitoring and lifetime management
            // let the library rebind the card after it re-enumerates during an
            // EDID write.
            (api.ip_monitoring_set)(ip, 1);
            (api.ip_lifetime_set)(ip, 1);
            let r = (api.initialize)(ip);
            (api.delete_init_params)(ip);
            check("EGAVDS_Initialize", r)?;
        }
        let lib = Self { api };
        if let Some(json) = manifest_json {
            let mut z = json.to_vec();
            z.push(0);
            check("EGAVDS_SetAssetManifest", unsafe {
                (lib.api.set_asset_manifest)(z.as_ptr())
            })?;
        }
        Ok(lib)
    }

    pub fn open(&self, device_path: &str) -> Result<Device<'_>, String> {
        unsafe {
            let h = (self.api.new_device_handle)();
            if h.is_null() {
                return Err("new_EGAVDS_DEVICE_HANDLE returned null".into());
            }
            let r = (self.api.open_device)(win::ansi(device_path)?.as_ptr(), h);
            if let Err(e) = check("OpenDevice", r) {
                (self.api.delete_device_handle)(h);
                return Err(e);
            }
            Ok(Device { lib: self, h })
        }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe { (self.api.deinitialize)() };
        if let Ok(mut file) = LOG_FILE.lock() {
            *file = None;
        }
    }
}

pub struct Device<'a> {
    lib: &'a Library,
    h: P,
}

impl Device<'_> {
    fn api(&self) -> &Api {
        &self.lib.api
    }

    fn read_edid(&self, what: &str, f: impl FnOnce(P) -> i32) -> Result<EdidInfo, String> {
        let a = self.api();
        unsafe {
            let e = (a.new_edid)();
            if e.is_null() {
                return Err("new_EGAVDS_EDID returned null".into());
            }
            let info = check(what, f(e)).map(|_| EdidInfo {
                name: take_string((a.edid_name)(e)),
                file_path: take_string((a.edid_file_path)(e)),
                edid_type: EdidType::from((a.edid_type)(e)),
                sha256: take_string((a.edid_sha256)(e)),
                download_required: (a.edid_download_required)(e) != 0,
            });
            (a.delete_edid)(e);
            info
        }
    }

    pub fn supports_edid(&self) -> bool {
        unsafe { (self.api().supports_edid_functions)(self.h) != 0 }
    }

    pub fn edid_mode(&self) -> Option<i32> {
        unsafe {
            if (self.api().supports_edid_mode)(self.h) == 0 {
                return None;
            }
            let mut m = -1;
            check("GetEDIDMode", (self.api().get_edid_mode)(self.h, &mut m))
                .ok()
                .map(|_| m)
        }
    }

    /// Read the EDID currently stored on the card and identify it.
    pub fn current_edid(&self) -> Result<EdidInfo, String> {
        let h = self.h;
        let f = self.api().get_edid;
        self.read_edid("GetEDID", |e| unsafe { f(h, e) })
    }

    pub fn default_edid(&self) -> Result<EdidInfo, String> {
        let h = self.h;
        let f = self.api().get_default_edid;
        self.read_edid("GetDefaultEDID", |e| unsafe { f(h, e) })
    }

    pub fn custom_edids(&self) -> Result<Vec<EdidInfo>, String> {
        let mut n = 0;
        check("GetCustomEDIDCount", unsafe {
            (self.api().get_custom_edid_count)(self.h, &mut n)
        })?;
        let h = self.h;
        let f = self.api().get_custom_edid;
        (0..n)
            .map(|i| self.read_edid("GetCustomEDID", |e| unsafe { f(h, i, e) }))
            .collect()
    }

    /// Write an EDID file to the card. Blocks while the card re-enumerates.
    pub fn set_edid(&self, file: &Path) -> Result<(), String> {
        let p = win::ansi(file.as_os_str())?;
        check("SetEDID", unsafe {
            (self.api().set_edid)(self.h, p.as_ptr())
        })
        .map(|_| ())
    }
}

impl Drop for Device<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api().close_device)(self.h);
            (self.api().delete_device_handle)(self.h);
        }
    }
}
