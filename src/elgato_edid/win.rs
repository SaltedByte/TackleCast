//! Small Win32 helpers for the Elgato EDID helper process.
//!
//! These calls run only in the `--elgato-edid` child process. Functions outside
//! kernel32 (appmodel, bcrypt) are resolved at runtime.

use std::ffi::{c_void, OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

pub type Hmodule = *mut c_void;

const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;
const CP_ACP: u32 = 0;
const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const INVALID_HANDLE_VALUE: isize = -1;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;

#[repr(C)]
struct ProcessEntry32W {
    dw_size: u32,
    cnt_usage: u32,
    th32_process_id: u32,
    th32_default_heap_id: usize,
    th32_module_id: u32,
    cnt_threads: u32,
    th32_parent_process_id: u32,
    pc_pri_class_base: i32,
    dw_flags: u32,
    sz_exe_file: [u16; 260],
}

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> Hmodule;
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> Hmodule;
    fn GetProcAddress(module: Hmodule, name: *const u8) -> *mut c_void;
    fn SetDllDirectoryW(path: *const u16) -> i32;
    fn GetLastError() -> u32;
    #[allow(clippy::too_many_arguments)]
    fn WideCharToMultiByte(
        cp: u32,
        flags: u32,
        wide: *const u16,
        wide_len: i32,
        out: *mut u8,
        out_len: i32,
        default_char: *const u8,
        used_default: *mut i32,
    ) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
    fn Process32FirstW(snap: isize, entry: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snap: isize, entry: *mut ProcessEntry32W) -> i32;
    fn CloseHandle(h: isize) -> i32;
    fn GetStdHandle(which: u32) -> isize;
    fn SetStdHandle(which: u32, h: isize) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        sa: *mut c_void,
        disposition: u32,
        flags: u32,
        template: isize,
    ) -> isize;
}

/// Run `f` with the process stdout pointed at NUL. Elgato's DLL has a
/// statically linked CRT that captures the std handles when it loads, so its
/// console output stays out of the JSON this process writes to stdout.
pub fn with_stdout_muted<T>(f: impl FnOnce() -> T) -> T {
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    unsafe {
        let original = GetStdHandle(STD_OUTPUT_HANDLE);
        let nul = CreateFileW(
            wide("NUL").as_ptr(),
            GENERIC_WRITE,
            3,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            0,
        );
        if nul == INVALID_HANDLE_VALUE {
            return f();
        }
        SetStdHandle(STD_OUTPUT_HANDLE, nul);
        let result = f();
        SetStdHandle(STD_OUTPUT_HANDLE, original);
        // `nul` stays open on purpose: the DLL keeps writing to it.
        result
    }
}

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide_nul(buf: &[u16]) -> OsString {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..end])
}

/// Convert to the system ANSI code page, NUL-terminated. The DLL's SWIG
/// bindings take `char*` strings in that code page.
pub fn ansi(s: impl AsRef<OsStr>) -> Result<Vec<u8>, String> {
    let w: Vec<u16> = s.as_ref().encode_wide().collect();
    if w.is_empty() {
        return Ok(vec![0]);
    }
    unsafe {
        let mut used_default = 0i32;
        let n = WideCharToMultiByte(
            CP_ACP,
            0,
            w.as_ptr(),
            w.len() as i32,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            &mut used_default,
        );
        if n <= 0 {
            return Err("WideCharToMultiByte failed".into());
        }
        let mut out = vec![0u8; n as usize + 1];
        WideCharToMultiByte(
            CP_ACP,
            0,
            w.as_ptr(),
            w.len() as i32,
            out.as_mut_ptr(),
            n,
            std::ptr::null(),
            &mut used_default,
        );
        if used_default != 0 {
            return Err(format!(
                "'{}' contains characters the Elgato library cannot accept",
                s.as_ref().to_string_lossy()
            ));
        }
        Ok(out)
    }
}

fn load_library(name: &str) -> Option<Hmodule> {
    let w = wide(name);
    let h = unsafe { LoadLibraryW(w.as_ptr()) };
    (!h.is_null()).then_some(h)
}

/// Load a DLL by full path so its own dependencies resolve from its folder.
/// This changes the DLL search path for the whole process, which is one reason
/// it only ever runs in the helper process.
pub fn load_library_from(path: &Path) -> Result<Hmodule, String> {
    if let Some(dir) = path.parent() {
        let wd = wide(dir);
        unsafe { SetDllDirectoryW(wd.as_ptr()) };
    }
    let w = wide(path);
    let h = unsafe {
        LoadLibraryExW(
            w.as_ptr(),
            std::ptr::null_mut(),
            LOAD_WITH_ALTERED_SEARCH_PATH,
        )
    };
    if h.is_null() {
        Err(format!(
            "Cannot load {} (Win32 error {})",
            path.display(),
            unsafe { GetLastError() }
        ))
    } else {
        Ok(h)
    }
}

pub fn proc_address(module: Hmodule, name: &str) -> Option<*mut c_void> {
    let mut n = name.as_bytes().to_vec();
    n.push(0);
    let p = unsafe { GetProcAddress(module, n.as_ptr()) };
    (!p.is_null()).then_some(p)
}

/// Install folder of a packaged (MSIX / Store) app, by package family name.
pub fn package_path_for_family(family: &str) -> Option<PathBuf> {
    type ByFamily =
        unsafe extern "system" fn(*const u16, *mut u32, *mut *mut u16, *mut u32, *mut u16) -> i32;
    type PathByName = unsafe extern "system" fn(*const u16, *mut u32, *mut u16) -> i32;
    let k32 = load_library("kernel32.dll")?;
    let by_family = unsafe {
        std::mem::transmute::<*mut c_void, ByFamily>(proc_address(
            k32,
            "GetPackagesByPackageFamily",
        )?)
    };
    let path_by_name = unsafe {
        std::mem::transmute::<*mut c_void, PathByName>(proc_address(
            k32,
            "GetPackagePathByFullName",
        )?)
    };
    let fam = wide(family);
    unsafe {
        let (mut count, mut buf_len) = (0u32, 0u32);
        let rc = by_family(
            fam.as_ptr(),
            &mut count,
            std::ptr::null_mut(),
            &mut buf_len,
            std::ptr::null_mut(),
        );
        if rc != ERROR_INSUFFICIENT_BUFFER || count == 0 {
            return None;
        }
        let mut names: Vec<*mut u16> = vec![std::ptr::null_mut(); count as usize];
        let mut buf = vec![0u16; buf_len as usize];
        if by_family(
            fam.as_ptr(),
            &mut count,
            names.as_mut_ptr(),
            &mut buf_len,
            buf.as_mut_ptr(),
        ) != 0
        {
            return None;
        }
        // Two versions can be registered briefly during an update; take the last.
        let full_name = *names.last()?;
        let mut len = 0u32;
        path_by_name(full_name, &mut len, std::ptr::null_mut());
        if len == 0 {
            return None;
        }
        let mut path = vec![0u16; len as usize];
        if path_by_name(full_name, &mut len, path.as_mut_ptr()) != 0 {
            return None;
        }
        Some(PathBuf::from(from_wide_nul(&path)))
    }
}

/// SHA-256 via the Windows CNG one-shot API (Windows 10+).
pub fn sha256_hex(data: &[u8]) -> Result<String, String> {
    type HashFn =
        unsafe extern "system" fn(usize, *const u8, u32, *const u8, u32, *mut u8, u32) -> i32;
    const BCRYPT_SHA256_ALG_HANDLE: usize = 0x0000_0041;
    let bcrypt = load_library("bcrypt.dll").ok_or("Cannot load bcrypt.dll")?;
    let hash = unsafe {
        std::mem::transmute::<*mut c_void, HashFn>(
            proc_address(bcrypt, "BCryptHash").ok_or("BCryptHash is unavailable")?,
        )
    };
    let mut out = [0u8; 32];
    let status = unsafe {
        hash(
            BCRYPT_SHA256_ALG_HANDLE,
            std::ptr::null(),
            0,
            data.as_ptr(),
            data.len() as u32,
            out.as_mut_ptr(),
            32,
        )
    };
    if status != 0 {
        return Err(format!("BCryptHash failed (NTSTATUS {status:#x})"));
    }
    Ok(out.iter().map(|b| format!("{b:02X}")).collect())
}

/// Lower-case executable names of running processes.
pub fn running_process_names() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: ProcessEntry32W = std::mem::zeroed();
        entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut entry);
        while ok != 0 {
            out.push(
                from_wide_nul(&entry.sz_exe_file)
                    .to_string_lossy()
                    .to_lowercase(),
            );
            ok = Process32NextW(snap, &mut entry);
        }
        CloseHandle(snap);
    }
    out
}
