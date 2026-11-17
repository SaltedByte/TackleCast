//! GPU resources for frame generation (child module of `render`).
//!
//! Data flow for every captured frame while frame generation is active:
//!
//! ```text
//!  Y/U/V textures --(VIDEO_SHADER, native size)--> source slot texture (RGBA8)
//!  source slot texture --copy--> source slot buffer (shared DX12 <-> CUDA)
//!  source slot vs. last unique slot --DIFF_SHADER--> 16-byte stats --> readback
//!  CUDA/FRUC: (previous unique buffer, source buffer) --> output buffer
//!  output buffer --copy--> output texture --(RGB blit / FSR)--> swapchain
//! ```
//!
//! Everything stays on the GPU. The only GPU->CPU transfer is the 16-byte
//! duplicate statistic per frame; frames themselves are never read back.

use super::*;
use crate::framegen::duplicate::{diff_dispatch_size, DiffStats, DIFF_SHADER};
use std::collections::HashMap;

pub const FG_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

#[path = "render_framegen_fix.rs"]
mod fix;
pub use fix::{
    FgProtection, FixReport, FixStats, VIEW_FINAL, VIEW_OVERLAY, VIEW_RAW, ZONE_BOTTOM_LEFT,
    ZONE_BOTTOM_RIGHT, ZONE_TOP_LEFT, ZONE_TOP_RIGHT,
};

const PROTECT_TILE: u32 = 8;

/// Bytes of the per-source readback: duplicate statistics, then the column
/// and row luma profiles of the frame and of its reference (f32 each).
pub fn fg_readback_size(width: u32, height: u32) -> u64 {
    std::mem::size_of::<DiffStats>() as u64 + 8 * (width as u64 + height as u64)
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PrepParams {
    width: u32,
    height: u32,
    tiles_x: u32,
    tiles_y: u32,
    /// Bit 0: paint the HUD out; bit 1: HUD guard (salient elements only,
    /// write the restore mask, fill along the camera motion).
    hud: u32,
    total_tiles: u32,
    /// Word offset of this slot's mask in `hud_bits`.
    slot_offset: u32,
    mv_valid: u32,
    static_thr: f32,
    global_motion: f32,
    local_motion: f32,
    /// Minimum luma contrast against the surroundings for a HUD pixel.
    salience: f32,
    /// Camera motion: `current(x) ≈ previous(x - mv)`.
    mv_x: i32,
    mv_y: i32,
    /// HUD guard window half-size around the centre (0 = whole frame).
    guard_window: u32,
    _pad1: u32,
}

/// Capture-side passes: luma profiles (camera motion) and HUD removal from
/// the image FRUC sees.
struct PrepResources {
    layout: wgpu::BindGroupLayout,
    profile: wgpu::ComputePipeline,
    persist: wgpu::ComputePipeline,
    core: wgpu::ComputePipeline,
    isolate: wgpu::ComputePipeline,
    fill: wgpu::ComputePipeline,
    uniforms: wgpu::Buffer,
    persistence: wgpu::Buffer,
    /// HUD guard memory: how reliably each pixel has kept its shape while
    /// the scenery around it moved (survives still periods).
    confidence: wgpu::Buffer,
    tile_moving: wgpu::Buffer,
    global: wgpu::Buffer,
    core_mask: wgpu::Buffer,
    profiles: wgpu::Buffer,
    /// FRUC input with the HUD painted over (copied into the shared buffer).
    fruc_input: wgpu::Texture,
    fruc_input_view: wgpu::TextureView,
    bind_groups: HashMap<(usize, usize), wgpu::BindGroup>,
    tiles_x: u32,
    tiles_y: u32,
}

/// Which frame-generation image the renderer should present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FgImage {
    Source(usize),
    Generated(usize),
}

pub struct FgSlot {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    /// Shared with CUDA on Windows; a plain buffer elsewhere (tests).
    pub buffer: wgpu::Buffer,
    blit_bind_group: wgpu::BindGroup,
}

pub struct FgSourceExtras {
    pub diff_storage: wgpu::Buffer,
    pub readback: wgpu::Buffer,
}

/// Handles the worker needs to import the shared buffers into CUDA.
#[cfg_attr(
    not(all(target_os = "windows", feature = "gpu-decode")),
    allow(dead_code)
)]
pub struct FgImport {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub buffer_size: u64,
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    pub source_handles: Vec<crate::dx12_interop::SharedHandle>,
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    pub output_handles: Vec<crate::dx12_interop::SharedHandle>,
    pub readbacks: Vec<wgpu::Buffer>,
    pub adapter_luid: Option<[u8; 8]>,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BlitUniforms {
    filter_mode: u32,
    _pad: u32,
    viewport_size: [f32; 2],
}

struct CaptureBinding {
    generation: u64,
    bind_group: wgpu::BindGroup,
}

pub struct FgGpu {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub sources: Vec<FgSlot>,
    pub source_extras: Vec<FgSourceExtras>,
    pub outputs: Vec<FgSlot>,
    diff_pipeline: wgpu::ComputePipeline,
    diff_layout: wgpu::BindGroupLayout,
    diff_bind_groups: HashMap<(usize, usize), wgpu::BindGroup>,
    blit_pipeline: wgpu::RenderPipeline,
    blit_uniforms: wgpu::Buffer,
    capture_pipeline: Option<wgpu::RenderPipeline>,
    capture_uniforms: wgpu::Buffer,
    capture_binding: Option<CaptureBinding>,
    fix: Option<fix::FixResources>,
    /// The correction passes could not be created on this device.
    fix_failed: bool,
    prep: Option<PrepResources>,
    prep_failed: bool,
    prep_timer: Option<fix::GpuTimer>,
    /// HUD-guard masks, one bit per pixel, `hud_words_per_slot` u32 words
    /// per source slot: written by the capture-side fill, read when the
    /// generated frame ending at that slot is published.
    hud_bits: wgpu::Buffer,
    hud_words_per_slot: usize,
    /// HUD guard search window: half-size (px) of a square around the
    /// screen centre, where RDR2 keeps its crosshair / aim reticle. 0 =
    /// whole frame (tests).
    pub guard_window: u32,
    /// Output slots hold flow + blend mask (RIFE `fm` models) instead of an
    /// image; [`FgGpu::encode_flow_warp`] composes the image on publish.
    pub flow_output: bool,
    flow_warp: Option<FlowWarp>,
    flow_warp_failed: bool,
}

/// Composition of a generated frame from RIFE flow + mask.
struct FlowWarp {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    uniforms: wgpu::Buffer,
    /// Copy of the output slot's flow/mask: the image is written back into
    /// the same slot.
    field: wgpu::Buffer,
    bind_groups: HashMap<(usize, usize, usize), wgpu::BindGroup>,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct WarpParams {
    width: u32,
    height: u32,
    half_w: u32,
    half_h: u32,
    contrast: f32,
    row_words: u32,
    _pad0: u32,
    _pad1: u32,
}

/// Blend-mask contrast around 0.5 (1 = the network's mask). Above 1 makes
/// occlusion edges pick one frame more decisively; replays showed 1.5-3 cut
/// thin objects whose flow the network misses, so 1 is the default.
/// `TACKLECAST_RIFE_CONTRAST` overrides it for experiments.
fn warp_contrast() -> f32 {
    std::env::var("TACKLECAST_RIFE_CONTRAST")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0 && *v <= 8.0)
        .unwrap_or(1.0)
}

/// Bytes per row for a tightly packed RGBA8 row, rounded up to wgpu's copy
/// alignment (256). For widths that are multiples of 64 this is exactly
/// `width * 4`, which FRUC receives as "packed" (pitch 0).
pub fn fg_pitch(width: u32) -> u32 {
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    (width * 4).div_ceil(align) * align
}

impl FgGpu {
    /// Create all frame-generation resources. On Windows the slot buffers
    /// are shared DX12 resources and `FgImport` carries their NT handles.
    pub fn new(
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        source_count: usize,
        output_count: usize,
    ) -> Result<(Self, FgImport), String> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(format!("unsupported frame size {width}x{height}"));
        }
        let pitch = fg_pitch(width);
        let buffer_size = pitch as u64 * height as u64;
        // STORAGE: the flow warp reads source slots and writes output slots
        // (the shared DX12 buffers allow unordered access).
        let buffer_usage = wgpu::BufferUsages::COPY_SRC
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::STORAGE;

        #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
        let (source_buffers, source_handles, output_buffers, output_handles) = {
            let (sb, sh) = crate::dx12_interop::create_shared_buffers(
                device,
                source_count,
                buffer_size,
                buffer_usage,
                "tacklecast-fg-source",
            )
            .ok_or("shared DX12 buffers could not be created (DX12 backend required)")?;
            let (ob, oh) = crate::dx12_interop::create_shared_buffers(
                device,
                output_count,
                buffer_size,
                buffer_usage,
                "tacklecast-fg-output",
            )
            .ok_or("shared DX12 buffers could not be created (DX12 backend required)")?;
            (sb, sh, ob, oh)
        };
        #[cfg(not(all(target_os = "windows", feature = "gpu-decode")))]
        let (source_buffers, output_buffers) = {
            let make = |label: &str, count: usize| -> Vec<wgpu::Buffer> {
                (0..count)
                    .map(|_| {
                        device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some(label),
                            size: buffer_size,
                            usage: buffer_usage,
                            mapped_at_creation: false,
                        })
                    })
                    .collect()
            };
            (
                make("tacklecast-fg-source", source_count),
                make("tacklecast-fg-output", output_count),
            )
        };

        let blit_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tacklecast-fg-blit-layout"),
            entries: &[
                texture_layout_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let blit_uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tacklecast-fg-blit-uniforms"),
            size: std::mem::size_of::<BlitUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let linear = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tacklecast-fg-linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let nearest = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tacklecast-fg-nearest"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tacklecast-fg-blit-shader"),
            source: wgpu::ShaderSource::Wgsl(RGB_BLIT_SHADER.into()),
        });
        let blit_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tacklecast-fg-blit-pipeline-layout"),
            bind_group_layouts: &[Some(&blit_layout)],
            immediate_size: 0,
        });
        let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tacklecast-fg-blit-pipeline"),
            layout: Some(&blit_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &blit_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &blit_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let make_slot = |label: &str, buffer: wgpu::Buffer, render_target: bool| -> FgSlot {
            let mut usage = wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST;
            if render_target {
                usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
            } else {
                // Output slots are written by the protection compose pass.
                usage |= wgpu::TextureUsages::STORAGE_BINDING;
            }
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FG_FORMAT,
                usage,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let blit_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("tacklecast-fg-blit-bind-group"),
                layout: &blit_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&linear),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&nearest),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: blit_uniforms.as_entire_binding(),
                    },
                ],
            });
            FgSlot {
                texture,
                view,
                buffer,
                blit_bind_group,
            }
        };

        let sources: Vec<FgSlot> = source_buffers
            .into_iter()
            .map(|b| make_slot("tacklecast-fg-source-texture", b, true))
            .collect();
        let outputs: Vec<FgSlot> = output_buffers
            .into_iter()
            .map(|b| make_slot("tacklecast-fg-output-texture", b, false))
            .collect();
        let source_extras: Vec<FgSourceExtras> = (0..sources.len())
            .map(|_| FgSourceExtras {
                diff_storage: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("tacklecast-fg-diff-storage"),
                    size: std::mem::size_of::<DiffStats>() as u64,
                    usage: wgpu::BufferUsages::STORAGE
                        | wgpu::BufferUsages::COPY_SRC
                        | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                readback: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("tacklecast-fg-diff-readback"),
                    size: fg_readback_size(width, height),
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
            })
            .collect();

        let diff_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tacklecast-fg-diff-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let diff_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tacklecast-fg-diff-shader"),
            source: wgpu::ShaderSource::Wgsl(DIFF_SHADER.into()),
        });
        let diff_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tacklecast-fg-diff-pipeline-layout"),
            bind_group_layouts: &[Some(&diff_layout)],
            immediate_size: 0,
        });
        let diff_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("tacklecast-fg-diff-pipeline"),
            layout: Some(&diff_pipeline_layout),
            module: &diff_shader,
            entry_point: Some("cs_main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let capture_uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tacklecast-fg-capture-uniforms"),
            size: std::mem::size_of::<VideoUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
        let adapter_luid = crate::dx12_interop::adapter_luid(device);
        #[cfg(not(all(target_os = "windows", feature = "gpu-decode")))]
        let adapter_luid = None;

        let import = FgImport {
            width,
            height,
            pitch,
            buffer_size,
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            source_handles,
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            output_handles,
            readbacks: source_extras.iter().map(|e| e.readback.clone()).collect(),
            adapter_luid,
        };

        let hud_words_per_slot = (width as usize * height as usize).div_ceil(32);
        let hud_bits = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tacklecast-fg-hud-bits"),
            size: (hud_words_per_slot * source_count.max(1) * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok((
            Self {
                width,
                height,
                pitch,
                sources,
                source_extras,
                outputs,
                diff_pipeline,
                diff_layout,
                diff_bind_groups: HashMap::new(),
                blit_pipeline,
                blit_uniforms,
                capture_pipeline: None,
                capture_uniforms,
                capture_binding: None,
                fix: None,
                fix_failed: false,
                prep: None,
                prep_failed: false,
                prep_timer: None,
                hud_bits,
                hud_words_per_slot,
                guard_window: (height * 96 / 1440).max(48),
                flow_output: false,
                flow_warp: None,
                flow_warp_failed: false,
            },
            import,
        ))
    }

    /// Create the protection passes on first use. Returns false (and stays
    /// false) if the device rejects them; frames are then published as-is.
    fn diff_bind_group(
        &mut self,
        device: &wgpu::Device,
        current: usize,
        reference: usize,
    ) -> &wgpu::BindGroup {
        let sources = &self.sources;
        let extras = &self.source_extras;
        let layout = &self.diff_layout;
        self.diff_bind_groups
            .entry((current, reference))
            .or_insert_with(|| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("tacklecast-fg-diff-bind-group"),
                    layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&sources[current].view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&sources[reference].view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: extras[current].diff_storage.as_entire_binding(),
                        },
                    ],
                })
            })
    }

    /// Copy a source slot texture into its shared buffer and, when a
    /// reference slot is given, compute the duplicate statistics.
    pub fn encode_transfer(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        slot: usize,
        reference: Option<usize>,
        protection: FgProtection,
    ) {
        let reference = reference.filter(|r| *r != slot);
        if protection.hud_guard {
            // This slot's restore mask is rebuilt below (or stays empty).
            let words = self.hud_words_per_slot as u64 * 4;
            encoder.clear_buffer(&self.hud_bits, slot as u64 * words, Some(words));
        }
        let prep_ready = reference.is_some() && self.ensure_prep(device, queue);
        let hud_removed = match reference {
            Some(r) if prep_ready => self.encode_prep(
                device,
                queue,
                encoder,
                (slot, r),
                protection.removes_hud(),
                protection.hud_guard,
                protection.motion,
            ),
            _ => false,
        };
        let input_texture = match (&self.prep, hud_removed) {
            (Some(prep), true) => &prep.fruc_input,
            _ => &self.sources[slot].texture,
        };
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: input_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.sources[slot].buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.pitch),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        let Some(reference) = reference else {
            return;
        };
        encoder.clear_buffer(&self.source_extras[slot].diff_storage, 0, None);
        let (groups_x, groups_y) = diff_dispatch_size(self.width, self.height);
        let pipeline = self.diff_pipeline.clone();
        let bind_group = self.diff_bind_group(device, slot, reference).clone();
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("tacklecast-fg-diff-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(groups_x, groups_y, 1);
        }
        encoder.copy_buffer_to_buffer(
            &self.source_extras[slot].diff_storage,
            0,
            &self.source_extras[slot].readback,
            0,
            std::mem::size_of::<DiffStats>() as u64,
        );
        if let (Some(prep), true) = (&self.prep, prep_ready) {
            encoder.copy_buffer_to_buffer(
                &prep.profiles,
                0,
                &self.source_extras[slot].readback,
                std::mem::size_of::<DiffStats>() as u64,
                8 * (self.width as u64 + self.height as u64),
            );
        }
    }

    fn ensure_prep(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> bool {
        if self.prep.is_some() {
            return true;
        }
        if self.prep_timer.is_none() {
            self.prep_timer = fix::GpuTimer::new(device, queue, "tacklecast-fg-prep-timer");
        }
        if self.prep_failed {
            return false;
        }
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let texture_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let storage_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: false },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tacklecast-fg-prep-layout"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: FG_FORMAT,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                storage_entry(3),
                storage_entry(4),
                storage_entry(5),
                storage_entry(6),
                storage_entry(7),
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                storage_entry(9),
                storage_entry(10),
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tacklecast-fg-prep-shader"),
            source: wgpu::ShaderSource::Wgsl(PREP_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tacklecast-fg-prep-pipeline-layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("tacklecast-fg-prep-pipeline"),
                layout: Some(&pipeline_layout),
                module: &shader,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let (width, height) = (self.width, self.height);
        let tiles_x = width.div_ceil(PROTECT_TILE);
        let tiles_y = height.div_ceil(PROTECT_TILE);
        let storage = |label: &str, size: u64, extra: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | extra,
                mapped_at_creation: false,
            })
        };
        let pixels = width as u64 * height as u64;
        let fruc_input = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tacklecast-fg-fruc-input"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FG_FORMAT,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let fruc_input_view = fruc_input.create_view(&Default::default());
        self.prep = Some(PrepResources {
            profile: pipeline("profile"),
            persist: pipeline("persist"),
            core: pipeline("core"),
            isolate: pipeline("isolate"),
            fill: pipeline("fill"),
            layout,
            uniforms: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tacklecast-fg-prep-uniforms"),
                size: std::mem::size_of::<PrepParams>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            persistence: storage(
                "tacklecast-fg-prep-persistence",
                pixels * 4,
                wgpu::BufferUsages::empty(),
            ),
            confidence: storage(
                "tacklecast-fg-prep-confidence",
                pixels * 4,
                wgpu::BufferUsages::empty(),
            ),
            tile_moving: storage(
                "tacklecast-fg-prep-tiles",
                tiles_x as u64 * tiles_y as u64 * 4,
                wgpu::BufferUsages::empty(),
            ),
            global: storage("tacklecast-fg-prep-global", 16, wgpu::BufferUsages::empty()),
            core_mask: storage(
                "tacklecast-fg-prep-core",
                pixels * 4,
                wgpu::BufferUsages::empty(),
            ),
            profiles: storage(
                "tacklecast-fg-prep-profiles",
                8 * (width as u64 + height as u64),
                wgpu::BufferUsages::COPY_SRC,
            ),
            fruc_input,
            fruc_input_view,
            bind_groups: HashMap::new(),
            tiles_x,
            tiles_y,
        });
        if let Some(error) = pollster::block_on(scope.pop()) {
            tracing::warn!(
                "frame generation HUD removal / motion estimate unavailable on this GPU: {error}"
            );
            self.prep = None;
            self.prep_failed = true;
            return false;
        }
        true
    }

    /// Profiles for the motion estimate and, when `hud` is set, the FRUC
    /// input with static HUD painted over. Returns true if `fruc_input`
    /// holds the image to hand to FRUC.
    #[allow(clippy::too_many_arguments)]
    fn encode_prep(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        (slot, reference): (usize, usize),
        hud: bool,
        guard: bool,
        motion: Option<[f32; 2]>,
    ) -> bool {
        let (width, height) = (self.width, self.height);
        let sources = &self.sources;
        let hud_bits = &self.hud_bits;
        let slot_offset = (slot * self.hud_words_per_slot) as u32;
        let prep = self.prep.as_mut().expect("ensured");
        // The fill uses the previous frame shifted by the camera motion;
        // only while that motion stays well inside the frame.
        let motion = motion
            .filter(|m| m[0].abs() < width as f32 * 0.25 && m[1].abs() < height as f32 * 0.25);
        let [mv_x, mv_y] = motion.map_or([0, 0], |m| [m[0].round() as i32, m[1].round() as i32]);
        let params = PrepParams {
            width,
            height,
            tiles_x: prep.tiles_x,
            tiles_y: prep.tiles_y,
            hud: u32::from(hud) | (u32::from(hud && guard) << 1),
            total_tiles: prep.tiles_x * prep.tiles_y,
            slot_offset,
            mv_valid: u32::from(guard && motion.is_some() && (mv_x != 0 || mv_y != 0)),
            static_thr: 3.0,
            // The guard acts only while the camera turns (85-90 % of the
            // screen moving in RDR2 recordings); wind in grass on a still
            // camera moves about a third of the screen.
            global_motion: if guard { 0.55 } else { 0.30 },
            local_motion: 0.40,
            salience: 10.0,
            mv_x,
            mv_y,
            guard_window: self.guard_window,
            _pad1: 0,
        };
        // Queue writes land before the next submission (this encoder's).
        queue.write_buffer(&prep.uniforms, 0, bytemuck::bytes_of(&params));
        let layout = &prep.layout;
        let confidence = &prep.confidence;
        let (out_view, persistence, tile_moving, global, core_mask, profiles, uniforms) = (
            &prep.fruc_input_view,
            &prep.persistence,
            &prep.tile_moving,
            &prep.global,
            &prep.core_mask,
            &prep.profiles,
            &prep.uniforms,
        );
        let bind_group = prep
            .bind_groups
            .entry((slot, reference))
            .or_insert_with(|| {
                let entries = [
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&sources[slot].view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&sources[reference].view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(out_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: persistence.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: tile_moving.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: global.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 6,
                        resource: core_mask.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 7,
                        resource: profiles.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 8,
                        resource: uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 9,
                        resource: hud_bits.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 10,
                        resource: confidence.as_entire_binding(),
                    },
                ];
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("tacklecast-fg-prep-bind-group"),
                    layout,
                    entries: &entries,
                })
            })
            .clone();
        encoder.clear_buffer(&prep.global, 0, None);

        let pixel_groups = (width.div_ceil(8), height.div_ceil(8));
        let mut passes: Vec<(&wgpu::ComputePipeline, (u32, u32), &str)> = vec![(
            &prep.profile,
            ((width + height).div_ceil(64), 1),
            "tacklecast-fg-prep-profile",
        )];
        if hud {
            passes.push((&prep.persist, pixel_groups, "tacklecast-fg-prep-persist"));
            passes.push((&prep.core, pixel_groups, "tacklecast-fg-prep-core"));
            if guard {
                passes.push((&prep.isolate, pixel_groups, "tacklecast-fg-prep-isolate"));
            }
            passes.push((&prep.fill, pixel_groups, "tacklecast-fg-prep-fill"));
        }
        let timer = self.prep_timer.as_mut();
        let timed = timer.is_some_and(|t| t.arm());
        let count = passes.len();
        for (index, (pipeline, (gx, gy), label)) in passes.into_iter().enumerate() {
            let timestamp_writes = if timed {
                self.prep_timer
                    .as_ref()
                    .and_then(|t| t.writes(index == 0, index + 1 == count))
            } else {
                None
            };
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some(label),
                timestamp_writes,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(gx, gy, 1);
        }
        if let Some(timer) = self.prep_timer.as_mut() {
            timer.finish(encoder);
        }
        hud
    }

    /// Copy an interpolated output buffer (written by CUDA) to its texture.
    /// Map the capture-side timing readback used by the encoder just
    /// submitted (never waits).
    pub fn after_capture_submit(&mut self) {
        if let Some(timer) = self.prep_timer.as_mut() {
            timer.after_submit();
        }
    }

    fn ensure_flow_warp(&mut self, device: &wgpu::Device) -> bool {
        if self.flow_warp.is_some() {
            return true;
        }
        if self.flow_warp_failed {
            return false;
        }
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let entry = |binding: u32, ty: wgpu::BufferBindingType| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let read = wgpu::BufferBindingType::Storage { read_only: true };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tacklecast-fg-warp-layout"),
            entries: &[
                entry(0, wgpu::BufferBindingType::Uniform),
                entry(1, read),
                entry(2, read),
                entry(3, read),
                entry(4, wgpu::BufferBindingType::Storage { read_only: false }),
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tacklecast-fg-warp-shader"),
            source: wgpu::ShaderSource::Wgsl(WARP_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tacklecast-fg-warp-pipeline-layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("tacklecast-fg-warp-pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("warp"),
            compilation_options: Default::default(),
            cache: None,
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tacklecast-fg-warp-uniforms"),
            size: std::mem::size_of::<WarpParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let field = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tacklecast-fg-warp-field"),
            size: self.pitch as u64 * self.height as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if let Some(e) = pollster::block_on(scope.pop()) {
            tracing::warn!("frame generation: flow warp unavailable: {e}");
            self.flow_warp_failed = true;
            return false;
        }
        self.flow_warp = Some(FlowWarp {
            pipeline,
            layout,
            uniforms,
            field,
            bind_groups: HashMap::new(),
        });
        true
    }

    /// Compose the generated image in output `slot` from the flow + mask the
    /// interpolator left there, sampling source slots `prev` and `next`
    /// (the same, HUD-filled, images the network saw). Afterwards the slot
    /// holds an RGBA image like every other backend's output.
    pub fn encode_flow_warp(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        (slot, prev, next): (usize, usize, usize),
    ) -> bool {
        if slot >= self.outputs.len() || prev >= self.sources.len() || next >= self.sources.len() {
            return false;
        }
        if !self.ensure_flow_warp(device) {
            return false;
        }
        let (width, height, pitch) = (self.width, self.height, self.pitch);
        let warp = self.flow_warp.as_mut().expect("ensured");
        let params = WarpParams {
            width,
            height,
            half_w: width / 2,
            half_h: height / 2,
            contrast: warp_contrast(),
            row_words: pitch / 4,
            _pad0: 0,
            _pad1: 0,
        };
        queue.write_buffer(&warp.uniforms, 0, bytemuck::bytes_of(&params));
        encoder.copy_buffer_to_buffer(
            &self.outputs[slot].buffer,
            0,
            &warp.field,
            0,
            pitch as u64 * height as u64,
        );
        let (layout, uniforms, field) = (&warp.layout, &warp.uniforms, &warp.field);
        let (sources, outputs) = (&self.sources, &self.outputs);
        let bind_group = warp
            .bind_groups
            .entry((slot, prev, next))
            .or_insert_with(|| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("tacklecast-fg-warp-bind-group"),
                    layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniforms.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: field.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: sources[prev].buffer.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: sources[next].buffer.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: outputs[slot].buffer.as_entire_binding(),
                        },
                    ],
                })
            });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("tacklecast-fg-warp"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&warp.pipeline);
        pass.set_bind_group(0, &*bind_group, &[]);
        pass.dispatch_workgroups(width.div_ceil(8), height.div_ceil(8), 1);
        true
    }

    pub fn encode_publish_output(&self, encoder: &mut wgpu::CommandEncoder, slot: usize) {
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &self.outputs[slot].buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.pitch),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture: &self.outputs[slot].texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    pub fn image_slot(&self, image: FgImage) -> Option<&FgSlot> {
        match image {
            FgImage::Source(i) => self.sources.get(i),
            FgImage::Generated(i) => self.outputs.get(i),
        }
    }
}

impl Renderer {
    /// Allocate frame-generation resources for frames of `width`x`height`.
    pub fn fg_prepare(
        &mut self,
        width: u32,
        height: u32,
        source_count: usize,
        output_count: usize,
    ) -> Result<FgImport, String> {
        self.fg_release();
        let (gpu, import) = FgGpu::new(
            &self.device,
            self.surface_format,
            width,
            height,
            source_count,
            output_count,
        )?;
        self.fg_gpu = Some(gpu);
        Ok(import)
    }

    /// Detach frame-generation resources. The caller keeps them alive until
    /// the CUDA worker has released its imports, then drops them.
    pub fn fg_release(&mut self) -> Option<FgGpu> {
        self.fg_display = None;
        self.fg_gpu.take()
    }

    pub fn current_video_size(&self) -> Option<(u32, u32)> {
        self.video_frame.as_ref().map(|v| (v.width, v.height))
    }

    pub fn device_handle(&self) -> wgpu::Device {
        self.device.clone()
    }

    pub fn set_fg_display(&mut self, image: Option<FgImage>) {
        self.fg_display = image;
    }

    /// Convert the current YUV frame into source slot `slot`, copy it to the
    /// shared buffer and queue the duplicate comparison against `reference`.
    pub fn fg_capture(
        &mut self,
        slot: usize,
        reference: Option<usize>,
        protection: FgProtection,
    ) -> Option<wgpu::SubmissionIndex> {
        let video_frame = self.video_frame.as_ref()?;
        let generation = self.video_generation;
        let fg = self.fg_gpu.as_mut()?;
        if video_frame.width != fg.width || video_frame.height != fg.height {
            return None;
        }
        if fg.capture_pipeline.is_none() {
            let shader = self
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("tacklecast-fg-capture-shader"),
                    source: wgpu::ShaderSource::Wgsl(VIDEO_SHADER.into()),
                });
            let layout = self
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("tacklecast-fg-capture-layout"),
                    bind_group_layouts: &[Some(&self.video_bind_group_layout)],
                    immediate_size: 0,
                });
            fg.capture_pipeline = Some(self.device.create_render_pipeline(
                &wgpu::RenderPipelineDescriptor {
                    label: Some("tacklecast-fg-capture-pipeline"),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some("fs_main"),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: FG_FORMAT,
                            blend: Some(wgpu::BlendState::REPLACE),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                },
            ));
        }
        if fg
            .capture_binding
            .as_ref()
            .is_none_or(|b| b.generation != generation)
        {
            let y = video_frame
                .y_texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let u = video_frame
                .u_texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let v = video_frame
                .v_texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("tacklecast-fg-capture-bind-group"),
                layout: &self.video_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&y),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&u),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&v),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&self.video_samplers.filtering),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: fg.capture_uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: wgpu::BindingResource::Sampler(&self.video_samplers.nearest),
                    },
                ],
            });
            fg.capture_binding = Some(CaptureBinding {
                generation,
                bind_group,
            });
        }

        let resolved_space = self
            .source_color
            .resolve_space(self.color_space, video_frame.height);
        let resolved_range = self.source_color.resolve_range(self.color_range);
        let filter_mode = match self.scale_filter {
            ScaleFilter::Fsr1 | ScaleFilter::RtxVsr => 0,
            other => other.as_u32(),
        };
        let uniforms = VideoUniforms {
            format_mode: VideoUniforms::format_mode_for(video_frame.format),
            filter_mode,
            color_space: resolved_space.as_u32(),
            color_range: resolved_range.as_u32(),
            viewport_size: [video_frame.width as f32, video_frame.height as f32],
            _padding: [0.0; 2],
            brightness: self.brightness,
            contrast: self.contrast,
            saturation: self.saturation,
            gamma: self.gamma,
        };
        self.queue
            .write_buffer(&fg.capture_uniforms, 0, bytemuck::bytes_of(&uniforms));

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tacklecast-fg-capture-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                multiview_mask: None,
                label: Some("tacklecast-fg-capture-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    depth_slice: None,
                    view: &fg.sources[slot].view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR_COLOR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(fg.capture_pipeline.as_ref().expect("created above"));
            pass.set_bind_group(
                0,
                &fg.capture_binding
                    .as_ref()
                    .expect("created above")
                    .bind_group,
                &[],
            );
            pass.set_viewport(0.0, 0.0, fg.width as f32, fg.height as f32, 0.0, 1.0);
            pass.draw(0..6, 0..1);
        }
        fg.encode_transfer(
            &self.device,
            &self.queue,
            &mut encoder,
            slot,
            reference,
            protection,
        );
        let index = self.queue.submit(std::iter::once(encoder.finish()));
        fg.after_capture_submit();
        Some(index)
    }

    /// Make an interpolated output (finished by CUDA) visible to wgpu,
    /// applying artifact protection against the two real frames around it.
    pub fn fg_publish_output(
        &mut self,
        slot: usize,
        prev: Option<usize>,
        next: usize,
        protection: FgProtection,
    ) {
        let Some(fg) = self.fg_gpu.as_mut() else {
            return;
        };
        if slot >= fg.outputs.len() || next >= fg.sources.len() {
            return;
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tacklecast-fg-publish-encoder"),
            });
        if fg.flow_output {
            // Flow + mask in the slot: compose the image first; without
            // both real frames there is nothing valid to show.
            let composed = prev.filter(|p| *p != next).is_some_and(|p| {
                fg.encode_flow_warp(&self.device, &self.queue, &mut encoder, (slot, p, next))
            });
            if !composed {
                return;
            }
        }
        match prev.filter(|p| *p < fg.sources.len() && *p != next) {
            Some(prev) if protection.any() => fg.encode_publish_corrected(
                &self.device,
                &self.queue,
                &mut encoder,
                (slot, prev, next),
                protection,
            ),
            _ => fg.encode_publish_output(&mut encoder, slot),
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        fg.after_publish_submit();
    }

    /// Output slots hold flow + mask (RIFE `fm` models) from now on.
    pub fn fg_set_flow_output(&mut self, flow_output: bool) {
        if let Some(fg) = self.fg_gpu.as_mut() {
            fg.flow_output = flow_output;
        }
    }

    /// Artifact-correction statistics and GPU pass times.
    pub fn fg_report(&self) -> Option<FixReport> {
        self.fg_gpu.as_ref().map(|fg| fg.fix_report())
    }

    /// Draw the selected frame-generation image into the swapchain view.
    /// Returns false when nothing was drawn (caller falls back to YUV).
    pub(super) fn encode_fg_display(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
    ) -> bool {
        let Some(image) = self.fg_display else {
            return false;
        };
        let Some(fg) = self.fg_gpu.as_ref() else {
            return false;
        };
        let Some(slot) = fg.image_slot(image) else {
            return false;
        };
        let viewport = calculate_video_viewport(
            self.size.width as f32,
            self.size.height as f32,
            fg.width as f32,
            fg.height as f32,
        );
        let is_upscaling = viewport.width > fg.width as f32 || viewport.height > fg.height as f32;
        let is_equal = viewport.width == fg.width as f32 && viewport.height == fg.height as f32;

        if matches!(self.scale_filter, ScaleFilter::Fsr1 | ScaleFilter::RtxVsr) && is_upscaling {
            // RTX VSR consumes the YUV pass directly and is not wired to the
            // frame-generation output; FSR 1.0 is used instead.
            self.active_scale_filter = if self.scale_filter == ScaleFilter::RtxVsr {
                ActiveScaleFilter::RtxVsrFallbackFsr1
            } else {
                ActiveScaleFilter::Fsr1
            };
            let out_w = viewport.width.round().max(1.0) as u32;
            let out_h = viewport.height.round().max(1.0) as u32;
            let (fg_w, fg_h) = (fg.width, fg.height);
            Self::ensure_upscale_resources(
                &self.device,
                &self.video_bind_group_layout,
                self.surface_format,
                &mut self.upscale_pipelines,
                &mut self.upscale_textures,
                fg_w,
                fg_h,
                out_w,
                out_h,
            );
            let pipelines = self.upscale_pipelines.as_ref().expect("ensured");
            let textures = self.upscale_textures.as_ref().expect("ensured");
            let easu =
                calculate_easu_constants(fg_w as f32, fg_h as f32, out_w as f32, out_h as f32);
            self.queue
                .write_buffer(&pipelines.easu_uniforms, 0, bytemuck::bytes_of(&easu));
            let rcas = RcasConstants::new(self.sharpness, out_w as f32, out_h as f32);
            self.queue
                .write_buffer(&pipelines.rcas_uniforms, 0, bytemuck::bytes_of(&rcas));
            let fg = self.fg_gpu.as_ref().expect("checked");
            let slot = fg.image_slot(image).expect("checked");
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &slot.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &textures.native_rgb_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: fg_w,
                    height: fg_h,
                    depth_or_array_layers: 1,
                },
            );
            encode_easu_rcas(encoder, pipelines, textures, view, viewport, out_w, out_h);
            return true;
        }

        self.active_scale_filter = match self.scale_filter {
            ScaleFilter::Bilinear => ActiveScaleFilter::Bilinear,
            ScaleFilter::Bicubic => ActiveScaleFilter::Bicubic,
            ScaleFilter::Lanczos => ActiveScaleFilter::Lanczos,
            ScaleFilter::Fsr1 if is_equal => ActiveScaleFilter::Fsr1FallbackDirect,
            ScaleFilter::Fsr1 => ActiveScaleFilter::Fsr1FallbackBilinear,
            ScaleFilter::RtxVsr if is_equal => ActiveScaleFilter::RtxVsrFallbackDirect,
            ScaleFilter::RtxVsr => ActiveScaleFilter::RtxVsrFallbackBilinear,
        };
        let filter_mode = match self.scale_filter {
            ScaleFilter::Fsr1 | ScaleFilter::RtxVsr => 0,
            other => other.as_u32(),
        };
        let uniforms = BlitUniforms {
            filter_mode,
            _pad: 0,
            viewport_size: [viewport.width, viewport.height],
        };
        self.queue
            .write_buffer(&fg.blit_uniforms, 0, bytemuck::bytes_of(&uniforms));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            multiview_mask: None,
            label: Some("tacklecast-fg-display-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                depth_slice: None,
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(CLEAR_COLOR),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&fg.blit_pipeline);
        pass.set_bind_group(0, &slot.blit_bind_group, &[]);
        pass.set_viewport(
            viewport.x,
            viewport.y,
            viewport.width.max(1.0),
            viewport.height.max(1.0),
            0.0,
            1.0,
        );
        pass.draw(0..6, 0..1);
        true
    }
}

/// EASU + RCAS passes from `textures.native_rgb_texture` to `view`.
pub(super) fn encode_easu_rcas(
    encoder: &mut wgpu::CommandEncoder,
    pipelines: &UpscalePipelines,
    textures: &UpscaleTextures,
    view: &wgpu::TextureView,
    viewport: Viewport,
    out_w: u32,
    out_h: u32,
) {
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            multiview_mask: None,
            label: Some("tacklecast-fg-easu-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                depth_slice: None,
                view: &textures.easu_output_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(CLEAR_COLOR),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&pipelines.easu_pipeline);
        pass.set_bind_group(0, &textures.easu_bind_group, &[]);
        pass.set_viewport(0.0, 0.0, out_w as f32, out_h as f32, 0.0, 1.0);
        pass.draw(0..6, 0..1);
    }
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        multiview_mask: None,
        label: Some("tacklecast-fg-rcas-pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            depth_slice: None,
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(CLEAR_COLOR),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_pipeline(&pipelines.rcas_pipeline);
    pass.set_bind_group(0, &textures.rcas_bind_group, &[]);
    pass.set_viewport(
        viewport.x,
        viewport.y,
        viewport.width.max(1.0),
        viewport.height.max(1.0),
        0.0,
        1.0,
    );
    pass.draw(0..6, 0..1);
}

/// Presents an RGBA frame with the same filter choices as `VIDEO_SHADER`
/// (bilinear, Catmull-Rom bicubic, adaptive Lanczos-2/3), applied to all
/// channels at once. Colour conversion and picture adjustments were already
/// applied when the frame was captured into the slot.
const RGB_BLIT_SHADER: &str = r#"
struct BlitUniforms {
    filter_mode: u32,
    _pad: u32,
    viewport_size: vec2<f32>,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var nearest_sampler: sampler;
@group(0) @binding(3) var<uniform> uniforms: BlitUniforms;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOut {
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0, 1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
    );
    var uvs = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(1.0, 0.0),
    );
    var out: VertexOut;
    out.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    out.uv = uvs[vertex_index];
    return out;
}

fn cubic_weight(x: f32) -> f32 {
    let a = -0.5;
    let ax = abs(x);
    if ax <= 1.0 {
        return (a + 2.0) * ax * ax * ax - (a + 3.0) * ax * ax + 1.0;
    } else if ax < 2.0 {
        return a * ax * ax * ax - 5.0 * a * ax * ax + 8.0 * a * ax - 4.0 * a;
    }
    return 0.0;
}

fn sinc(x: f32) -> f32 {
    if abs(x) < 1e-5 { return 1.0; }
    let px = 3.14159265 * x;
    return sin(px) / px;
}

fn lanczos_weight(x: f32, a: f32) -> f32 {
    if abs(x) >= a { return 0.0; }
    return sinc(x) * sinc(x / a);
}

// kernel: 0 = bicubic (4 taps), 2 = lanczos-2 (4 taps), 3 = lanczos-3 (6 taps)
fn sample_separable(uv: vec2<f32>, kernel: u32) -> vec4<f32> {
    let dims = vec2<f32>(textureDimensions(src_tex));
    let texel = uv * dims - 0.5;
    let base = floor(texel);
    let frac = texel - base;
    var taps = 4;
    var first = -1;
    if kernel == 3u {
        taps = 6;
        first = -2;
    }
    var wx: array<f32, 6>;
    var wy: array<f32, 6>;
    var wx_sum = 0.0;
    var wy_sum = 0.0;
    for (var k = 0; k < taps; k = k + 1) {
        let offset = f32(k + first);
        if kernel == 0u {
            wx[k] = cubic_weight(offset - frac.x);
            wy[k] = cubic_weight(offset - frac.y);
        } else {
            let a = f32(kernel);
            wx[k] = lanczos_weight(offset - frac.x, a);
            wy[k] = lanczos_weight(offset - frac.y, a);
        }
        wx_sum = wx_sum + wx[k];
        wy_sum = wy_sum + wy[k];
    }
    var sum = vec4<f32>(0.0);
    for (var j = 0; j < taps; j = j + 1) {
        var row = vec4<f32>(0.0);
        for (var i = 0; i < taps; i = i + 1) {
            let pos = (base + vec2<f32>(f32(i + first), f32(j + first)) + 0.5) / dims;
            row = row + textureSample(src_tex, nearest_sampler, pos) * wx[i];
        }
        sum = sum + row * wy[j];
    }
    return sum / max(wx_sum * wy_sum, 1e-5);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let src_dims = vec2<f32>(textureDimensions(src_tex));
    let scale = uniforms.viewport_size / max(src_dims, vec2<f32>(1.0));
    let max_scale = max(scale.x, scale.y);
    var color: vec4<f32>;
    if max_scale <= 1.0 || uniforms.filter_mode == 0u {
        color = textureSample(src_tex, linear_sampler, in.uv);
    } else if uniforms.filter_mode == 1u {
        color = sample_separable(in.uv, 0u);
    } else if max_scale > 2.0 {
        color = sample_separable(in.uv, 3u);
    } else {
        color = sample_separable(in.uv, 2u);
    }
    return vec4<f32>(clamp(color.rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

#[cfg(test)]
mod gpu_tests {
    //! Run the frame-generation shaders on a real wgpu adapter (any
    //! backend). Explicit, like the other GPU tests:
    //! `cargo test fg_gpu -- --ignored`.
    use super::*;
    use crate::framegen::duplicate::{is_duplicate, DuplicateThresholds};

    const W: u32 = 256;
    const H: u32 = 128;

    fn device() -> (wgpu::Device, wgpu::Queue) {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("A wgpu adapter is required for this explicit graphics test");
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap()
    }

    fn pattern(shift: u32) -> Vec<u8> {
        let mut data = vec![0_u8; (W * H * 4) as usize];
        for y in 0..H {
            for x in 0..W {
                let i = ((y * W + x) * 4) as usize;
                let v = if ((x + shift) / 8 + y / 8).is_multiple_of(2) {
                    220
                } else {
                    30
                };
                data[i] = v;
                data[i + 1] = (x * 255 / W) as u8;
                data[i + 2] = (y * 255 / H) as u8;
                data[i + 3] = 255;
            }
        }
        data
    }

    fn write(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8]) {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(W * 4),
                rows_per_image: Some(H),
            },
            wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
        );
    }

    fn read_buffer(device: &wgpu::Device, queue: &wgpu::Queue, src: &wgpu::Buffer) -> Vec<u8> {
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: src.size(),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(src, 0, &staging, 0, src.size());
        queue.submit(std::iter::once(encoder.finish()));
        staging.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("GPU test wait failed");
        let out = staging.slice(..).get_mapped_range().to_vec();
        staging.unmap();
        out
    }

    fn read_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
    ) -> Vec<u8> {
        let (w, h) = (texture.width(), texture.height());
        let pitch = fg_pitch(w);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (pitch * h) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(pitch),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));
        buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("GPU test wait failed");
        let mapped = buffer.slice(..).get_mapped_range();
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for row in 0..h {
            let start = (row * pitch) as usize;
            out.extend_from_slice(&mapped[start..start + (w * 4) as usize]);
        }
        out
    }

    fn stats_after(
        gpu: &mut FgGpu,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        slot: usize,
        reference: usize,
    ) -> DiffStats {
        let mut encoder = device.create_command_encoder(&Default::default());
        gpu.encode_transfer(
            device,
            queue,
            &mut encoder,
            slot,
            Some(reference),
            FgProtection::default(),
        );
        queue.submit(std::iter::once(encoder.finish()));
        let readback = &gpu.source_extras[slot].readback;
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("GPU test wait failed");
        let stats =
            bytemuck::pod_read_unaligned::<DiffStats>(&readback.slice(..).get_mapped_range()[..16]);
        readback.unmap();
        stats
    }

    /// Deterministic noise in 0..=255 (stands in for detailed game scenery).
    fn noise(x: i32, y: i32) -> u8 {
        let mut h = (x as u32).wrapping_mul(374_761_393) ^ (y as u32).wrapping_mul(668_265_263);
        h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
        (h >> 24) as u8
    }

    /// Background scrolled by `scroll` px plus optional overlays:
    /// `hud` = a static minimap-like ring at (40,40), `menu` = a menu panel.
    fn scene(scroll: i32, hud_offset: Option<i32>, menu: bool) -> Vec<u8> {
        let mut data = vec![0_u8; (W * H * 4) as usize];
        for y in 0..H as i32 {
            for x in 0..W as i32 {
                let v = noise(x + scroll, y);
                let mut rgb = [v, v / 2 + 40, 255 - v];
                if let Some(o) = hud_offset {
                    let (hx, hy) = (x - 40 - o, y - 40);
                    if (0..24).contains(&hx) && (0..24).contains(&hy) {
                        let ring = hx < 3 || hy < 3 || hx >= 21 || hy >= 21;
                        rgb = if ring { [250, 250, 250] } else { [20, 20, 20] };
                    }
                }
                if menu && (150..230).contains(&x) && (30..90).contains(&y) {
                    let line = (y - 30) % 10 < 2;
                    rgb = if line { [240, 240, 240] } else { [10, 10, 30] };
                }
                let i = ((y as u32 * W + x as u32) * 4) as usize;
                data[i..i + 3].copy_from_slice(&rgb);
                data[i + 3] = 255;
            }
        }
        data
    }

    fn pixel(data: &[u8], x: u32, y: u32) -> [u8; 3] {
        let i = ((y * W + x) * 4) as usize;
        [data[i], data[i + 1], data[i + 2]]
    }

    /// The panning scene at `scroll` (content moves 1 px left per scroll
    /// step, so B = scene(s + k) means camera motion g = [-k, 0]).
    fn pan(scroll: i32) -> Vec<u8> {
        scene(scroll, None, false)
    }

    fn mae(a: &[u8], b: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
        let mut sum = 0u64;
        let mut n = 0u64;
        for y in y0..y1 {
            for x in x0..x1 {
                let (pa, pb) = (pixel(a, x, y), pixel(b, x, y));
                for c in 0..3 {
                    sum += (pa[c] as i32 - pb[c] as i32).unsigned_abs() as u64;
                }
                n += 3;
            }
        }
        sum as f32 / n.max(1) as f32
    }

    /// Paint a typical optical-flow failure into `img`: the rectangle is
    /// filled with its left column stretched sideways (smear / tear).
    fn smear(img: &mut [u8], x0: u32, y0: u32, x1: u32, y1: u32) {
        for y in y0..y1 {
            let src = ((y * W + x0) * 4) as usize;
            let px: [u8; 4] = img[src..src + 4].try_into().unwrap();
            for x in x0..x1 {
                let i = ((y * W + x) * 4) as usize;
                img[i..i + 4].copy_from_slice(&px);
            }
        }
    }

    /// Draw `overlay` (RGB, alpha) into a rectangle of `img`.
    fn paint(
        img: &mut [u8],
        x0: u32,
        y0: u32,
        x1: u32,
        y1: u32,
        f: impl Fn(u32, u32, [u8; 3]) -> [u8; 3],
    ) {
        for y in y0..y1 {
            for x in x0..x1 {
                let i = ((y * W + x) * 4) as usize;
                let rgb = f(x, y, [img[i], img[i + 1], img[i + 2]]);
                img[i..i + 3].copy_from_slice(&rgb);
            }
        }
    }

    struct Fix {
        device: wgpu::Device,
        queue: wgpu::Queue,
        gpu: FgGpu,
        _import: FgImport,
    }

    impl Fix {
        fn new() -> Self {
            let (device, queue) = device();
            let (gpu, import) = FgGpu::new(&device, FG_FORMAT, W, H, 2, 1).unwrap();
            Self {
                device,
                queue,
                gpu,
                _import: import,
            }
        }

        /// One generated frame through the correction pass: returns the
        /// published output and the statistics measured for it.
        fn run(
            &mut self,
            a: &[u8],
            b: &[u8],
            raw: &[u8],
            motion: Option<[f32; 2]>,
            protection: FgProtection,
        ) -> (Vec<u8>, FixStats) {
            write(&self.queue, &self.gpu.sources[0].texture, a);
            write(&self.queue, &self.gpu.sources[1].texture, b);
            self.queue.write_buffer(&self.gpu.outputs[0].buffer, 0, raw);
            let mut encoder = self.device.create_command_encoder(&Default::default());
            let protection = FgProtection {
                motion,
                ..protection
            };
            self.gpu.encode_publish_corrected(
                &self.device,
                &self.queue,
                &mut encoder,
                (0, 0, 1),
                protection,
            );
            self.queue.submit(std::iter::once(encoder.finish()));
            self.gpu.after_publish_submit();
            self.device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("GPU test wait failed");
            self.gpu.poll_fix_stats();
            let out = read_texture(&self.device, &self.queue, &self.gpu.outputs[0].texture);
            let stats = self.gpu.fix_report().last.expect("statistics read back");
            (out, stats)
        }
    }

    fn dynamic() -> FgProtection {
        FgProtection {
            fix_static: true,
            fix_motion: true,
            fix_edges: true,
            fix_fades: true,
            ..Default::default()
        }
    }

    const GPU: &str = "requires a working wgpu adapter; run explicitly on desktop test machines";

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_fix_clean_interpolation_is_left_untouched_everywhere() {
        // Mixed-frame-rate regression: with a correct generated frame the
        // whole screen must stay interpolated — no 30 FPS rectangles, bands
        // or HUD islands, on any frame.
        let _ = GPU;
        let mut fix = Fix::new();
        for k in 0..20 {
            let s = 8 * k;
            let mut a = scene(s, Some(0), false);
            let mut b = scene(s + 8, Some(0), false);
            let mut truth = scene(s + 4, Some(0), false);
            // A translucent panel (static, background visible through it).
            for img in [&mut a, &mut b, &mut truth] {
                paint(img, 120, 90, 200, 120, |_, _, c| c.map(|v| v / 2));
            }
            let (out, stats) = fix.run(&a, &b, &truth, Some([-8.0, 0.0]), dynamic());
            assert_eq!(out, truth, "frame {k}: clean output was modified");
            assert!(stats.corrected_pct < 0.01, "frame {k}: {stats:?}");
            assert_eq!(stats.zone_pct, 0.0);
        }
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_fix_hud_dragged_by_flow_is_repaired_locally() {
        // Fixed minimap over a scrolling scene; FRUC dragged the HUD 4 px
        // with the background (ghost copy + hole).
        let mut fix = Fix::new();
        let a = scene(0, Some(0), false);
        let b = scene(8, Some(0), false);
        let truth = scene(4, Some(0), false);
        let mut raw = scene(4, Some(-4), false); // HUD dragged left
                                                 // Plus a ghost of the ring where it used to be.
        paint(&mut raw, 40, 40, 64, 43, |_, _, _| [180, 180, 180]);
        let before = mae(&raw, &truth, 30, 30, 74, 74);
        let (out, stats) = fix.run(&a, &b, &raw, Some([-8.0, 0.0]), dynamic());
        let after = mae(&out, &truth, 30, 30, 74, 74);
        assert!(before > 20.0, "test setup: raw error {before}");
        assert!(after < before * 0.15, "HUD error {before:.1} -> {after:.1}");
        // Far from the HUD the generated frame is untouched.
        assert_eq!(mae(&out, &raw, 120, 0, W, H), 0.0);
        // Correction is local: the HUD area plus its tile gate, not the frame.
        assert!(
            stats.corrected_pct > 0.5 && stats.corrected_pct < 6.0,
            "{stats:?}"
        );
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_fix_animated_hud_is_not_frozen() {
        // A HUD element that changes every frame (needle, counter) is not
        // static and does not follow the camera: no prediction applies, so
        // FRUC's output is kept rather than forced to a real frame.
        let mut fix = Fix::new();
        let needle = |img: &mut Vec<u8>, phase: u32| {
            paint(img, 200, 20, 232, 52, |x, y, _| {
                let on = (x + 2 * y + 7 * phase) % 11 < 4;
                if on {
                    [250, 220, 40]
                } else {
                    [30, 30, 30]
                }
            })
        };
        let (mut a, mut b, mut raw) = (pan(0), pan(8), pan(4));
        needle(&mut a, 0);
        needle(&mut b, 2);
        needle(&mut raw, 1);
        let (out, _) = fix.run(&a, &b, &raw, Some([-8.0, 0.0]), dynamic());
        assert_eq!(
            mae(&out, &raw, 203, 23, 229, 49),
            0.0,
            "animated HUD was overwritten"
        );
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_fix_smeared_scenery_during_a_pan_is_reconstructed() {
        let mut fix = Fix::new();
        let (a, b, truth) = (pan(100), pan(108), pan(104));
        let mut raw = truth.clone();
        smear(&mut raw, 90, 40, 150, 80);
        let (out, stats) = fix.run(&a, &b, &raw, Some([-8.0, 0.0]), dynamic());
        let before = mae(&raw, &truth, 90, 40, 150, 80);
        let after = mae(&out, &truth, 90, 40, 150, 80);
        assert!(
            after < before * 0.1,
            "smear error {before:.1} -> {after:.1}"
        );
        // The replacement is the motion-compensated midpoint (moves at 60).
        assert!(stats.motion_pct > 5.0, "{stats:?}");
        assert!(stats.corrected_pct < 12.0, "{stats:?}");
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_fix_leading_edge_band_follows_the_motion_and_disappears() {
        // Fast turn: 40 px per source frame (16% of the width). Content
        // entering on the right exists only in B; FRUC stretched it.
        let mut fix = Fix::new();
        let (a, b, truth) = (pan(0), pan(40), pan(20));
        let mut raw = truth.clone();
        smear(&mut raw, W - 20, 0, W, H);
        let (out, stats) = fix.run(&a, &b, &raw, Some([-40.0, 0.0]), dynamic());
        let before = mae(&raw, &truth, W - 20, 0, W, H);
        let after = mae(&out, &truth, W - 20, 0, W, H);
        assert!(
            after < before * 0.15,
            "entering band {before:.1} -> {after:.1}"
        );
        assert!(stats.edge_pct > 5.0 && stats.edge_pct < 9.0, "{stats:?}");
        // The left edge (content leaving) was fine and stays untouched.
        assert_eq!(mae(&out, &raw, 0, 0, 40, H), 0.0);

        // Camera stops: no band at all (dynamic, not a fixed strip).
        let still = pan(40);
        let (out, stats) = fix.run(&still, &still, &still, Some([0.0, 0.0]), dynamic());
        assert_eq!(out, still);
        assert!(
            stats.edge_pct == 0.0 && stats.corrected_pct < 0.01,
            "{stats:?}"
        );
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_fix_fading_ui_gets_the_halfway_state_not_a_ghost() {
        // A menu panel fading in over the darkened scrim RDR2 puts behind
        // its menus (still scene): same structure, more contrast in B.
        // FRUC warped it (shifted copy = ghost).
        let mut fix = Fix::new();
        let menu = |img: &mut Vec<u8>, alpha: f32| {
            paint(img, 150, 30, 230, 90, |x, y, _| {
                let scrim = 20.0 + (noise(x as i32, y as i32) / 16) as f32;
                let target = if (y - 30) % 10 < 2 { 240.0 } else { 10.0 };
                [scrim * (1.0 - alpha) + target * alpha; 3].map(|v| v as u8)
            })
        };
        let still = scene(0, None, false);
        let (mut a, mut b, mut truth) = (still.clone(), still.clone(), still.clone());
        menu(&mut a, 0.4);
        menu(&mut b, 0.8);
        // The true midpoint of a fade is the average of the two states.
        for i in 0..truth.len() {
            truth[i] = ((a[i] as u16 + b[i] as u16) / 2) as u8;
        }
        let mut raw = truth.clone();
        // Ghost: half of the panel shifted down by 3 px.
        for y in (60..90).rev() {
            for x in 150..230 {
                let (d, s) = (((y * W + x) * 4) as usize, (((y - 3) * W + x) * 4) as usize);
                let px: [u8; 4] = raw[s..s + 4].try_into().unwrap();
                raw[d..d + 4].copy_from_slice(&px);
            }
        }
        let before = mae(&raw, &truth, 150, 30, 230, 90);
        let (out, stats) = fix.run(&a, &b, &raw, Some([0.0, 0.0]), dynamic());
        let after = mae(&out, &truth, 150, 30, 230, 90);
        assert!(
            after < before * 0.25,
            "fade ghost {before:.1} -> {after:.1}"
        );
        assert!(stats.fade_pct > 0.0, "{stats:?}");
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_fix_scene_cut_is_not_patched_into_a_mess() {
        // Unrelated frames (cut): no prediction holds, so the pass must not
        // start replacing pixels (the controller skips the frame instead).
        let mut fix = Fix::new();
        let a = pan(0);
        let mut b = pan(0);
        for (i, v) in b.iter_mut().enumerate() {
            if i % 4 != 3 {
                *v = 255 - *v / 2;
            }
        }
        let raw: Vec<u8> = a
            .iter()
            .zip(&b)
            .map(|(x, y)| ((*x as u16 + *y as u16) / 2) as u8)
            .collect();
        let (_, stats) = fix.run(&a, &b, &raw, None, dynamic());
        assert!(stats.corrected_pct < 3.0, "{stats:?}");
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_fix_correction_stops_as_soon_as_the_artifact_is_gone() {
        // Temporal behaviour: an artifact on one frame, clean frames after.
        // The hysteresis must not keep stale corrections alive.
        let mut fix = Fix::new();
        let (a, b, truth) = (pan(0), pan(8), pan(4));
        let mut raw = truth.clone();
        smear(&mut raw, 60, 30, 120, 90);
        let (_, stats) = fix.run(&a, &b, &raw, Some([-8.0, 0.0]), dynamic());
        assert!(stats.corrected_pct > 5.0, "{stats:?}");
        for k in 1..4 {
            let (a, b, truth) = (pan(8 * k), pan(8 * k + 8), pan(8 * k + 4));
            let (out, stats) = fix.run(&a, &b, &truth, Some([-8.0, 0.0]), dynamic());
            assert_eq!(out, truth, "frame {k}");
            assert!(stats.corrected_pct < 0.01, "frame {k}: {stats:?}");
        }
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_fix_raw_view_and_off_mode_show_fruc_unchanged() {
        let mut fix = Fix::new();
        let (a, b) = (pan(0), pan(8));
        let mut raw = pan(4);
        smear(&mut raw, 60, 30, 120, 90);
        let view_raw = FgProtection {
            view: VIEW_RAW,
            ..dynamic()
        };
        let (out, stats) = fix.run(&a, &b, &raw, Some([-8.0, 0.0]), view_raw);
        assert_eq!(out, raw, "raw view must show FRUC output");
        assert!(
            stats.corrected_pct > 5.0,
            "corrections are still measured: {stats:?}"
        );
        // Manual regions are opt-in only.
        assert_eq!(FgProtection::default().zones, 0);
        assert!(
            !FgProtection::default().any(),
            "Off mode must skip the pass entirely"
        );
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_gpu_hud_removed_from_fruc_input_and_motion_measured() {
        let (device, queue) = device();
        let (mut gpu, _import) = FgGpu::new(&device, FG_FORMAT, W, H, 2, 1).unwrap();
        let advanced = FgProtection {
            hud_input_removal: true,
            ..dynamic()
        };
        let mut readback = Vec::new();
        let mut current = Vec::new();
        for k in 0..14 {
            let reference = scene(6 * k, Some(0), false);
            current = scene(6 * (k + 1), Some(0), false);
            write(&queue, &gpu.sources[0].texture, &reference);
            write(&queue, &gpu.sources[1].texture, &current);
            let mut encoder = device.create_command_encoder(&Default::default());
            gpu.encode_transfer(&device, &queue, &mut encoder, 1, Some(0), advanced);
            queue.submit(std::iter::once(encoder.finish()));
            let rb = &gpu.source_extras[1].readback;
            rb.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("GPU test wait failed");
            readback = rb.slice(..).get_mapped_range().to_vec();
            rb.unmap();
        }
        let fruc_input = read_buffer(&device, &queue, &gpu.sources[1].buffer);
        assert_eq!(pixel(&current, 41, 41), [250, 250, 250]);
        for (x, y) in [(41, 41), (52, 52), (62, 42)] {
            let px = pixel(&fruc_input, x, y);
            assert!(
                px != [250, 250, 250] && px != [20, 20, 20],
                "HUD pixel ({x},{y}) reached FRUC: {px:?}"
            );
        }
        for (x, y) in [(150, 64), (200, 20), (100, 110)] {
            assert_eq!(pixel(&fruc_input, x, y), pixel(&current, x, y));
        }
        let floats: Vec<f32> = readback[16..]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let (w, h) = (W as usize, H as usize);
        let (cur, prev) = floats.split_at(w + h);
        let motion =
            crate::framegen::motion::estimate_motion(&prev[..w], &cur[..w], &prev[w..], &cur[w..])
                .expect("motion estimate");
        assert!((motion.shift[0] + 6.0).abs() <= 1.0, "motion {motion:?}");
        assert!(motion.shift[1].abs() <= 1.0, "motion {motion:?}");
        assert!(motion.residual < 0.2, "motion {motion:?}");

        // Dynamic (default) mode: FRUC gets the exact frame.
        let mut encoder = device.create_command_encoder(&Default::default());
        gpu.encode_transfer(&device, &queue, &mut encoder, 1, Some(0), dynamic());
        queue.submit(std::iter::once(encoder.finish()));
        assert_eq!(
            read_buffer(&device, &queue, &gpu.sources[1].buffer),
            current
        );
    }

    /// Smooth scenery (sky and clouds) scrolling 1 px left per step.
    fn clouds(scroll: i32) -> Vec<u8> {
        let mut data = vec![0_u8; (W * H * 4) as usize];
        for y in 0..H as i32 {
            for x in 0..W as i32 {
                let t = (x + scroll) as f32;
                let v = 128.0 + 90.0 * (t / 37.0).sin() * (0.6 + 0.4 * (y as f32 / 23.0).cos());
                let v = v.clamp(0.0, 255.0) as u8;
                let i = ((y as u32 * W + x as u32) * 4) as usize;
                data[i..i + 4].copy_from_slice(&[v / 2 + 40, v / 2 + 60, v, 255]);
            }
        }
        data
    }

    /// A square HUD ring of `size` px (2 px wide) at (`x0`, `y0`).
    /// `alpha` 1.0 = opaque white ring with a dark centre (crosshair dot);
    /// 0.5 = 50 % white over whatever is behind, centre see-through.
    fn hud_ring(mut data: Vec<u8>, x0: u32, y0: u32, size: u32, alpha: f32) -> Vec<u8> {
        for y in y0..y0 + size {
            for x in x0..x0 + size {
                let (hx, hy) = (x - x0, y - y0);
                let ring = hx < 2 || hy < 2 || hx >= size - 2 || hy >= size - 2;
                let i = ((y * W + x) * 4) as usize;
                for c in 0..3 {
                    let bg = data[i + c] as f32;
                    data[i + c] = if ring {
                        (bg * (1.0 - alpha) + 250.0 * alpha).round() as u8
                    } else if alpha >= 1.0 {
                        20
                    } else {
                        data[i + c]
                    };
                }
            }
        }
        data
    }

    /// Runs `frames` capture-side passes of a camera panning `step` px per
    /// frame to the left, returning the last (reference, current) pair.
    fn pan_frames(
        gpu: &mut FgGpu,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frames: i32,
        step: i32,
        frame: &dyn Fn(i32) -> Vec<u8>,
        guard: FgProtection,
    ) -> (Vec<u8>, Vec<u8>) {
        let (mut reference, mut current) = (Vec::new(), Vec::new());
        for k in 0..frames {
            reference = frame(step * k);
            current = frame(step * (k + 1));
            write(queue, &gpu.sources[0].texture, &reference);
            write(queue, &gpu.sources[1].texture, &current);
            let mut encoder = device.create_command_encoder(&Default::default());
            gpu.encode_transfer(device, queue, &mut encoder, 1, Some(0), guard);
            queue.submit(std::iter::once(encoder.finish()));
            device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("GPU test wait failed");
        }
        (reference, current)
    }

    fn publish(
        gpu: &mut FgGpu,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        generated: &[u8],
        guard: FgProtection,
    ) -> Vec<u8> {
        queue.write_buffer(&gpu.outputs[0].buffer, 0, generated);
        let mut encoder = device.create_command_encoder(&Default::default());
        gpu.encode_publish_corrected(device, queue, &mut encoder, (0, 0, 1), guard);
        queue.submit(std::iter::once(encoder.finish()));
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("GPU test wait failed");
        read_texture(device, queue, &gpu.outputs[0].texture)
    }

    fn guard_on() -> FgProtection {
        FgProtection {
            hud_guard: true,
            motion: Some([-30.0, 0.0]),
            ..Default::default()
        }
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_gpu_hud_guard_restores_element_guarded_only_in_previous_frame() {
        // The guard learnt the marker in slot 1 (previous real frame) but has
        // no mask for slot 0 (next real frame).
        let (device, queue) = device();
        let (mut gpu, _import) = FgGpu::new(&device, FG_FORMAT, W, H, 2, 1).unwrap();
        gpu.guard_window = 0;
        let guard = guard_on();
        let frame = |s: i32| hud_ring(scene(s, None, false), 40, 40, 12, 1.0);
        let (_, current) = pan_frames(&mut gpu, &device, &queue, 14, 30, &frame, guard);
        let generated = scene(30 * 14 + 15, None, false);
        let run = |gpu: &mut FgGpu, next_real: &[u8]| -> Vec<u8> {
            write(&queue, &gpu.sources[0].texture, next_real);
            queue.write_buffer(&gpu.outputs[0].buffer, 0, &generated);
            let mut encoder = device.create_command_encoder(&Default::default());
            gpu.encode_publish_corrected(&device, &queue, &mut encoder, (0, 1, 0), guard);
            queue.submit(std::iter::once(encoder.finish()));
            device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("GPU test wait failed");
            read_texture(&device, &queue, &gpu.outputs[0].texture)
        };
        // Marker still there in the next real frame: drawn back.
        let next_real = frame(30 * 15);
        let out = run(&mut gpu, &next_real);
        for (x, y) in [(40, 40), (45, 45), (51, 46)] {
            assert_eq!(
                pixel(&out, x, y),
                pixel(&current, x, y),
                "marker at ({x},{y})"
            );
        }
        for (x, y) in [(150, 64), (200, 20)] {
            assert_eq!(pixel(&out, x, y), pixel(&generated, x, y));
        }
        // Marker gone from the next real frame: nothing is drawn back.
        let gone = scene(30 * 15, None, false);
        let out = run(&mut gpu, &gone);
        for (x, y) in [(40, 40), (45, 45)] {
            assert_eq!(
                pixel(&out, x, y),
                pixel(&generated, x, y),
                "stale marker at ({x},{y})"
            );
        }
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_gpu_hud_guard_fills_along_motion_and_restores_hud() {
        // A crosshair-sized (12 px) opaque marker over scenery panning
        // 30 px/frame to the left.
        let (device, queue) = device();
        let (mut gpu, _import) = FgGpu::new(&device, FG_FORMAT, W, H, 2, 1).unwrap();
        gpu.guard_window = 0;
        let guard = guard_on();
        let frame = |s: i32| hud_ring(scene(s, None, false), 40, 40, 12, 1.0);
        let (_, current) = pan_frames(&mut gpu, &device, &queue, 14, 30, &frame, guard);
        // The interpolator input shows the scenery behind the marker, taken
        // from the previous frame along the motion (exact here).
        let input = read_buffer(&device, &queue, &gpu.sources[1].buffer);
        let background = scene(30 * 14, None, false);
        for (x, y) in [(40, 40), (50, 41), (41, 50), (45, 45)] {
            assert_ne!(pixel(&current, x, y), pixel(&background, x, y));
            assert_eq!(
                pixel(&input, x, y),
                pixel(&background, x, y),
                "marker pixel ({x},{y}) not filled from the motion-shifted frame"
            );
        }
        for (x, y) in [(150, 64), (200, 20), (100, 110)] {
            assert_eq!(pixel(&input, x, y), pixel(&current, x, y));
        }
        // Publish: the interpolator produced the midpoint scenery only; the
        // guard draws the real marker back and leaves the rest as generated.
        let generated = scene(30 * 13 + 15, None, false);
        let out = publish(&mut gpu, &device, &queue, &generated, guard);
        for (x, y) in [(41, 41), (50, 42), (41, 50)] {
            assert_eq!(
                pixel(&out, x, y),
                [250, 250, 250],
                "marker ring at ({x},{y})"
            );
        }
        for (x, y) in [(150, 64), (200, 20), (100, 110), (90, 50)] {
            assert_eq!(
                pixel(&out, x, y),
                pixel(&generated, x, y),
                "scenery at ({x},{y})"
            );
        }

        // A static scene (no camera motion): nothing is treated as HUD.
        let still = frame(0);
        let (_, _) = pan_frames(&mut gpu, &device, &queue, 12, 0, &|_| still.clone(), guard);
        assert_eq!(read_buffer(&device, &queue, &gpu.sources[1].buffer), still);
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_gpu_hud_guard_keeps_translucent_hud() {
        // A 50 % white marker (RDR2's crosshair ring) over smooth panning
        // sky: its colour changes every frame, its shape does not.
        let (device, queue) = device();
        let (mut gpu, _import) = FgGpu::new(&device, FG_FORMAT, W, H, 2, 1).unwrap();
        gpu.guard_window = 0;
        let guard = guard_on();
        let frame = |s: i32| hud_ring(clouds(s), 40, 40, 12, 0.5);
        let (_, current) = pan_frames(&mut gpu, &device, &queue, 14, 30, &frame, guard);
        let input = read_buffer(&device, &queue, &gpu.sources[1].buffer);
        let removed = [(40, 40), (50, 41), (41, 50), (45, 51)]
            .iter()
            .filter(|&&(x, y)| pixel(&input, x, y) != pixel(&current, x, y))
            .count();
        assert!(
            removed >= 3,
            "translucent marker left in the input ({removed}/4 removed)"
        );
        let generated = clouds(30 * 13 + 15);
        let out = publish(&mut gpu, &device, &queue, &generated, guard);
        for (x, y) in [(40, 40), (50, 41), (41, 50)] {
            assert_eq!(
                pixel(&out, x, y),
                pixel(&current, x, y),
                "ring at ({x},{y})"
            );
        }
        for (x, y) in [(150, 64), (200, 20), (100, 110)] {
            assert_eq!(
                pixel(&out, x, y),
                pixel(&generated, x, y),
                "scenery at ({x},{y})"
            );
        }
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_gpu_hud_guard_leaves_large_hud_to_the_interpolator() {
        // A minimap-sized ring (70 px) over panning scenery: the
        // interpolator handles it; the guard must not repaint it.
        let (device, queue) = device();
        let (mut gpu, _import) = FgGpu::new(&device, FG_FORMAT, W, H, 2, 1).unwrap();
        gpu.guard_window = 0;
        let guard = guard_on();
        let frame = |s: i32| hud_ring(scene(s, None, false), 30, 20, 70, 1.0);
        let (_, current) = pan_frames(&mut gpu, &device, &queue, 14, 30, &frame, guard);
        let input = read_buffer(&device, &queue, &gpu.sources[1].buffer);
        let changed: Vec<(u32, u32)> = (0..H)
            .flat_map(|y| (0..W).map(move |x| (x, y)))
            .filter(|&(x, y)| pixel(&input, x, y) != pixel(&current, x, y))
            .collect();
        assert!(
            changed.is_empty(),
            "large HUD repainted at {} px, e.g. {:?}",
            changed.len(),
            &changed[..changed.len().min(12)]
        );
        let generated = scene(30 * 13 + 15, None, false);
        let out = publish(&mut gpu, &device, &queue, &generated, guard);
        assert!(
            out == generated,
            "large HUD restored over the generated frame"
        );
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_gpu_hud_guard_only_near_the_centre() {
        // Default window: a crosshair-sized marker at the centre is guarded,
        // the same marker away from the centre (e.g. on the player's hat)
        // is left to the interpolator.
        let (device, queue) = device();
        let (mut gpu, _import) = FgGpu::new(&device, FG_FORMAT, W, H, 2, 1).unwrap();
        let guard = guard_on();
        let (cx, cy) = (W / 2 - 6, H / 2 - 6);
        let frame = |s: i32| {
            hud_ring(
                hud_ring(scene(s, None, false), cx, cy, 12, 1.0),
                200,
                20,
                12,
                1.0,
            )
        };
        let (_, current) = pan_frames(&mut gpu, &device, &queue, 14, 30, &frame, guard);
        let input = read_buffer(&device, &queue, &gpu.sources[1].buffer);
        assert_ne!(
            pixel(&input, cx, cy),
            pixel(&current, cx, cy),
            "centre marker not guarded"
        );
        for (x, y) in [(200, 20), (211, 31), (205, 25)] {
            assert_eq!(
                pixel(&input, x, y),
                pixel(&current, x, y),
                "off-centre marker touched at ({x},{y})"
            );
        }
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_gpu_duplicate_detection_and_transfer() {
        let (device, queue) = device();
        let (mut gpu, import) = FgGpu::new(&device, FG_FORMAT, W, H, 3, 1).unwrap();
        assert_eq!(import.pitch, W * 4);
        let a = pattern(0);
        write(&queue, &gpu.sources[0].texture, &a);
        write(&queue, &gpu.sources[1].texture, &a);
        let same = stats_after(&mut gpu, &device, &queue, 1, 0);
        assert_eq!(same.samples, (W / 4) * (H / 4));
        assert_eq!(same.sum_fixed, 0);
        assert_eq!(same.changed, 0);
        assert!(is_duplicate(&same, &DuplicateThresholds::default()));
        // The shared buffer holds an exact copy of the slot texture.
        assert_eq!(read_buffer(&device, &queue, &gpu.sources[1].buffer), a);

        write(&queue, &gpu.sources[2].texture, &pattern(3));
        let moved = stats_after(&mut gpu, &device, &queue, 2, 0);
        assert!(moved.changed > moved.samples / 10, "{moved:?}");
        assert!(moved.mean_levels() > 10.0, "{moved:?}");
        assert!(!is_duplicate(&moved, &DuplicateThresholds::default()));
        // Repeated use of the same storage must start from zero each time.
        let again = stats_after(&mut gpu, &device, &queue, 2, 0);
        assert_eq!(again, moved);
    }

    fn f16_bits(v: f32) -> u16 {
        if v == 0.0 {
            return 0;
        }
        let b = v.to_bits();
        let sign = ((b >> 16) & 0x8000) as u16;
        let exp = ((b >> 23) & 0xff) as i32 - 127 + 15;
        let mant = ((b >> 13) & 0x3ff) as u16;
        sign | ((exp as u16) << 10) | mant
    }

    /// Half-res flow/mask field as the `fm` RIFE models write it.
    fn flow_field(f0: [f32; 2], f1: [f32; 2], mask: f32) -> Vec<u8> {
        let mut out = Vec::with_capacity((W * H * 4) as usize);
        for _ in 0..(W / 2) * (H / 2) {
            for v in [f0[0], f0[1], f1[0], f1[1], mask, 0.0, 0.0, 0.0] {
                out.extend_from_slice(&f16_bits(v).to_le_bytes());
            }
        }
        out
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_gpu_flow_warp_composes_from_flow_and_mask() {
        let (device, queue) = device();
        let (mut gpu, _import) = FgGpu::new(&device, FG_FORMAT, W, H, 2, 1).unwrap();
        gpu.flow_output = true;
        let (a, b) = (pattern(1), pattern(9));
        queue.write_buffer(&gpu.sources[0].buffer, 0, &a);
        queue.write_buffer(&gpu.sources[1].buffer, 0, &b);
        let run = |gpu: &mut FgGpu, field: Vec<u8>| -> Vec<u8> {
            queue.write_buffer(&gpu.outputs[0].buffer, 0, &field);
            let mut encoder = device.create_command_encoder(&Default::default());
            assert!(gpu.encode_flow_warp(&device, &queue, &mut encoder, (0, 0, 1)));
            queue.submit(std::iter::once(encoder.finish()));
            read_buffer(&device, &queue, &gpu.outputs[0].buffer)
        };
        let rgb = |img: &[u8], x: u32, y: u32| pixel(img, x.min(W - 1), y.min(H - 1));
        // Mask 1: frame 0 sampled along its flow (whole pixels: exact).
        let out = run(&mut gpu, flow_field([4.0, 0.0], [0.0, 0.0], 1.0));
        for (x, y) in [(0, 0), (10, 7), (W - 3, H / 2), (W / 2, H - 1)] {
            assert_eq!(rgb(&out, x, y), rgb(&a, x + 4, y), "frame 0 at {x},{y}");
        }
        // Mask 0: frame 1 along its own flow, clamped at the border.
        let out = run(&mut gpu, flow_field([4.0, 0.0], [0.0, -2.0], 0.0));
        for (x, y) in [(0, 0), (10, 7), (W - 3, H / 2), (W / 2, H - 1)] {
            assert_eq!(
                rgb(&out, x, y),
                rgb(&b, x, y.saturating_sub(2)),
                "frame 1 at {x},{y}"
            );
        }
        // Mask 0.5, no motion: the average of both frames.
        let out = run(&mut gpu, flow_field([0.0, 0.0], [0.0, 0.0], 0.5));
        for (x, y) in [(3, 3), (W / 2, H / 3)] {
            let (p, q, o) = (rgb(&a, x, y), rgb(&b, x, y), rgb(&out, x, y));
            for c in 0..3 {
                let avg = (p[c] as i32 + q[c] as i32) as f32 / 2.0;
                assert!(
                    (o[c] as f32 - avg).abs() <= 1.0,
                    "blend at {x},{y}: {o:?} vs {p:?}/{q:?}"
                );
            }
        }
    }

    #[test]
    #[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
    fn fg_gpu_output_publish_and_blit() {
        let (device, queue) = device();
        let (gpu, _import) = FgGpu::new(&device, FG_FORMAT, W, H, 2, 1).unwrap();
        let b = pattern(5);
        // CUDA writes the output buffer; emulate that and publish it.
        queue.write_buffer(&gpu.outputs[0].buffer, 0, &b);
        let mut encoder = device.create_command_encoder(&Default::default());
        gpu.encode_publish_output(&mut encoder, 0);
        queue.submit(std::iter::once(encoder.finish()));
        assert_eq!(read_texture(&device, &queue, &gpu.outputs[0].texture), b);

        for (filter_mode, scale) in [(0_u32, 1_u32), (1, 1), (1, 2), (2, 2), (2, 3)] {
            let (tw, th) = (W * scale, H * scale);
            let target = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: tw,
                    height: th,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FG_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = target.create_view(&Default::default());
            queue.write_buffer(
                &gpu.blit_uniforms,
                0,
                bytemuck::bytes_of(&BlitUniforms {
                    filter_mode,
                    _pad: 0,
                    viewport_size: [tw as f32, th as f32],
                }),
            );
            let mut encoder = device.create_command_encoder(&Default::default());
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    multiview_mask: None,
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        depth_slice: None,
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                pass.set_pipeline(&gpu.blit_pipeline);
                pass.set_bind_group(0, &gpu.outputs[0].blit_bind_group, &[]);
                pass.draw(0..6, 0..1);
            }
            queue.submit(std::iter::once(encoder.finish()));
            let out = read_texture(&device, &queue, &target);
            // Sample pixels at texel centres of the source.
            for (x, y) in [(3_u32, 3_u32), (100, 40), (200, 100)] {
                let si = ((y * W + x) * 4) as usize;
                let (ox, oy) = (x * scale + scale / 2, y * scale + scale / 2);
                let oi = ((oy * tw + ox) * 4) as usize;
                for c in 1..3 {
                    // Smooth gradient channels survive every filter closely.
                    let diff = (out[oi + c] as i32 - b[si + c] as i32).abs();
                    assert!(
                        diff <= 3,
                        "filter {filter_mode} x{scale} at ({x},{y}) channel {c}: {} vs {}",
                        out[oi + c],
                        b[si + c]
                    );
                }
            }
            if scale == 1 {
                assert_eq!(
                    out, b,
                    "1:1 presentation must be exact (filter {filter_mode})"
                );
            }
        }
    }
}

/// Generated frame from RIFE flow + mask (see `FlowWarp`): the half-res
/// field is upsampled bilinearly (which also removes the network mask's
/// 2-px checkerboard), both real frames are sampled bicubically (a = -0.75,
/// as PyTorch's grid_sample) along their flows and blended by the mask.
const WARP_SHADER: &str = r#"
struct Params {
    width: u32, height: u32, half_w: u32, half_h: u32,
    contrast: f32, row_words: u32, _pad0: u32, _pad1: u32,
};
@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> field: array<vec4<u32>>;
@group(0) @binding(2) var<storage, read> img0: array<u32>;
@group(0) @binding(3) var<storage, read> img1: array<u32>;
@group(0) @binding(4) var<storage, read_write> dst: array<u32>;

struct Fm { f0: vec2<f32>, f1: vec2<f32>, m: f32 };

fn load_fm(q: vec2<i32>) -> Fm {
    let c = clamp(q, vec2<i32>(0), vec2<i32>(i32(P.half_w) - 1, i32(P.half_h) - 1));
    let v = field[u32(c.y) * P.half_w + u32(c.x)];
    return Fm(unpack2x16float(v.x), unpack2x16float(v.y), unpack2x16float(v.z).x);
}

fn mix_fm(a: Fm, b: Fm, t: f32) -> Fm {
    return Fm(mix(a.f0, b.f0, t), mix(a.f1, b.f1, t), mix(a.m, b.m, t));
}

fn px0(x: i32, y: i32) -> vec3<f32> {
    let cx = clamp(x, 0, i32(P.width) - 1);
    let cy = clamp(y, 0, i32(P.height) - 1);
    return unpack4x8unorm(img0[u32(cy) * P.row_words + u32(cx)]).rgb;
}

fn px1(x: i32, y: i32) -> vec3<f32> {
    let cx = clamp(x, 0, i32(P.width) - 1);
    let cy = clamp(y, 0, i32(P.height) - 1);
    return unpack4x8unorm(img1[u32(cy) * P.row_words + u32(cx)]).rgb;
}

fn cubic(x: f32) -> f32 {
    let a = -0.75;
    let t = abs(x);
    if t <= 1.0 {
        return ((a + 2.0) * t - (a + 3.0)) * t * t + 1.0;
    }
    if t < 2.0 {
        return ((a * t - 5.0 * a) * t + 8.0 * a) * t - 4.0 * a;
    }
    return 0.0;
}

fn weights(t: f32) -> vec4<f32> {
    return vec4<f32>(cubic(1.0 + t), cubic(t), cubic(1.0 - t), cubic(2.0 - t));
}

fn sample(which: u32, s: vec2<f32>) -> vec3<f32> {
    let c = clamp(s, vec2<f32>(0.0), vec2<f32>(f32(P.width) - 1.0, f32(P.height) - 1.0));
    let b = floor(c);
    let wx = weights(c.x - b.x);
    let wy = weights(c.y - b.y);
    let ix = i32(b.x);
    let iy = i32(b.y);
    var acc = vec3<f32>(0.0);
    for (var j = 0; j < 4; j++) {
        var row = vec3<f32>(0.0);
        for (var i = 0; i < 4; i++) {
            var p: vec3<f32>;
            if which == 0u {
                p = px0(ix + i - 1, iy + j - 1);
            } else {
                p = px1(ix + i - 1, iy + j - 1);
            }
            row += p * wx[i];
        }
        acc += row * wy[j];
    }
    return acc;
}

@compute @workgroup_size(8, 8, 1)
fn warp(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= P.width || gid.y >= P.height {
        return;
    }
    let p = vec2<f32>(f32(gid.x), f32(gid.y));
    let q = (p + 0.5) * 0.5 - 0.5;
    let q0 = floor(q);
    let t = q - q0;
    let qi = vec2<i32>(q0);
    let top = mix_fm(load_fm(qi), load_fm(qi + vec2<i32>(1, 0)), t.x);
    let bottom = mix_fm(load_fm(qi + vec2<i32>(0, 1)), load_fm(qi + vec2<i32>(1, 1)), t.x);
    let f = mix_fm(top, bottom, t.y);
    let m = clamp((f.m - 0.5) * P.contrast + 0.5, 0.0, 1.0);
    let a = sample(0u, p + f.f0);
    let b = sample(1u, p + f.f1);
    let rgb = clamp(a * m + b * (1.0 - m), vec3<f32>(0.0), vec3<f32>(1.0));
    dst[gid.y * P.row_words + gid.x] = pack4x8unorm(vec4<f32>(rgb, 1.0));
}
"#;

/// Capture-side passes (run when a frame is handed to FRUC).
///
/// `profile`: mean luma of every column and row of the frame and of its
/// reference, for the CPU motion estimate.
///
/// `persist` / `core` / `fill`: static HUD removal. A pixel is HUD when it
/// has been unchanged for ~8 source frames while the area around it (and a
/// good part of the screen) moves: a static island in moving scenery. Those
/// pixels, widened by 3 px for anti-aliasing, are painted over with nearby
/// scenery colours in the image FRUC receives, so optical flow never sees
/// (and never drags) the HUD. The real HUD is restored after interpolation.
const PREP_SHADER: &str = r#"
struct Params {
    width: u32, height: u32, tiles_x: u32, tiles_y: u32,
    hud: u32, total_tiles: u32, slot_offset: u32, mv_valid: u32,
    static_thr: f32, global_motion: f32, local_motion: f32, salience: f32,
    mv_x: i32, mv_y: i32, guard_window: u32, _pad1: u32,
};
struct Global { moving_tiles: atomic<u32>, _a: u32, _b: u32, _c: u32 };

@group(0) @binding(0) var cur_tex: texture_2d<f32>;
@group(0) @binding(1) var ref_tex: texture_2d<f32>;
@group(0) @binding(2) var out_tex: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(3) var<storage, read_write> persistence: array<f32>;
@group(0) @binding(4) var<storage, read_write> tile_moving: array<u32>;
@group(0) @binding(5) var<storage, read_write> global_state: Global;
@group(0) @binding(6) var<storage, read_write> core_mask: array<u32>;
@group(0) @binding(7) var<storage, read_write> profiles: array<f32>;
@group(0) @binding(8) var<uniform> p: Params;
@group(0) @binding(9) var<storage, read_write> hud_bits: array<atomic<u32>>;
@group(0) @binding(10) var<storage, read_write> confidence: array<f32>;

fn clamp_pos(pos: vec2<i32>) -> vec2<i32> {
    return clamp(pos, vec2<i32>(0, 0), vec2<i32>(i32(p.width) - 1, i32(p.height) - 1));
}

fn luma(t: texture_2d<f32>, pos: vec2<i32>) -> f32 {
    return dot(textureLoad(t, clamp_pos(pos), 0).rgb, vec3<f32>(0.299, 0.587, 0.114)) * 255.0;
}

// Layout: [cur cols (W)] [cur rows (H)] [ref cols (W)] [ref rows (H)].
@compute @workgroup_size(64, 1, 1)
fn profile(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let w = p.width;
    let h = p.height;
    if i >= w + h {
        return;
    }
    var sum_c = 0.0;
    var sum_r = 0.0;
    var count = 0.0;
    if i < w {
        for (var y = 0u; y < h; y = y + 2u) {
            sum_c = sum_c + luma(cur_tex, vec2<i32>(i32(i), i32(y)));
            sum_r = sum_r + luma(ref_tex, vec2<i32>(i32(i), i32(y)));
            count = count + 1.0;
        }
    } else {
        let y = i - w;
        for (var x = 0u; x < w; x = x + 2u) {
            sum_c = sum_c + luma(cur_tex, vec2<i32>(i32(x), i32(y)));
            sum_r = sum_r + luma(ref_tex, vec2<i32>(i32(x), i32(y)));
            count = count + 1.0;
        }
    }
    profiles[i] = sum_c / count;
    profiles[w + h + i] = sum_r / count;
}

// Pixel luma minus the mean of 8 samples on a radius-8 ring.
fn local_contrast(t: texture_2d<f32>, pos: vec2<i32>) -> f32 {
    var mean = 0.0;
    for (var k = 0; k < 8; k = k + 1) {
        let a = f32(k) * 0.785398;
        let off = vec2<i32>(vec2<f32>(cos(a), sin(a)) * 8.0);
        mean = mean + luma(t, pos + off);
    }
    return luma(t, pos) - mean / 8.0;
}

var<workgroup> wg_moving: atomic<u32>;

@compute @workgroup_size(8, 8, 1)
fn persist(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
    @builtin(workgroup_id) wid: vec3<u32>,
) {
    if li == 0u {
        atomicStore(&wg_moving, 0u);
    }
    workgroupBarrier();
    if gid.x < p.width && gid.y < p.height {
        let pos = vec2<i32>(gid.xy);
        var dmax = 0.0;
        for (var dy = -1; dy <= 1; dy = dy + 1) {
            for (var dx = -1; dx <= 1; dx = dx + 1) {
                let q = pos + vec2<i32>(dx, dy);
                dmax = max(dmax, abs(luma(cur_tex, q) - luma(ref_tex, q)));
            }
        }
        let idx = gid.y * p.width + gid.x;
        let old = persistence[idx];
        if dmax < p.static_thr {
            persistence[idx] = min(old + 0.125, 1.0);
        } else {
            persistence[idx] = max(old - 0.5, 0.0);
            atomicAdd(&wg_moving, 1u);
        }
    }
    workgroupBarrier();
    if li == 0u {
        let moving = atomicLoad(&wg_moving);
        tile_moving[wid.y * p.tiles_x + wid.x] = select(0u, 1u, moving >= 16u);
        if moving >= 16u {
            atomicAdd(&global_state.moving_tiles, 1u);
        }
    }
}

// HUD guard search windows: the crosshair / aim reticle at the screen
// centre, and the minimap corner (its player marker; the minimap is larger
// when expanded). Detection inside them is still per frame.
fn in_guard_window(pos: vec2<i32>) -> bool {
    let w = i32(p.guard_window);
    if w == 0 {
        return true;
    }
    let centre = vec2<i32>(i32(p.width / 2u), i32(p.height / 2u));
    let off = abs(pos - centre);
    if off.x <= w && off.y <= w {
        return true;
    }
    return f32(pos.x) < 0.17 * f32(p.width) && f32(pos.y) > 0.70 * f32(p.height);
}

@compute @workgroup_size(8, 8, 1)
fn core(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= p.width || gid.y >= p.height {
        return;
    }
    let idx = gid.y * p.width + gid.x;
    var is_core = 0u;
    let motion = f32(atomicLoad(&global_state.moving_tiles)) / f32(max(p.total_tiles, 1u));
    let guard = (p.hud & 2u) != 0u;
    if motion >= p.global_motion && (guard || persistence[idx] >= 0.99) {
        // Static island: most tiles within 2 tiles (16 px) are moving.
        let tx = i32(gid.x / 8u);
        let ty = i32(gid.y / 8u);
        var moving = 0.0;
        var total = 0.0;
        for (var dy = -2; dy <= 2; dy = dy + 1) {
            for (var dx = -2; dx <= 2; dx = dx + 1) {
                let q = vec2<i32>(tx + dx, ty + dy);
                if q.x < 0 || q.y < 0 || q.x >= i32(p.tiles_x) || q.y >= i32(p.tiles_y) {
                    continue;
                }
                moving = moving + f32(tile_moving[u32(q.y) * p.tiles_x + u32(q.x)]);
                total = total + 1.0;
            }
        }
        let island = moving >= p.local_motion * total;
        if !guard {
            is_core = select(0u, 1u, island);
        } else if island {
            // HUD guard: only elements that stand out from what is around
            // them (crosshair, icons, text), not flat scenery.
            let pos = vec2<i32>(gid.xy);
            let hc = local_contrast(cur_tex, pos);
            // Only the crosshair and minimap windows: this keeps the guard
            // off the player character, who also stays in place on screen
            // while the camera orbits.
            if in_guard_window(pos) && abs(hc) >= p.salience {
                // Semi-transparent HUD (RDR2's crosshair ring) takes its
                // colour from the scenery behind it, so its pixels change
                // while the camera moves; its shape does not. A pixel that
                // keeps its local contrast while everything around it moves
                // builds confidence; one that loses it, loses more. Scenery
                // matches by chance far less often, so only real HUD gets
                // there, and the memory carries over to the next pan.
                let hr = local_contrast(ref_tex, pos);
                let tol = max(p.static_thr, 0.35 * max(abs(hc), abs(hr)));
                let kept = abs(hc - hr) < tol;
                let c = confidence[idx];
                confidence[idx] = select(max(c - 0.25, 0.0), min(c + 0.125, 1.0), kept);
                let colour_static = persistence[idx] >= 0.99;
                // Crosshair shapes change (dot, ring, aim reticle) and each
                // turn starts unprotected until the shape is learnt; two
                // matching frames in a row are enough inside the window.
                if colour_static || (kept && c >= 0.25) {
                    is_core = 1u;
                }
            }
        }
    }
    // HUD guard: a learnt element keeps being guarded while it keeps its
    // shape, also when the camera slows down (slow aiming); otherwise the
    // guard switches on and off from frame to frame, which itself flickers.
    if guard && is_core == 0u && confidence[idx] >= 0.5 {
        let pos = vec2<i32>(gid.xy);
        if in_guard_window(pos) {
            let hc = local_contrast(cur_tex, pos);
            let hr = local_contrast(ref_tex, pos);
            let tol = max(p.static_thr, 0.35 * max(abs(hc), abs(hr)));
            if abs(hc) >= p.salience && abs(hc - hr) < tol {
                is_core = 1u;
            }
        }
    }
    core_mask[idx] = is_core;
}

fn core_at(pos: vec2<i32>) -> bool {
    if pos.x < 0 || pos.y < 0 || pos.x >= i32(p.width) || pos.y >= i32(p.height) {
        return false;
    }
    return (core_mask[u32(pos.y) * p.width + u32(pos.x)] & 1u) != 0u;
}

// HUD guard: the pixels the guard acts on (isolated HUD, see `isolate`).
fn guarded_at(pos: vec2<i32>) -> bool {
    if pos.x < 0 || pos.y < 0 || pos.x >= i32(p.width) || pos.y >= i32(p.height) {
        return false;
    }
    return (core_mask[u32(pos.y) * p.width + u32(pos.x)] & 2u) != 0u;
}

// HUD found in this frame, or holding its exact colour during the turn.
fn hud_now(pos: vec2<i32>) -> bool {
    if pos.x < 0 || pos.y < 0 || pos.x >= i32(p.width) || pos.y >= i32(p.height) {
        return false;
    }
    let i = u32(pos.y) * p.width + u32(pos.x);
    return (core_mask[i] & 1u) != 0u || persistence[i] >= 0.99;
}

fn hud_like(pos: vec2<i32>) -> bool {
    if pos.x < 0 || pos.y < 0 || pos.x >= i32(p.width) || pos.y >= i32(p.height) {
        return false;
    }
    let i = u32(pos.y) * p.width + u32(pos.x);
    // Core HUD, HUD seen recently, or anything holding its exact colour
    // while the camera turns (flat menu / minimap panels).
    return (core_mask[i] & 1u) != 0u || confidence[i] >= 0.375 || persistence[i] >= 0.99;
}

/// HUD guard: keep only small, isolated HUD elements (crosshair, markers):
/// nothing HUD-like on a 24 px circle around the pixel. The interpolator
/// loses those during camera turns; larger HUD (minimap, its icons, prompt
/// text, menus) and the player character, who stays in place on screen
/// while the camera orbits, come through the interpolator intact, and
/// repainting them was what made them flicker.
@compute @workgroup_size(8, 8, 1)
fn isolate(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= p.width || gid.y >= p.height {
        return;
    }
    let idx = gid.y * p.width + gid.x;
    if core_mask[idx] != 1u {
        return;
    }
    if isolated(vec2<i32>(gid.xy)) {
        // Bit 0 (core) stays set; other invocations only read bit 0.
        core_mask[idx] = 3u;
    }
}

fn isolated(pos: vec2<i32>) -> bool {
    // A HUD element, not a stray pixel of scenery that matched by chance.
    var cluster = 0;
    for (var dy = -2; dy <= 2; dy = dy + 1) {
        for (var dx = -2; dx <= 2; dx = dx + 1) {
            if core_at(pos + vec2<i32>(dx, dy)) {
                cluster = cluster + 1;
            }
        }
    }
    if cluster < 8 {
        return false;
    }
    // 16 samples on the circle, each a 3x3 patch so thin HUD lines (the
    // minimap's rim) cannot slip between them.
    for (var k = 0; k < 16; k = k + 1) {
        let a = f32(k) * 0.392699;
        let c = pos + vec2<i32>(round(vec2<f32>(cos(a), sin(a)) * 24.0));
        if hud_like(c) {
            return false;
        }
        for (var dy = -1; dy <= 1; dy = dy + 1) {
            for (var dx = -1; dx <= 1; dx = dx + 1) {
                if hud_now(c + vec2<i32>(dx, dy)) {
                    return false;
                }
            }
        }
    }
    return true;
}

@compute @workgroup_size(8, 8, 1)
fn fill(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= p.width || gid.y >= p.height {
        return;
    }
    let pos = vec2<i32>(gid.xy);
    let original = textureLoad(cur_tex, pos, 0);
    var masked = false;
    var near = false;
    let guard = (p.hud & 2u) != 0u;
    for (var dy = -3; dy <= 3; dy = dy + 1) {
        for (var dx = -3; dx <= 3; dx = dx + 1) {
            let q = pos + vec2<i32>(dx, dy);
            if select(core_at(q), guarded_at(q), guard) {
                masked = true;
                if abs(dx) <= 2 && abs(dy) <= 2 {
                    near = true;
                }
            }
        }
    }
    if !masked {
        textureStore(out_tex, pos, original);
        return;
    }
    if (p.hud & 2u) != 0u {
        // Restore mask: the HUD element plus 2 px of anti-aliased rim. A
        // wider ring would paste the next frame's background around the
        // element, a visible halo over trees (invisible over sky). The rest
        // of the painted area is scenery the interpolator rebuilt.
        if near {
            let i = gid.y * p.width + gid.x;
            atomicOr(&hud_bits[p.slot_offset + i / 32u], 1u << (i % 32u));
        }
        // Fill with what the previous frame showed at the spot the camera
        // motion brings here, so the interpolator sees consistent scenery
        // in both frames and has nothing to drag.
        if p.mv_valid != 0u {
            let q = pos - vec2<i32>(p.mv_x, p.mv_y);
            if q.x >= 0 && q.y >= 0 && q.x < i32(p.width) && q.y < i32(p.height) {
                var clear = true;
                for (var dy = -3; dy <= 3; dy = dy + 3) {
                    for (var dx = -3; dx <= 3; dx = dx + 3) {
                        if core_at(q + vec2<i32>(dx, dy)) {
                            clear = false;
                        }
                    }
                }
                if clear {
                    textureStore(out_tex, pos, textureLoad(ref_tex, q, 0));
                    return;
                }
            }
        }
    }
    // Walk 8 directions to the first non-HUD pixel, step past the
    // anti-aliased rim and average what is found there.
    var dirs = array<vec2<i32>, 8>(
        vec2<i32>(1, 0), vec2<i32>(-1, 0), vec2<i32>(0, 1), vec2<i32>(0, -1),
        vec2<i32>(1, 1), vec2<i32>(-1, -1), vec2<i32>(1, -1), vec2<i32>(-1, 1),
    );
    var sum = vec3<f32>(0.0);
    var weight = 0.0;
    for (var d = 0; d < 8; d = d + 1) {
        for (var step = 1; step <= 16; step = step + 1) {
            let q = pos + dirs[d] * (step * 3);
            if q.x < 0 || q.y < 0 || q.x >= i32(p.width) || q.y >= i32(p.height) {
                break;
            }
            if !core_at(q) {
                let s = clamp_pos(q + dirs[d] * 4);
                let wgt = 1.0 / f32(step);
                sum = sum + textureLoad(cur_tex, s, 0).rgb * wgt;
                weight = weight + wgt;
                break;
            }
        }
    }
    if weight > 0.0 {
        textureStore(out_tex, pos, vec4<f32>(sum / weight, 1.0));
    } else {
        textureStore(out_tex, pos, original);
    }
}
"#;
