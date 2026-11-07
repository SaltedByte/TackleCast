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

#[link(name = "winhttp")]
extern "system" {
    fn WinHttpOpen(
        agent: *const u16,
        access: u32,
        proxy: *const u16,
        bypass: *const u16,
        flags: u32,
    ) -> *mut c_void;
    fn WinHttpConnect(
        session: *mut c_void,
        server: *const u16,
        port: u16,
        reserved: u32,
    ) -> *mut c_void;
    fn WinHttpOpenRequest(
        connect: *mut c_void,
        verb: *const u16,
        object: *const u16,
        version: *const u16,
        referrer: *const u16,
        accept: *const *const u16,
        flags: u32,
    ) -> *mut c_void;
    fn WinHttpSetTimeouts(
        handle: *mut c_void,
        resolve: i32,
        connect: i32,
        send: i32,
        receive: i32,
    ) -> i32;
    fn WinHttpSendRequest(
        request: *mut c_void,
        headers: *const u16,
        headers_len: u32,
        optional: *const c_void,
        optional_len: u32,
        total_len: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: *mut c_void, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: *mut c_void,
        level: u32,
        name: *const u16,
        buffer: *mut c_void,
        len: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpReadData(request: *mut c_void, buffer: *mut c_void, len: u32, read: *mut u32) -> i32;
    fn WinHttpCloseHandle(handle: *mut c_void) -> i32;
}

/// Hosts the Elgato helper may download from: Elgato's update service (the
/// asset manifest) and its CDN (the EDID files the manifest lists).
pub const ALLOWED_HOSTS: &[&str] = &["gc-updates.elgato.com", "edge.elgato.com"];

struct HttpHandle(*mut c_void);
impl Drop for HttpHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// HTTPS GET through WinHTTP (system proxy settings, Windows' TLS), allowed
/// only for [`ALLOWED_HOSTS`]. Fails on a non-200 status or a body larger than
/// `max_bytes`.
pub fn https_get(url: &str, max_bytes: usize, timeout_ms: i32) -> Result<Vec<u8>, String> {
    https_request(url, None, max_bytes, timeout_ms).map(|(body, _)| body)
}

/// HTTPS GET of part of a file. `range` is an HTTP byte range such as
/// `"0-99"` or `"-65536"` (the last 64 KiB). Returns the bytes and the file's
/// total size from `Content-Range`.
pub fn https_get_range(
    url: &str,
    range: &str,
    max_bytes: usize,
    timeout_ms: i32,
) -> Result<(Vec<u8>, u64), String> {
    let (body, total) = https_request(url, Some(range), max_bytes, timeout_ms)?;
    let total = total.ok_or_else(|| format!("{url} ignored the byte range request"))?;
    Ok((body, total))
}

fn https_request(
    url: &str,
    range: Option<&str>,
    max_bytes: usize,
    timeout_ms: i32,
) -> Result<(Vec<u8>, Option<u64>), String> {
    const ACCESS_AUTOMATIC_PROXY: u32 = 4;
    const FLAG_SECURE: u32 = 0x0080_0000;
    const QUERY_STATUS_CODE: u32 = 19;
    const QUERY_CUSTOM: u32 = 65535;
    const QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("Refusing a non-HTTPS download: {url}"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !ALLOWED_HOSTS.contains(&host.to_ascii_lowercase().as_str()) {
        return Err(format!(
            "Refusing a download from {host}: not an Elgato host"
        ));
    }
    let failed = |what: &str| {
        format!("{what} failed for {url} (Win32 error {})", unsafe {
            GetLastError()
        })
    };
    unsafe {
        let agent = wide(concat!("TackleCast/", env!("CARGO_PKG_VERSION")));
        let session = HttpHandle(WinHttpOpen(
            agent.as_ptr(),
            ACCESS_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(failed("WinHttpOpen"));
        }
        WinHttpSetTimeouts(session.0, timeout_ms, timeout_ms, timeout_ms, timeout_ms);
        let connect = HttpHandle(WinHttpConnect(session.0, wide(host).as_ptr(), 443, 0));
        if connect.0.is_null() {
            return Err(failed("Connecting"));
        }
        let object = wide(format!("/{path}"));
        let request = HttpHandle(WinHttpOpenRequest(
            connect.0,
            wide("GET").as_ptr(),
            object.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(failed("Opening the request"));
        }
        let header = range.map(|r| wide(format!("Range: bytes={r}")));
        let (header_ptr, header_len) = match &header {
            // Length -1: the header string is NUL-terminated.
            Some(h) => (h.as_ptr(), u32::MAX),
            None => (std::ptr::null(), 0),
        };
        if WinHttpSendRequest(request.0, header_ptr, header_len, std::ptr::null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0
        {
            return Err(failed("The download"));
        }
        let mut status = 0u32;
        let mut len = std::mem::size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            request.0,
            QUERY_STATUS_CODE | QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&mut status as *mut u32).cast(),
            &mut len,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(failed("Reading the response status"));
        }
        let expected = if range.is_some() { 206 } else { 200 };
        if status != expected {
            return Err(format!("{url} returned HTTP {status}"));
        }
        // "Content-Range: bytes 100-199/12345" carries the file's total size.
        let total = if range.is_some() {
            let name = wide("Content-Range");
            let mut buffer = vec![0u16; 128];
            let mut len = (buffer.len() * 2) as u32;
            if WinHttpQueryHeaders(
                request.0,
                QUERY_CUSTOM,
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(failed("Reading Content-Range"));
            }
            let value = String::from_utf16_lossy(&buffer[..len as usize / 2]);
            value
                .rsplit_once('/')
                .and_then(|(_, total)| total.trim().parse::<u64>().ok())
        } else {
            None
        };
        let mut body = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            if WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            ) == 0
            {
                return Err(failed("Reading the download"));
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > max_bytes {
                return Err(format!("{url} is larger than expected"));
            }
        }
        Ok((body, total))
    }
}

/// The names a DLL exports, read from its PE export table without loading it.
pub fn dll_export_names(path: &Path) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    parse_export_names(&bytes).ok_or_else(|| format!("{} isn't a readable DLL", path.display()))
}

fn parse_export_names(b: &[u8]) -> Option<Vec<String>> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?));
    if b.get(..2)? != b"MZ" {
        return None;
    }
    let pe = u32_at(0x3C)? as usize;
    if b.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let sections = usize::from(u16_at(pe + 6)?);
    let optional_size = usize::from(u16_at(pe + 20)?);
    let optional = pe + 24;
    // PE32+ (0x20B) has the data directories 16 bytes later than PE32.
    let directories = match u16_at(optional)? {
        0x20B => optional + 112,
        0x10B => optional + 96,
        _ => return None,
    };
    let export_rva = u32_at(directories)?;
    if export_rva == 0 {
        return Some(Vec::new());
    }
    let section_table = optional + optional_size;
    let to_offset = |rva: u32| -> Option<usize> {
        (0..sections).find_map(|i| {
            let s = section_table + i * 40;
            let virtual_size = u32_at(s + 8)?;
            let virtual_address = u32_at(s + 12)?;
            let raw_size = u32_at(s + 16)?;
            let raw_pointer = u32_at(s + 20)?;
            let size = virtual_size.max(raw_size);
            (rva >= virtual_address && rva < virtual_address + size)
                .then(|| (rva - virtual_address + raw_pointer) as usize)
        })
    };
    let export = to_offset(export_rva)?;
    let count = u32_at(export + 24)? as usize;
    let names = to_offset(u32_at(export + 32)?)?;
    let mut out = Vec::with_capacity(count);
    for i in 0..count.min(100_000) {
        let name = to_offset(u32_at(names + i * 4)?)?;
        let end = b.get(name..)?.iter().position(|&c| c == 0)?;
        out.push(String::from_utf8_lossy(&b[name..name + end]).into_owned());
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Authenticode
// ---------------------------------------------------------------------------

#[repr(C)]
struct WintrustFileInfo {
    cb_struct: u32,
    file_path: *const u16,
    file: isize,
    known_subject: *const Guid,
}

#[repr(C)]
struct WintrustData {
    cb_struct: u32,
    policy_callback_data: *mut c_void,
    sip_client_data: *mut c_void,
    ui_choice: u32,
    revocation_checks: u32,
    union_choice: u32,
    file: *mut WintrustFileInfo,
    state_action: u32,
    state_data: isize,
    url_reference: *const u16,
    prov_flags: u32,
    ui_context: u32,
    signature_settings: *mut c_void,
}

#[repr(C)]
struct ProviderCert {
    cb_struct: u32,
    cert: *const c_void,
}

#[repr(C)]
struct ProviderSigner {
    cb_struct: u32,
    verify_as_of: [u32; 2],
    cert_count: u32,
    certs: *const ProviderCert,
}

#[repr(C)]
pub struct Guid(u32, u16, u16, [u8; 8]);

#[link(name = "wintrust")]
extern "system" {
    fn WinVerifyTrust(hwnd: isize, action: *const Guid, data: *mut c_void) -> i32;
    fn WTHelperProvDataFromStateData(state: isize) -> *mut c_void;
    fn WTHelperGetProvSignerFromChain(
        provider: *mut c_void,
        signer: u32,
        counter_signer: i32,
        counter_signer_index: u32,
    ) -> *const ProviderSigner;
}

#[link(name = "crypt32")]
extern "system" {
    fn CertGetNameStringW(
        cert: *const c_void,
        kind: u32,
        flags: u32,
        type_para: *const c_void,
        name: *mut u16,
        len: u32,
    ) -> u32;
}

/// Check that `path` carries a valid Authenticode signature that chains to a
/// trusted root, and return the signer's name (e.g. "Corsair Memory, Inc.").
pub fn authenticode_signer(path: &Path) -> Result<String, String> {
    const GENERIC_VERIFY_V2: Guid = Guid(
        0x00AA_C56B,
        0xCD44,
        0x11D0,
        [0x8C, 0xC2, 0x00, 0xC0, 0x4F, 0xC2, 0x95, 0xEE],
    );
    const UI_NONE: u32 = 2;
    const REVOKE_NONE: u32 = 0;
    const CHOICE_FILE: u32 = 1;
    const STATE_VERIFY: u32 = 1;
    const STATE_CLOSE: u32 = 2;
    const NAME_SIMPLE_DISPLAY: u32 = 4;
    let wide_path = wide(path);
    let mut file = WintrustFileInfo {
        cb_struct: std::mem::size_of::<WintrustFileInfo>() as u32,
        file_path: wide_path.as_ptr(),
        file: 0,
        known_subject: std::ptr::null(),
    };
    let mut data = WintrustData {
        cb_struct: std::mem::size_of::<WintrustData>() as u32,
        policy_callback_data: std::ptr::null_mut(),
        sip_client_data: std::ptr::null_mut(),
        ui_choice: UI_NONE,
        revocation_checks: REVOKE_NONE,
        union_choice: CHOICE_FILE,
        file: &mut file,
        state_action: STATE_VERIFY,
        state_data: 0,
        url_reference: std::ptr::null(),
        prov_flags: 0,
        ui_context: 0,
        signature_settings: std::ptr::null_mut(),
    };
    unsafe {
        let status = WinVerifyTrust(
            -1,
            &GENERIC_VERIFY_V2,
            (&mut data as *mut WintrustData).cast(),
        );
        let signer = if status == 0 {
            let provider = WTHelperProvDataFromStateData(data.state_data);
            let signer = if provider.is_null() {
                std::ptr::null()
            } else {
                WTHelperGetProvSignerFromChain(provider, 0, 0, 0)
            };
            if signer.is_null() || (*signer).cert_count == 0 || (*signer).certs.is_null() {
                Err(format!("{} has no readable signer", path.display()))
            } else {
                let cert = (*(*signer).certs).cert;
                let mut name = vec![0u16; 256];
                let len = CertGetNameStringW(
                    cert,
                    NAME_SIMPLE_DISPLAY,
                    0,
                    std::ptr::null(),
                    name.as_mut_ptr(),
                    name.len() as u32,
                );
                Ok(String::from_utf16_lossy(
                    &name[..len.saturating_sub(1) as usize],
                ))
            }
        } else {
            Err(format!(
                "{} is not validly signed (WinVerifyTrust {:#x})",
                path.display(),
                status as u32
            ))
        };
        data.state_action = STATE_CLOSE;
        WinVerifyTrust(
            -1,
            &GENERIC_VERIFY_V2,
            (&mut data as *mut WintrustData).cast(),
        );
        signer
    }
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
