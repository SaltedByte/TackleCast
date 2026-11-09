//! Zero-copy Direct3D 12 ↔ Direct3D 11 Video Processor interop for NVIDIA RTX
//! Video Super Resolution (RTX VSR).
//!
//! Uses the NVIDIA driver's built-in Post-Processing Extension (`kNvidiaPPEInterfaceGUID`)
//! on `ID3D11VideoContext::VideoProcessorSetStreamExtension` with shared DX12/D3D11
//! textures and a shared GPU fence (`ID3D12Fence` / `ID3D11Fence`).
//!
//! Zero external DLLs are required, zero CPU copies occur per frame, and no
//! resources are allocated until the user selects `ScaleFilter::RtxVsr`.

use std::ffi::c_void;
use tracing::{debug, info, warn};
use wgpu::hal::api::Dx12;
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};

/// NVIDIA proprietary D3D11 Video Processor Post-Processing Extension (PPE) GUID:
/// `{d43ce1b3-1f4b-48ac-baee-c3c25375e6f7}`.
const NVIDIA_PPE_INTERFACE_GUID: GUID = GUID::from_u128(0xd43ce1b3_1f4b_48ac_baee_c3c25375e6f7);

const STREAM_EXTENSION_VERSION_V1: u32 = 1;
const STREAM_EXTENSION_METHOD_SUPER_RESOLUTION: u32 = 2;

#[repr(C)]
struct NvidiaStreamExtensionInfo {
    version: u32,
    method: u32,
    enable: u32,
}

/// Ephemeral NT handle guard that closes the handle on drop unless taken.
struct ScopedHandle(HANDLE);

impl Drop for ScopedHandle {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

pub struct VsrResources {
    pub in_width: u32,
    pub in_height: u32,
    pub out_width: u32,
    pub out_height: u32,
    _input_texture: wgpu::Texture,
    pub input_view: wgpu::TextureView,
    #[cfg_attr(not(test), allow(dead_code))]
    pub output_texture: wgpu::Texture,
    _output_view: wgpu::TextureView,
    pub rcas_bind_group: wgpu::BindGroup,
    processor: ID3D11VideoProcessor,
    d3d11_input_view: ID3D11VideoProcessorInputView,
    d3d11_output_view: ID3D11VideoProcessorOutputView,
}

// SAFETY: All COM interfaces inside VsrContext/VsrResources are accessed exclusively
// from the renderer thread (or protected via ID3D11Multithread).
unsafe impl Send for VsrResources {}
unsafe impl Sync for VsrResources {}

pub struct VsrContext {
    d3d12_device: ID3D12Device,
    d3d12_queue: ID3D12CommandQueue,
    d3d12_fence: ID3D12Fence,
    d3d11_device5: ID3D11Device5,
    d3d11_context: ID3D11DeviceContext,
    d3d11_context4: ID3D11DeviceContext4,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    d3d11_fence: ID3D11Fence,
    fence_value: u64,
    pub resources: Option<VsrResources>,
}

unsafe impl Send for VsrContext {}
unsafe impl Sync for VsrContext {}

impl VsrContext {
    /// Attempt to initialize D3D11 Video Processor + NVIDIA RTX VSR on the same
    /// physical adapter as `wgpu::Device`. Returns `None` if DX12 is not active
    /// or the GPU/driver does not support NVIDIA RTX VSR.
    pub fn try_new(device: &wgpu::Device) -> Option<Self> {
        let (d3d12_device, d3d12_queue) = unsafe {
            device.as_hal::<Dx12, _, _>(|hal_device| {
                let hal_device = hal_device?;
                Some((
                    hal_device.raw_device().clone(),
                    hal_device.raw_queue().clone(),
                ))
            })?
        };

        unsafe {
            let luid = d3d12_device.GetAdapterLuid();
            let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
            let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).ok()?;

            if let Ok(desc) = adapter.GetDesc1() {
                // NVIDIA PCI Vendor ID is 0x10DE.
                if desc.VendorId != 0x10DE {
                    debug!(
                        "adapter vendor 0x{:04X} is not NVIDIA — RTX VSR unavailable",
                        desc.VendorId
                    );
                    return None;
                }
            }

            let mut d3d11_device: Option<ID3D11Device> = None;
            let mut d3d11_context: Option<ID3D11DeviceContext> = None;
            let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut d3d11_device),
                None,
                Some(&mut d3d11_context),
            )
            .ok()?;

            let d3d11_device = d3d11_device?;
            let d3d11_context = d3d11_context?;

            if let Ok(multithread) = d3d11_device.cast::<ID3D11Multithread>() {
                let _ = multithread.SetMultithreadProtected(true);
            }

            let d3d11_device5: ID3D11Device5 = d3d11_device.cast().ok()?;
            let d3d11_context4: ID3D11DeviceContext4 = d3d11_context.cast().ok()?;
            let video_device: ID3D11VideoDevice = d3d11_device.cast().ok()?;
            let video_context: ID3D11VideoContext = d3d11_context.cast().ok()?;

            // Probe NVIDIA RTX VSR extension support before committing
            let probe_enumerator = create_enumerator(&video_device, 1280, 720, 1920, 1080).ok()?;
            let probe_processor = video_device
                .CreateVideoProcessor(&probe_enumerator, 0)
                .ok()?;
            enable_nvidia_vsr(&video_context, &probe_processor)
                .map_err(|e| {
                    warn!("NVIDIA RTX VSR stream extension not supported by driver: {e}");
                    e
                })
                .ok()?;

            // Create shared GPU fence between D3D12 and D3D11
            let d3d12_fence: ID3D12Fence =
                d3d12_device.CreateFence(0, D3D12_FENCE_FLAG_SHARED).ok()?;
            let fence_handle = ScopedHandle(
                d3d12_device
                    .CreateSharedHandle(&d3d12_fence, None, GENERIC_ALL.0, PCWSTR::null())
                    .ok()?,
            );

            let mut d3d11_fence: Option<ID3D11Fence> = None;
            d3d11_device5
                .OpenSharedFence(fence_handle.0, &mut d3d11_fence)
                .ok()?;
            let d3d11_fence = d3d11_fence?;

            info!("NVIDIA RTX Video Super Resolution (RTX VSR) initialized via DX12↔D3D11 zero-copy interop");

            Some(Self {
                d3d12_device,
                d3d12_queue,
                d3d12_fence,
                d3d11_device5,
                d3d11_context,
                d3d11_context4,
                video_device,
                video_context,
                d3d11_fence,
                fence_value: 0,
                resources: None,
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ensure_resources(
        &mut self,
        device: &wgpu::Device,
        rcas_bind_group_layout: &wgpu::BindGroupLayout,
        rcas_uniforms: &wgpu::Buffer,
        in_width: u32,
        in_height: u32,
        out_width: u32,
        out_height: u32,
    ) -> Option<&VsrResources> {
        let needs_rebuild = match &self.resources {
            Some(res) => {
                res.in_width != in_width
                    || res.in_height != in_height
                    || res.out_width != out_width
                    || res.out_height != out_height
            }
            None => true,
        };

        if needs_rebuild {
            // Wait for in-flight GPU commands on the previous shared textures before replacing them.
            device.poll(wgpu::Maintain::Wait);
            self.resources = self.create_resources(
                device,
                rcas_bind_group_layout,
                rcas_uniforms,
                in_width,
                in_height,
                out_width,
                out_height,
            );
        }

        self.resources.as_ref()
    }

    #[allow(clippy::too_many_arguments)]
    fn create_resources(
        &self,
        device: &wgpu::Device,
        rcas_bind_group_layout: &wgpu::BindGroupLayout,
        rcas_uniforms: &wgpu::Buffer,
        in_width: u32,
        in_height: u32,
        out_width: u32,
        out_height: u32,
    ) -> Option<VsrResources> {
        unsafe {
            let enumerator = create_enumerator(
                &self.video_device,
                in_width,
                in_height,
                out_width,
                out_height,
            )
            .ok()?;
            let processor = self
                .video_device
                .CreateVideoProcessor(&enumerator, 0)
                .ok()?;

            configure_processor(
                &self.video_context,
                &processor,
                in_width,
                in_height,
                out_width,
                out_height,
            )
            .ok()?;

            let (in_d3d12, in_d3d11) = create_shared_texture_2d(
                &self.d3d12_device,
                &self.d3d11_device5,
                in_width,
                in_height,
            )
            .ok()?;
            let (out_d3d12, out_d3d11) = create_shared_texture_2d(
                &self.d3d12_device,
                &self.d3d11_device5,
                out_width,
                out_height,
            )
            .ok()?;

            let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut d3d11_input_view: Option<ID3D11VideoProcessorInputView> = None;
            self.video_device
                .CreateVideoProcessorInputView(
                    &in_d3d11,
                    &enumerator,
                    &input_view_desc,
                    Some(&mut d3d11_input_view),
                )
                .ok()?;
            let d3d11_input_view = d3d11_input_view?;

            let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut d3d11_output_view: Option<ID3D11VideoProcessorOutputView> = None;
            self.video_device
                .CreateVideoProcessorOutputView(
                    &out_d3d11,
                    &enumerator,
                    &output_view_desc,
                    Some(&mut d3d11_output_view),
                )
                .ok()?;
            let d3d11_output_view = d3d11_output_view?;

            let input_texture = wrap_shared_texture_in_wgpu(
                device,
                in_d3d12,
                in_width,
                in_height,
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                "tacklecast-vsr-input-texture",
            );
            let input_view = input_texture.create_view(&wgpu::TextureViewDescriptor::default());

            let output_texture = wrap_shared_texture_in_wgpu(
                device,
                out_d3d12,
                out_width,
                out_height,
                wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC,
                "tacklecast-vsr-output-texture",
            );
            let output_view = output_texture.create_view(&wgpu::TextureViewDescriptor::default());

            let rcas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("tacklecast-vsr-rcas-bind-group"),
                layout: rcas_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&output_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: rcas_uniforms.as_entire_binding(),
                    },
                ],
            });

            debug!(
                "allocated RTX VSR shared textures: {}x{} -> {}x{}",
                in_width, in_height, out_width, out_height
            );

            Some(VsrResources {
                in_width,
                in_height,
                out_width,
                out_height,
                _input_texture: input_texture,
                input_view,
                output_texture,
                _output_view: output_view,
                rcas_bind_group,
                processor,
                d3d11_input_view,
                d3d11_output_view,
            })
        }
    }

    /// Execute RTX VSR upscale on the GPU timeline between two wgpu submissions:
    /// 1. D3D12 queue signals `fence_value` (after Pass 1 YUV->RGB completes).
    /// 2. D3D11 context waits on `fence_value`, runs `VideoProcessorBlt`, signals `fence_value + 1`, and flushes.
    /// 3. D3D12 queue waits on `fence_value + 1` (before Pass 3 RCAS/present executes).
    pub fn execute_upscale(&mut self) -> bool {
        let Some(res) = &self.resources else {
            return false;
        };

        unsafe {
            self.fence_value += 1;
            let wait_for_dx12 = self.fence_value;
            if self
                .d3d12_queue
                .Signal(&self.d3d12_fence, wait_for_dx12)
                .is_err()
            {
                return false;
            }

            if self
                .d3d11_context4
                .Wait(&self.d3d11_fence, wait_for_dx12)
                .is_err()
            {
                return false;
            }

            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(res.d3d11_input_view.clone())),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };

            let blt_res = self.video_context.VideoProcessorBlt(
                &res.processor,
                &res.d3d11_output_view,
                0,
                std::slice::from_ref(&stream),
            );
            std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
            if let Err(e) = blt_res {
                warn!("RTX VSR VideoProcessorBlt failed: {e}");
                self.resources = None;
                return false;
            }

            self.fence_value += 1;
            let wait_for_d3d11 = self.fence_value;
            if self
                .d3d11_context4
                .Signal(&self.d3d11_fence, wait_for_d3d11)
                .is_err()
            {
                return false;
            }
            // Flush D3D11 commands to the kernel scheduler immediately so D3D12's GPU wait sees the signal.
            self.d3d11_context.Flush();

            if self
                .d3d12_queue
                .Wait(&self.d3d12_fence, wait_for_d3d11)
                .is_err()
            {
                return false;
            }

            true
        }
    }
}

unsafe fn create_enumerator(
    video_device: &ID3D11VideoDevice,
    in_w: u32,
    in_h: u32,
    out_w: u32,
    out_h: u32,
) -> windows::core::Result<ID3D11VideoProcessorEnumerator> {
    let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        },
        InputWidth: in_w.max(1),
        InputHeight: in_h.max(1),
        OutputFrameRate: DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        },
        OutputWidth: out_w.max(1),
        OutputHeight: out_h.max(1),
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };
    video_device.CreateVideoProcessorEnumerator(&content_desc)
}

unsafe fn enable_nvidia_vsr(
    video_context: &ID3D11VideoContext,
    processor: &ID3D11VideoProcessor,
) -> windows::core::Result<()> {
    let ext_info = NvidiaStreamExtensionInfo {
        version: STREAM_EXTENSION_VERSION_V1,
        method: STREAM_EXTENSION_METHOD_SUPER_RESOLUTION,
        enable: 1,
    };
    let hr = video_context.VideoProcessorSetStreamExtension(
        processor,
        0,
        &NVIDIA_PPE_INTERFACE_GUID,
        std::mem::size_of::<NvidiaStreamExtensionInfo>() as u32,
        (&ext_info as *const NvidiaStreamExtensionInfo).cast::<c_void>(),
    );
    windows::core::HRESULT(hr).ok()
}

unsafe fn configure_processor(
    video_context: &ID3D11VideoContext,
    processor: &ID3D11VideoProcessor,
    in_w: u32,
    in_h: u32,
    out_w: u32,
    out_h: u32,
) -> windows::core::Result<()> {
    video_context.VideoProcessorSetStreamFrameFormat(
        processor,
        0,
        D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    );
    video_context.VideoProcessorSetStreamAutoProcessingMode(processor, 0, false);

    let src_rect = RECT {
        left: 0,
        top: 0,
        right: in_w as i32,
        bottom: in_h as i32,
    };
    let dst_rect = RECT {
        left: 0,
        top: 0,
        right: out_w as i32,
        bottom: out_h as i32,
    };
    video_context.VideoProcessorSetStreamSourceRect(processor, 0, true, Some(&src_rect));
    video_context.VideoProcessorSetStreamDestRect(processor, 0, true, Some(&dst_rect));
    video_context.VideoProcessorSetOutputTargetRect(processor, true, Some(&dst_rect));

    // Full-range RGB (0-255) on both input and output so the video processor
    // preserves the exact color range already resolved by Pass 1.
    // Bit 1: RGB_Range (0 = Full, 1 = Limited)
    // Bit 2: YCbCr_Matrix (0 = BT.601, 1 = BT.709)
    // Bits 4-5: Nominal_Range (2 = 0-255 Full Range)
    let color_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
        _bitfield: (1 << 2) | (2 << 4),
    };
    video_context.VideoProcessorSetStreamColorSpace(processor, 0, &color_space);
    video_context.VideoProcessorSetOutputColorSpace(processor, &color_space);

    enable_nvidia_vsr(video_context, processor)
}

unsafe fn create_shared_texture_2d(
    d3d12_device: &ID3D12Device,
    d3d11_device5: &ID3D11Device5,
    width: u32,
    height: u32,
) -> Result<(ID3D12Resource, ID3D11Texture2D), String> {
    let heap_props = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 0,
        VisibleNodeMask: 0,
    };

    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: width.max(1) as u64,
        Height: height.max(1),
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
            | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
    };

    let mut d3d12_resource: Option<ID3D12Resource> = None;
    d3d12_device
        .CreateCommittedResource(
            &heap_props,
            D3D12_HEAP_FLAG_SHARED,
            &desc,
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut d3d12_resource,
        )
        .map_err(|e| format!("CreateCommittedResource failed: {e}"))?;
    let d3d12_resource = d3d12_resource.ok_or("CreateCommittedResource returned null")?;

    let shared_handle = ScopedHandle(
        d3d12_device
            .CreateSharedHandle(&d3d12_resource, None, GENERIC_ALL.0, PCWSTR::null())
            .map_err(|e| format!("CreateSharedHandle failed: {e}"))?,
    );

    let d3d11_texture: ID3D11Texture2D = d3d11_device5
        .OpenSharedResource1(shared_handle.0)
        .map_err(|e| format!("OpenSharedResource1 failed: {e}"))?;

    Ok((d3d12_resource, d3d11_texture))
}

unsafe fn wrap_shared_texture_in_wgpu(
    device: &wgpu::Device,
    resource: ID3D12Resource,
    width: u32,
    height: u32,
    usage: wgpu::TextureUsages,
    label: &str,
) -> wgpu::Texture {
    let size = wgpu::Extent3d {
        width: width.max(1),
        height: height.max(1),
        depth_or_array_layers: 1,
    };
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let hal_tex = wgpu::hal::dx12::Device::texture_from_raw(
        resource,
        format,
        wgpu::TextureDimension::D2,
        size,
        1,
        1,
    );
    device.create_texture_from_hal::<Dx12>(
        hal_tex,
        &wgpu::TextureDescriptor {
            label: Some(label),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        },
    )
}
