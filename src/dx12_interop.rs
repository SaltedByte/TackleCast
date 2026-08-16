//! DX12 ↔ CUDA shared buffer infrastructure for zero-copy GPU decode.
//!
//! Creates DX12 committed buffers with `D3D12_HEAP_FLAG_SHARED`, exports
//! NT handles for CUDA import, and wraps them as wgpu buffers for
//! `copy_buffer_to_texture` on the render side.
//!
//! Double-buffered: two sets of Y/U/V plane buffers so the CUDA decode
//! thread can write to one while the renderer reads from the other.
//!
//! # Handle Lifecycle
//!
//! NT kernel handles from `CreateSharedHandle` are ephemeral — they exist
//! only to transfer the DX12 resource reference to CUDA via
//! `cuImportExternalMemory`. After CUDA imports successfully, the handles
//! are closed immediately. The `ImportHandles` struct enforces this by
//! closing handles in its `Drop` impl, making leaks impossible by
//! construction.

#![cfg(feature = "gpu-decode")]

use std::ffi::c_void;
use tracing::{debug, info, warn};
use wgpu::hal::api::Dx12;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::PCWSTR;

/// Number of double-buffer sets.
pub const NUM_BUFFER_SETS: usize = 2;

// ---------------------------------------------------------------------------
// Long-lived types (stored in Renderer for the duration of capture)
// ---------------------------------------------------------------------------

/// A set of Y, U, V plane buffers for one frame (wgpu side).
pub struct PlaneBufferSet {
    pub y_buffer: wgpu::Buffer,
    pub u_buffer: wgpu::Buffer,
    pub v_buffer: wgpu::Buffer,
}

/// Buffer layout metadata — describes row pitches and sizes for the shared
/// buffers. Lives as long as the decode session. Contains no kernel resources.
#[derive(Debug, Clone, Copy)]
pub struct SharedBufferLayout {
    /// Row pitch for Y plane (aligned to COPY_BYTES_PER_ROW_ALIGNMENT).
    pub y_pitch: u32,
    /// Row pitch for U/V planes (aligned).
    pub uv_pitch: u32,
    /// Total Y plane buffer size in bytes.
    pub y_size: u64,
    /// Total U (or V) plane buffer size in bytes.
    pub uv_size: u64,
}

/// All shared GPU buffers for the zero-copy pipeline (renderer side).
pub struct SharedGpuBuffers(pub [PlaneBufferSet; NUM_BUFFER_SETS]);

// ---------------------------------------------------------------------------
// Ephemeral types (consumed during CUDA import, then dropped)
// ---------------------------------------------------------------------------

/// NT handles for one set of Y/U/V plane buffers. These are consumed by
/// `cuImportExternalMemory` and then closed. The `Drop` impl guarantees
/// handles are closed even if the CUDA import fails partway through.
pub struct ImportHandleSet {
    pub y_handle: *mut c_void,
    pub u_handle: *mut c_void,
    pub v_handle: *mut c_void,
    pub y_size: u64,
    pub u_size: u64,
    pub v_size: u64,
}

impl Drop for ImportHandleSet {
    fn drop(&mut self) {
        unsafe {
            close_handle_if_valid(&mut self.y_handle);
            close_handle_if_valid(&mut self.u_handle);
            close_handle_if_valid(&mut self.v_handle);
        }
    }
}

// SAFETY: NT handles are opaque pointers, safe to send across threads.
unsafe impl Send for ImportHandleSet {}

impl ImportHandleSet {
    /// Take a handle out, returning its value and nulling the slot so Drop
    /// won't close it. Use this after a successful CUDA import to transfer
    /// ownership responsibility to CUDA.
    pub fn take_y_handle(&mut self) -> *mut c_void {
        std::mem::replace(&mut self.y_handle, std::ptr::null_mut())
    }

    pub fn take_u_handle(&mut self) -> *mut c_void {
        std::mem::replace(&mut self.u_handle, std::ptr::null_mut())
    }

    pub fn take_v_handle(&mut self) -> *mut c_void {
        std::mem::replace(&mut self.v_handle, std::ptr::null_mut())
    }
}

/// All import handles for the zero-copy pipeline. Passed to the capture
/// thread, consumed during `NvjpegDecoder::try_new_shared`, then dropped.
pub struct ImportHandles {
    pub sets: Vec<ImportHandleSet>,
    pub layout: SharedBufferLayout,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

/// Calculate buffer sizes for YUV 4:2:2 planes at the given dimensions.
/// Each row is aligned to `COPY_BYTES_PER_ROW_ALIGNMENT` (256 bytes) because
/// wgpu's `copy_buffer_to_texture` requires aligned `bytes_per_row`.
fn plane_sizes(width: u32, height: u32) -> SharedBufferLayout {
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let y_pitch = (width + align - 1) & !(align - 1);
    let uv_pitch = ((width / 2) + align - 1) & !(align - 1);
    let y_size = (y_pitch as u64) * (height as u64);
    let uv_size = (uv_pitch as u64) * (height as u64);
    SharedBufferLayout { y_pitch, uv_pitch, y_size, uv_size }
}

impl SharedGpuBuffers {
    /// Try to create shared DX12 ↔ CUDA buffers via wgpu's HAL layer.
    ///
    /// Returns `(SharedGpuBuffers, ImportHandles)` on success:
    /// - `SharedGpuBuffers` stays in the renderer (wgpu Buffers + layout)
    /// - `ImportHandles` is passed to the capture thread for CUDA import,
    ///   then dropped (closing the NT handles)
    ///
    /// Returns None if the DX12 backend isn't active or creation fails.
    pub fn try_new(
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> Option<(Self, ImportHandles)> {
        let layout = plane_sizes(width, height);

        info!(
            "creating shared DX12 buffers for zero-copy: {}x{} (Y={}B pitch={}, UV={}B pitch={}, {} sets)",
            width, height, layout.y_size, layout.y_pitch, layout.uv_size, layout.uv_pitch, NUM_BUFFER_SETS
        );

        // Access the raw DX12 device via wgpu HAL.
        let raw_resources = unsafe {
            device.as_hal::<Dx12, _, _>(|hal_device| {
                let hal_device = hal_device?;
                let raw_device: &ID3D12Device = hal_device.raw_device();

                let mut sets = Vec::with_capacity(NUM_BUFFER_SETS);
                for i in 0..NUM_BUFFER_SETS {
                    match create_shared_buffer_set(raw_device, layout.y_size, layout.uv_size, i) {
                        Ok(set) => sets.push(set),
                        Err(e) => {
                            warn!("failed to create shared DX12 buffer set {i}: {e}");
                            // Drop already-created sets — their handles are
                            // closed by RawBufferSet's implicit drop via
                            // close_handle_if_valid (not yet applied here since
                            // RawBufferSet uses raw ptrs). Close manually:
                            for prev_set in &sets {
                                close_handle_ptr(prev_set.y_handle);
                                close_handle_ptr(prev_set.u_handle);
                                close_handle_ptr(prev_set.v_handle);
                            }
                            return None;
                        }
                    }
                }

                Some(sets)
            })
        };

        let raw_sets = match raw_resources {
            Some(r) => r,
            None => {
                warn!("DX12 HAL not available (wgpu may be using Vulkan) — zero-copy disabled");
                return None;
            }
        };

        // Wrap the DX12 resources as high-level wgpu Buffers and split
        // handles into the ephemeral ImportHandles struct.
        let mut buffer_sets_vec = Vec::with_capacity(NUM_BUFFER_SETS);
        let mut import_sets = Vec::with_capacity(NUM_BUFFER_SETS);

        for raw_set in raw_sets {
            let y_buffer = wrap_as_wgpu_buffer(device, raw_set.y_resource.clone(), layout.y_size);
            let u_buffer = wrap_as_wgpu_buffer(device, raw_set.u_resource.clone(), layout.uv_size);
            let v_buffer = wrap_as_wgpu_buffer(device, raw_set.v_resource.clone(), layout.uv_size);

            buffer_sets_vec.push(PlaneBufferSet {
                y_buffer,
                u_buffer,
                v_buffer,
            });

            import_sets.push(ImportHandleSet {
                y_handle: raw_set.y_handle,
                u_handle: raw_set.u_handle,
                v_handle: raw_set.v_handle,
                y_size: layout.y_size,
                u_size: layout.uv_size,
                v_size: layout.uv_size,
            });
        }

        let buffer_sets: [PlaneBufferSet; NUM_BUFFER_SETS] = buffer_sets_vec
            .try_into()
            .ok()
            .expect("buffer_sets length mismatch");

        info!("shared DX12 buffers created successfully for zero-copy pipeline");

        let shared = Self(buffer_sets);

        let import_handles = ImportHandles {
            sets: import_sets,
            layout,
        };

        Some((shared, import_handles))
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Raw DX12 resources for one buffer set, before wrapping as wgpu objects.
struct RawBufferSet {
    y_resource: ID3D12Resource,
    u_resource: ID3D12Resource,
    v_resource: ID3D12Resource,
    y_handle: *mut c_void,
    u_handle: *mut c_void,
    v_handle: *mut c_void,
}

/// Create a set of shared DX12 committed buffers with NT handles.
fn create_shared_buffer_set(
    device: &ID3D12Device,
    y_size: u64,
    uv_size: u64,
    set_index: usize,
) -> Result<RawBufferSet, String> {
    let (y_resource, y_handle) =
        create_shared_committed_buffer(device, y_size, &format!("Y-{set_index}"))?;

    let (u_resource, u_handle) = match create_shared_committed_buffer(device, uv_size, &format!("U-{set_index}")) {
        Ok(result) => result,
        Err(e) => {
            unsafe { close_handle_ptr(y_handle); }
            return Err(e);
        }
    };

    let (v_resource, v_handle) = match create_shared_committed_buffer(device, uv_size, &format!("V-{set_index}")) {
        Ok(result) => result,
        Err(e) => {
            unsafe { close_handle_ptr(y_handle); }
            unsafe { close_handle_ptr(u_handle); }
            return Err(e);
        }
    };

    debug!(
        "DX12 shared buffer set {set_index}: Y={y_size}B U={uv_size}B V={uv_size}B"
    );

    Ok(RawBufferSet {
        y_resource,
        u_resource,
        v_resource,
        y_handle,
        u_handle,
        v_handle,
    })
}

/// Create a single DX12 committed buffer with D3D12_HEAP_FLAG_SHARED and
/// export an NT shared handle for CUDA import.
fn create_shared_committed_buffer(
    device: &ID3D12Device,
    size: u64,
    label: &str,
) -> Result<(ID3D12Resource, *mut c_void), String> {
    let heap_properties = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 0,
        VisibleNodeMask: 0,
    };

    let resource_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_NONE,
    };

    let mut resource: Option<ID3D12Resource> = None;

    unsafe {
        device
            .CreateCommittedResource(
                &heap_properties,
                D3D12_HEAP_FLAG_SHARED,
                &resource_desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut resource,
            )
            .map_err(|e| {
                format!("CreateCommittedResource failed for {label}: {e}")
            })?;
    }

    let resource = resource
        .ok_or_else(|| format!("CreateCommittedResource returned null for {label}"))?;

    // Create an NT shared handle for CUDA import.
    let handle = unsafe {
        device
            .CreateSharedHandle(&resource, None, windows::Win32::Foundation::GENERIC_ALL.0, PCWSTR::null())
            .map_err(|e| format!("CreateSharedHandle failed for {label}: {e}"))?
    };

    debug!("created shared DX12 buffer '{label}': {size}B, handle={handle:?}");

    Ok((resource, handle.0))
}

/// Wrap a raw DX12 ID3D12Resource as a high-level wgpu::Buffer.
fn wrap_as_wgpu_buffer(
    device: &wgpu::Device,
    resource: ID3D12Resource,
    size: u64,
) -> wgpu::Buffer {
    unsafe {
        let hal_buffer =
            wgpu::hal::dx12::Device::buffer_from_raw(resource, size);

        device.create_buffer_from_hal::<Dx12>(
            hal_buffer,
            &wgpu::BufferDescriptor {
                label: Some("tacklecast-shared-plane-buffer"),
                size,
                usage: wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            },
        )
    }
}

/// Close an NT handle via a mutable pointer slot. Nulls the slot after
/// closing so it won't be double-closed.
unsafe fn close_handle_if_valid(slot: &mut *mut c_void) {
    let ptr = *slot;
    if !ptr.is_null() {
        *slot = std::ptr::null_mut();
        if let Err(e) = CloseHandle(HANDLE(ptr)) {
            warn!("CloseHandle failed: {e}");
        }
    }
}

/// Close an NT handle given a raw pointer (non-mutable convenience for
/// error paths where we don't have mutable access to the slot).
unsafe fn close_handle_ptr(handle: *mut c_void) {
    if !handle.is_null() {
        if let Err(e) = CloseHandle(HANDLE(handle)) {
            warn!("CloseHandle failed: {e}");
        }
    }
}
