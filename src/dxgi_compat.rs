#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::info;
use windows::core::{s, w, GUID, HRESULT};
use windows::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualProtect, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
    PAGE_PROTECTION_FLAGS,
};
use windows::Win32::System::Threading::GetCurrentProcess;

/// GUID for `IDXGIFactoryMedia`: {41E7D1F2-A591-4F7B-A2E5-FA9C843E1C12}
const IID_IDXGI_FACTORY_MEDIA: GUID = GUID::from_u128(0x41e7d1f2_a591_4f7b_a2e5_fa9c843e1c12);

/// HRESULT `E_NOINTERFACE` (0x80004002)
const E_NOINTERFACE: HRESULT = HRESULT(-2147467262);

static TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);

type CreateDXGIFactory1Fn =
    unsafe extern "system" fn(riid: *const GUID, pp_factory: *mut *mut std::ffi::c_void) -> HRESULT;

unsafe extern "system" fn hooked_create_dxgi_factory1(
    riid: *const GUID,
    pp_factory: *mut *mut std::ffi::c_void,
) -> HRESULT {
    if !riid.is_null() && *riid == IID_IDXGI_FACTORY_MEDIA {
        if !pp_factory.is_null() {
            *pp_factory = std::ptr::null_mut();
        }
        // Intercepted IDXGIFactoryMedia! Return E_NOINTERFACE directly.
        // This prevents ReShade from crashing when attempting to wrap an interface
        // that does not inherit from IDXGIFactory1. wgpu-hal treats this as an
        // expected fallback (.ok() -> None) and continues DX12 initialization.
        return E_NOINTERFACE;
    }

    let tramp_ptr = TRAMPOLINE.load(Ordering::Relaxed);
    if tramp_ptr != 0 {
        let original: CreateDXGIFactory1Fn = std::mem::transmute(tramp_ptr);
        original(riid, pp_factory)
    } else {
        E_NOINTERFACE
    }
}

/// Computes the number of instruction bytes to displace from the prologue (>= 14 bytes)
/// without splitting any x86_64 instructions.
fn compute_prologue_len(code: &[u8]) -> Option<usize> {
    let mut offset = 0;
    while offset < 14 && offset < code.len() {
        let mut b = code[offset];
        let has_rex = (0x40..=0x4F).contains(&b);
        if has_rex {
            offset += 1;
            if offset >= code.len() {
                return None;
            }
            b = code[offset];
        }

        match b {
            // push reg (0x50..=0x57)
            0x50..=0x57 => {
                offset += 1;
            }
            // sub rsp, imm8 (0x83 0xEC imm8)
            0x83 if offset + 2 < code.len() && code[offset + 1] == 0xEC => {
                offset += 3;
            }
            // sub rsp, imm32 (0x81 0xEC imm32)
            0x81 if offset + 5 < code.len() && code[offset + 1] == 0xEC => {
                offset += 6;
            }
            // mov [rsp + imm8], reg (0x89 0x5C 0x24 imm8 or 0x89 modrm with SIB)
            0x89 if offset + 3 < code.len() && code[offset + 1] == 0x5C && code[offset + 2] == 0x24 => {
                offset += 4;
            }
            // mov reg, reg or mov reg, [reg] (0x8B modrm)
            0x8B if offset + 1 < code.len() => {
                let modrm = code[offset + 1];
                let mod_bits = modrm >> 6;
                let rm_bits = modrm & 0x07;
                offset += 2;
                if mod_bits != 3 && rm_bits == 4 {
                    // SIB byte
                    offset += 1;
                }
                if mod_bits == 1 {
                    offset += 1; // disp8
                } else if mod_bits == 2 || (mod_bits == 0 && rm_bits == 5) {
                    offset += 4; // disp32
                }
            }
            _ => {
                // Unknown instruction format in prologue; fallback to known patterns
                if code.len() >= 14 && code[0] == 0x41 && code[1] == 0x56 {
                    return Some(14); // ReShade standard prologue
                }
                if code.len() >= 16 && code[0] == 0x48 && code[1] == 0x89 {
                    return Some(16); // System dxgi standard prologue
                }
                return None;
            }
        }
    }

    if offset >= 14 {
        Some(offset)
    } else {
        None
    }
}

/// Ensures that calling `CreateDXGIFactory1` with `IDXGIFactoryMedia` does not crash
/// third-party injectors like ReShade.
pub fn ensure_dxgi_compatibility() {
    static INITIALIZED: std::sync::Once = std::sync::Once::new();
    INITIALIZED.call_once(|| unsafe {
        // Load whatever dxgi.dll the process uses (local proxy if present, or system).
        let h_dxgi = match LoadLibraryW(w!("dxgi.dll")) {
            Ok(h) if !h.is_invalid() => h,
            _ => return,
        };

        let p_proc = GetProcAddress(h_dxgi, s!("CreateDXGIFactory1"));
        let Some(p_proc) = p_proc else {
            return;
        };

        let p_func = p_proc as *mut u8;
        let mut prologue_peek = [0u8; 32];
        std::ptr::copy_nonoverlapping(p_func, prologue_peek.as_mut_ptr(), 32);

        // Check if already hooked (starts with 0xFF 0x25)
        if prologue_peek[0] == 0xFF && prologue_peek[1] == 0x25 {
            return;
        }

        let Some(orig_len) = compute_prologue_len(&prologue_peek) else {
            return;
        };

        // Allocate executable trampoline (orig_len displaced bytes + 14-byte jump back)
        let tramp_size = orig_len + 14;
        let tramp_mem = VirtualAlloc(
            None,
            tramp_size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        );
        if tramp_mem.is_null() {
            return;
        }

        let p_tramp = tramp_mem as *mut u8;
        // 1. Copy original displaced instructions into trampoline
        std::ptr::copy_nonoverlapping(p_func, p_tramp, orig_len);

        // 2. Append absolute indirect jump back to (p_func + orig_len)
        let tramp_jmp = p_tramp.add(orig_len);
        *tramp_jmp.add(0) = 0xFF;
        *tramp_jmp.add(1) = 0x25;
        *tramp_jmp.add(2) = 0x00;
        *tramp_jmp.add(3) = 0x00;
        *tramp_jmp.add(4) = 0x00;
        *tramp_jmp.add(5) = 0x00;
        let ret_addr = (p_func as usize + orig_len) as u64;
        std::ptr::copy_nonoverlapping(
            &ret_addr as *const u64 as *const u8,
            tramp_jmp.add(6),
            8,
        );

        TRAMPOLINE.store(p_tramp as usize, Ordering::Release);

        // 3. Overwrite p_func with jump to hooked_create_dxgi_factory1
        let mut old_protect = PAGE_PROTECTION_FLAGS::default();
        if VirtualProtect(
            p_func as *const std::ffi::c_void,
            orig_len,
            PAGE_EXECUTE_READWRITE,
            &mut old_protect,
        )
        .is_ok()
        {
            *p_func.add(0) = 0xFF;
            *p_func.add(1) = 0x25;
            *p_func.add(2) = 0x00;
            *p_func.add(3) = 0x00;
            *p_func.add(4) = 0x00;
            *p_func.add(5) = 0x00;
            let target_addr = hooked_create_dxgi_factory1 as *const () as usize as u64;
            std::ptr::copy_nonoverlapping(
                &target_addr as *const u64 as *const u8,
                p_func.add(6),
                8,
            );

            // NOP any remaining displaced bytes
            for i in 14..orig_len {
                *p_func.add(i) = 0x90;
            }

            let _ = VirtualProtect(
                p_func as *const std::ffi::c_void,
                orig_len,
                old_protect,
                &mut old_protect,
            );
            let _ = FlushInstructionCache(GetCurrentProcess(), Some(p_func as *const _), orig_len);

            info!("DXGI ReShade compatibility guard active");
        }
    });
}

