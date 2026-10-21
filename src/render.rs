#![allow(clippy::items_after_test_module)]

use crate::color::ColorInfo;

use crate::capture::{CaptureFrame, PixelFormat};
use crate::settings::{ColorRange, ColorSpace, ScaleFilter};
use crate::ui::PreparedUi;
use bytemuck::{Pod, Zeroable};
use egui_wgpu::Renderer as EguiRenderer;
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use tracing::info;
use wgpu::{CompositeAlphaMode, PresentMode, SurfaceConfiguration, TextureUsages};
use winit::dpi::PhysicalSize;
use winit::window::Window;

const CLEAR_COLOR: wgpu::Color = wgpu::Color {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};

#[cfg(test)]
#[path = "render_gpu_tests.rs"]
mod gpu_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveScaleFilter {
    Bilinear,
    Bicubic,
    Lanczos,
    Fsr1,
    Fsr1FallbackBilinear,
    Fsr1FallbackDirect,
}

impl Display for ActiveScaleFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bilinear => f.write_str("Bilinear"),
            Self::Bicubic => f.write_str("Bicubic"),
            Self::Lanczos => f.write_str("Lanczos"),
            Self::Fsr1 => f.write_str("FSR 1.0"),
            Self::Fsr1FallbackBilinear => f.write_str("Bilinear (FSR fallback: <1x)"),
            Self::Fsr1FallbackDirect => f.write_str("Direct 1:1 (FSR fallback)"),
        }
    }
}

pub struct Renderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: SurfaceConfiguration,
    size: PhysicalSize<u32>,
    surface_format: wgpu::TextureFormat,
    video_pipeline: wgpu::RenderPipeline,
    video_bind_group_layout: wgpu::BindGroupLayout,
    video_samplers: VideoSamplers,
    uniforms: wgpu::Buffer,
    scale_filter: ScaleFilter,
    sharpness: f32,
    active_scale_filter: ActiveScaleFilter,
    source_color: ColorInfo,
    color_space: ColorSpace,
    color_range: ColorRange,
    brightness: f32,
    contrast: f32,
    saturation: f32,
    gamma: f32,
    video_frame: Option<VideoFrameResources>,
    egui_renderer: EguiRenderer,
    upscale_pipelines: Option<UpscalePipelines>,
    upscale_textures: Option<UpscaleTextures>,
    // Reusable scratch buffers to avoid per-frame allocations
    pad_scratch: Vec<u8>,
    nv12_u_scratch: Vec<u8>,
    nv12_v_scratch: Vec<u8>,
    // Shared DX12 buffers for zero-copy GPU decode (None = not available)
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    shared_gpu_buffers: Option<crate::dx12_interop::SharedGpuBuffers>,
}

impl Renderer {
    /// `scale_filter`, `color_space`, and `color_range` are from saved settings.
    /// Taken as constructor arguments rather than defaulted, so a renderer can't
    /// come up disagreeing with the settings the menu is showing.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        window: Arc<Window>,
        scale_filter: ScaleFilter,
        sharpness: f32,
        color_space: ColorSpace,
        color_range: ColorRange,
        brightness: f32,
        contrast: f32,
        saturation: f32,
        gamma: f32,
    ) -> Result<Self, RenderError> {
        let size = window.inner_size();
        // Prefer DX12 on Windows so that CUDA ↔ DX12 zero-copy interop works.
        // Fall back to all backends if DX12 isn't available.
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: if cfg!(windows) {
                wgpu::Backends::DX12
            } else {
                wgpu::Backends::all()
            },
            ..Default::default()
        });
        let surface = instance
            .create_surface(window.clone())
            .map_err(RenderError::CreateSurface)?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or(RenderError::AdapterUnavailable)?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("tacklecast-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .map_err(RenderError::RequestDevice)?;

        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|format| *format == wgpu::TextureFormat::Bgra8Unorm)
            .or_else(|| caps.formats.first().copied())
            .ok_or(RenderError::SurfaceFormatUnavailable)?;

        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: caps
                .present_modes
                .iter()
                .copied()
                .find(|mode| *mode == PresentMode::Mailbox)
                .or_else(|| {
                    caps.present_modes
                        .iter()
                        .copied()
                        .find(|mode| *mode == PresentMode::Immediate)
                })
                .unwrap_or(PresentMode::AutoVsync),
            alpha_mode: caps
                .alpha_modes
                .iter()
                .copied()
                .find(|mode| *mode == CompositeAlphaMode::Opaque)
                .unwrap_or(CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };

        info!("present mode: {:?}", config.present_mode);
        surface.configure(&device, &config);

        let video_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tacklecast-video-bind-group-layout"),
                entries: &[
                    texture_layout_entry(0),
                    texture_layout_entry(1),
                    texture_layout_entry(2),
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 4,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 5,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tacklecast-video-uniforms"),
            size: std::mem::size_of::<VideoUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tacklecast-video-shader"),
            source: wgpu::ShaderSource::Wgsl(VIDEO_SHADER.into()),
        });

        let video_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tacklecast-video-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let nearest_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tacklecast-nearest-sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tacklecast-video-pipeline-layout"),
            bind_group_layouts: &[&video_bind_group_layout],
            push_constant_ranges: &[],
        });

        let video_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tacklecast-video-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let egui_renderer = EguiRenderer::new(&device, surface_format, None, 1, false);

        let initial_active = match scale_filter {
            ScaleFilter::Bilinear => ActiveScaleFilter::Bilinear,
            ScaleFilter::Bicubic => ActiveScaleFilter::Bicubic,
            ScaleFilter::Lanczos => ActiveScaleFilter::Lanczos,
            ScaleFilter::Fsr1 => ActiveScaleFilter::Fsr1,
        };

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            size,
            surface_format,
            video_pipeline,
            video_bind_group_layout,
            video_samplers: VideoSamplers {
                filtering: video_sampler,
                nearest: nearest_sampler,
            },
            uniforms,
            scale_filter,
            sharpness: sharpness.clamp(0.0, 2.0),
            active_scale_filter: initial_active,
            source_color: ColorInfo::default(),
            color_space,
            color_range,
            brightness,
            contrast,
            saturation,
            gamma,
            video_frame: None,
            egui_renderer,
            upscale_pipelines: None,
            upscale_textures: None,
            pad_scratch: Vec::new(),
            nv12_u_scratch: Vec::new(),
            nv12_v_scratch: Vec::new(),
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            shared_gpu_buffers: None,
        })
    }

    pub fn max_texture_side(&self) -> usize {
        self.device.limits().max_texture_dimension_2d as usize
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            self.size = size;
            return;
        }

        self.size = size;
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn set_scale_filter(&mut self, filter: ScaleFilter) {
        self.scale_filter = filter;
        if filter != ScaleFilter::Fsr1 {
            self.upscale_textures = None;
        }
    }

    pub fn set_sharpness(&mut self, sharpness: f32) {
        self.sharpness = sharpness.clamp(0.0, 2.0);
    }

    #[allow(dead_code)]
    pub fn active_scale_filter(&self) -> ActiveScaleFilter {
        self.active_scale_filter
    }

    pub fn active_scale_filter_label(&self) -> &'static str {
        match self.active_scale_filter {
            ActiveScaleFilter::Bilinear => "Bilinear",
            ActiveScaleFilter::Bicubic => "Bicubic",
            ActiveScaleFilter::Lanczos => "Lanczos",
            ActiveScaleFilter::Fsr1 => "FSR 1.0",
            ActiveScaleFilter::Fsr1FallbackBilinear => "Bilinear (FSR fallback: <1x)",
            ActiveScaleFilter::Fsr1FallbackDirect => "Direct 1:1 (FSR fallback)",
        }
    }

    pub fn set_picture_settings(
        &mut self,
        brightness: f32,
        contrast: f32,
        saturation: f32,
        gamma: f32,
    ) {
        self.brightness = brightness;
        self.contrast = contrast;
        self.saturation = saturation;
        self.gamma = gamma;
    }

    pub fn set_color_settings(&mut self, color_space: ColorSpace, color_range: ColorRange) {
        self.color_space = color_space;
        self.color_range = color_range;
    }

    pub fn scale_filter(&self) -> ScaleFilter {
        self.scale_filter
    }

    pub fn color_info(&self) -> ColorInfo {
        self.source_color
    }

    pub fn upload_frame(&mut self, frame: &CaptureFrame) {
        self.source_color = frame.color_info();
        match frame {
            CaptureFrame::Cpu {
                width,
                height,
                format,
                y_data,
                u_data,
                v_data,
                ..
            } => self.upload_cpu_frame(*width, *height, *format, y_data, u_data, v_data),
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            CaptureFrame::Gpu {
                width,
                height,
                buffer_index,
                ..
            } => self.upload_gpu_frame(*width, *height, *buffer_index),
        }
    }

    fn upload_cpu_frame(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
        y_data: &[u8],
        u_data: &[u8],
        v_data: &[u8],
    ) {
        let needs_rebuild = self
            .video_frame
            .as_ref()
            .map(|video_frame| {
                video_frame.width != width
                    || video_frame.height != height
                    || video_frame.format != format
            })
            .unwrap_or(true);

        if needs_rebuild {
            self.video_frame = Some(VideoFrameResources::new(
                &self.device,
                &self.video_bind_group_layout,
                &self.video_samplers,
                &self.uniforms,
                width,
                height,
                format,
            ));
        }

        let Some(video_frame) = &self.video_frame else {
            return;
        };

        upload_plane_r8(
            &self.queue,
            &video_frame.y_texture,
            width,
            height,
            y_data,
            &mut self.pad_scratch,
        );

        match format {
            PixelFormat::Nv12 => {
                let (u_plane, v_plane) = deinterleave_nv12_into(
                    width,
                    height,
                    u_data,
                    &mut self.nv12_u_scratch,
                    &mut self.nv12_v_scratch,
                );
                upload_plane_r8(
                    &self.queue,
                    &video_frame.u_texture,
                    width / 2,
                    height / 2,
                    u_plane,
                    &mut self.pad_scratch,
                );
                upload_plane_r8(
                    &self.queue,
                    &video_frame.v_texture,
                    width / 2,
                    height / 2,
                    v_plane,
                    &mut self.pad_scratch,
                );
            }
            PixelFormat::Yuvj422p => {
                upload_plane_r8(
                    &self.queue,
                    &video_frame.u_texture,
                    width / 2,
                    height,
                    u_data,
                    &mut self.pad_scratch,
                );
                upload_plane_r8(
                    &self.queue,
                    &video_frame.v_texture,
                    width / 2,
                    height,
                    v_data,
                    &mut self.pad_scratch,
                );
            }
        }
    }

    /// Try to initialize shared DX12 ↔ CUDA buffers for zero-copy.
    /// Returns import handles for the CUDA side if successful.
    /// The handles are ephemeral — CUDA imports them and they're closed on drop.
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    pub fn try_init_shared_buffers(
        &mut self,
        width: u32,
        height: u32,
    ) -> Option<crate::dx12_interop::ImportHandles> {
        let (shared, import_handles) =
            crate::dx12_interop::SharedGpuBuffers::try_new(&self.device, width, height)?;
        self.shared_gpu_buffers = Some(shared);
        Some(import_handles)
    }

    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
    fn upload_gpu_frame(&mut self, width: u32, height: u32, buffer_index: usize) {
        let Some(shared) = &self.shared_gpu_buffers else {
            tracing::warn!("GPU frame received but no shared buffers initialized");
            return;
        };

        if buffer_index >= shared.0.len() {
            tracing::warn!("GPU frame buffer_index {buffer_index} out of range");
            return;
        }

        let format = PixelFormat::Yuvj422p; // nvJPEG always outputs YUV 4:2:2

        // Rebuild textures if dimensions changed
        let needs_rebuild = self
            .video_frame
            .as_ref()
            .map(|vf| vf.width != width || vf.height != height || vf.format != format)
            .unwrap_or(true);

        if needs_rebuild {
            self.video_frame = Some(VideoFrameResources::new(
                &self.device,
                &self.video_bind_group_layout,
                &self.video_samplers,
                &self.uniforms,
                width,
                height,
                format,
            ));
        }

        let Some(video_frame) = &self.video_frame else {
            return;
        };

        let buf_set = &shared.0[buffer_index];
        let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

        // GPU-side copy: shared buffer → Y texture
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tacklecast-gpu-copy-encoder"),
            });

        let y_bytes_per_row = align_up(width, alignment);
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &buf_set.y_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(y_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture: &video_frame.y_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        // GPU-side copy: shared buffer → U texture (half-width, full-height for 4:2:2)
        let chroma_width = width / 2;
        let uv_bytes_per_row = align_up(chroma_width, alignment);
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &buf_set.u_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(uv_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture: &video_frame.u_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: chroma_width,
                height,
                depth_or_array_layers: 1,
            },
        );

        // GPU-side copy: shared buffer → V texture
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &buf_set.v_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(uv_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture: &video_frame.v_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: chroma_width,
                height,
                depth_or_array_layers: 1,
            },
        );

        self.queue.submit(std::iter::once(encoder.finish()));
    }

    pub fn render(&mut self, ui: Option<PreparedUi>) -> Result<(), RenderError> {
        if self.size.width == 0 || self.size.height == 0 {
            return Ok(());
        }

        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                // Flush any staged write_texture/write_buffer calls so they
                // don't accumulate when we can't present.
                self.queue.submit(std::iter::empty());
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => {
                self.queue.submit(std::iter::empty());
                return Ok(());
            }
            Err(wgpu::SurfaceError::OutOfMemory) => return Err(RenderError::OutOfMemory),
            Err(error) => return Err(RenderError::Surface(error)),
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tacklecast-clear-encoder"),
            });

        let mut ui_texture_free = Vec::new();
        let mut ui_user_command_buffers = Vec::new();

        if let Some(ui) = ui.as_ref() {
            for (texture_id, image_delta) in &ui.textures_delta.set {
                self.egui_renderer.update_texture(
                    &self.device,
                    &self.queue,
                    *texture_id,
                    image_delta,
                );
            }

            ui_user_command_buffers = self.egui_renderer.update_buffers(
                &self.device,
                &self.queue,
                &mut encoder,
                &ui.paint_jobs,
                &ui.screen_descriptor,
            );

            ui_texture_free.extend(ui.textures_delta.free.iter().copied());
        }

        // Where the letterboxed video lands on screen. The shader needs this to
        // work out the upscale ratio, so write it (and the filter selection, color space,
        // and color range) before opening the pass.
        let video_viewport = self.video_frame.as_ref().map(|video_frame| {
            calculate_video_viewport(
                self.size.width as f32,
                self.size.height as f32,
                video_frame.width as f32,
                video_frame.height as f32,
            )
        });

        if let (Some(video_frame), Some(viewport)) = (&self.video_frame, video_viewport) {
            let is_upscaling = viewport.width > video_frame.width as f32
                || viewport.height > video_frame.height as f32;
            let is_equal = viewport.width == video_frame.width as f32
                && viewport.height == video_frame.height as f32;

            let resolved_space = self
                .source_color
                .resolve_space(self.color_space, video_frame.height);
            let resolved_range = self.source_color.resolve_range(self.color_range);

            if self.scale_filter == ScaleFilter::Fsr1 && is_upscaling {
                self.active_scale_filter = ActiveScaleFilter::Fsr1;

                let out_w = viewport.width.round().max(1.0) as u32;
                let out_h = viewport.height.round().max(1.0) as u32;

                Self::ensure_upscale_resources(
                    &self.device,
                    &self.video_bind_group_layout,
                    self.surface_format,
                    &mut self.upscale_pipelines,
                    &mut self.upscale_textures,
                    video_frame.width,
                    video_frame.height,
                    out_w,
                    out_h,
                );
                let pipelines = self.upscale_pipelines.as_ref().unwrap();
                let textures = self.upscale_textures.as_ref().unwrap();

                // Pass 1: YUV -> RGB native resolution texture with picture adjustments
                let yuv_uniforms = VideoUniforms {
                    format_mode: VideoUniforms::format_mode_for(video_frame.format),
                    filter_mode: 0,
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
                    .write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&yuv_uniforms));

                let easu_con = calculate_easu_constants(
                    video_frame.width as f32,
                    video_frame.height as f32,
                    out_w as f32,
                    out_h as f32,
                );
                self.queue
                    .write_buffer(&pipelines.easu_uniforms, 0, bytemuck::bytes_of(&easu_con));

                let rcas_con = RcasConstants::new(self.sharpness, out_w as f32, out_h as f32);
                self.queue
                    .write_buffer(&pipelines.rcas_uniforms, 0, bytemuck::bytes_of(&rcas_con));

                // Pass 1: YUV -> RGB native resolution texture
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("tacklecast-fsr-yuv-to-rgb-pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &textures.native_rgb_view,
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
                    pass.set_pipeline(&pipelines.yuv_to_rgb_pipeline);
                    pass.set_bind_group(0, &video_frame.bind_group, &[]);
                    pass.set_viewport(
                        0.0,
                        0.0,
                        video_frame.width as f32,
                        video_frame.height as f32,
                        0.0,
                        1.0,
                    );
                    pass.draw(0..6, 0..1);
                }

                // Pass 2: EASU upscale pass
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("tacklecast-fsr-easu-pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
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

                // Pass 3: RCAS sharpening pass onto surface view with letterbox clear
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("tacklecast-fsr-rcas-surface-pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
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
            } else {
                self.active_scale_filter = match self.scale_filter {
                    ScaleFilter::Bilinear => ActiveScaleFilter::Bilinear,
                    ScaleFilter::Bicubic => ActiveScaleFilter::Bicubic,
                    ScaleFilter::Lanczos => ActiveScaleFilter::Lanczos,
                    ScaleFilter::Fsr1 => {
                        if is_equal {
                            ActiveScaleFilter::Fsr1FallbackDirect
                        } else {
                            ActiveScaleFilter::Fsr1FallbackBilinear
                        }
                    }
                };

                let filter_mode = match self.scale_filter {
                    ScaleFilter::Fsr1 => 0,
                    other => other.as_u32(),
                };

                let uniforms = VideoUniforms {
                    format_mode: VideoUniforms::format_mode_for(video_frame.format),
                    filter_mode,
                    color_space: resolved_space.as_u32(),
                    color_range: resolved_range.as_u32(),
                    viewport_size: [viewport.width, viewport.height],
                    _padding: [0.0; 2],
                    brightness: self.brightness,
                    contrast: self.contrast,
                    saturation: self.saturation,
                    gamma: self.gamma,
                };

                self.queue
                    .write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&uniforms));

                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("tacklecast-clear-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
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

                pass.set_pipeline(&self.video_pipeline);
                pass.set_bind_group(0, &video_frame.bind_group, &[]);
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
        } else {
            self.active_scale_filter = match self.scale_filter {
                ScaleFilter::Bilinear => ActiveScaleFilter::Bilinear,
                ScaleFilter::Bicubic => ActiveScaleFilter::Bicubic,
                ScaleFilter::Lanczos => ActiveScaleFilter::Lanczos,
                ScaleFilter::Fsr1 => ActiveScaleFilter::Fsr1,
            };

            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tacklecast-clear-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
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
        }

        if let Some(ui) = ui.as_ref() {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tacklecast-egui-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            let mut pass = pass.forget_lifetime();
            self.egui_renderer
                .render(&mut pass, &ui.paint_jobs, &ui.screen_descriptor);
        }

        self.queue.submit(
            ui_user_command_buffers
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );

        for texture_id in ui_texture_free {
            self.egui_renderer.free_texture(&texture_id);
        }

        self.window.pre_present_notify();
        frame.present();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn ensure_upscale_resources(
        device: &wgpu::Device,
        video_bind_group_layout: &wgpu::BindGroupLayout,
        surface_format: wgpu::TextureFormat,
        upscale_pipelines: &mut Option<UpscalePipelines>,
        upscale_textures: &mut Option<UpscaleTextures>,
        video_width: u32,
        video_height: u32,
        out_width: u32,
        out_height: u32,
    ) {
        if upscale_pipelines.is_none() {
            *upscale_pipelines = Some(Self::create_upscale_pipelines(
                device,
                video_bind_group_layout,
                surface_format,
            ));
        }

        let needs_texture_rebuild = match upscale_textures.as_ref() {
            Some(tex) => {
                tex.native_width != video_width
                    || tex.native_height != video_height
                    || tex.easu_width != out_width
                    || tex.easu_height != out_height
            }
            None => true,
        };

        if needs_texture_rebuild {
            let pipelines = upscale_pipelines.as_ref().unwrap();
            *upscale_textures = Some(Self::create_upscale_textures(
                device,
                pipelines,
                video_width,
                video_height,
                out_width,
                out_height,
            ));
        }
    }

    fn create_upscale_pipelines(
        device: &wgpu::Device,
        video_bind_group_layout: &wgpu::BindGroupLayout,
        surface_format: wgpu::TextureFormat,
    ) -> UpscalePipelines {
        let video_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tacklecast-video-shader-intermediate"),
            source: wgpu::ShaderSource::Wgsl(VIDEO_SHADER.into()),
        });
        let video_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("tacklecast-video-intermediate-pipeline-layout"),
                bind_group_layouts: &[video_bind_group_layout],
                push_constant_ranges: &[],
            });
        let yuv_to_rgb_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tacklecast-yuv-to-rgb-pipeline"),
            layout: Some(&video_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &video_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &video_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let easu_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tacklecast-fsr-easu-bind-group-layout"),
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
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });

        let easu_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tacklecast-fsr-easu-shader"),
            source: wgpu::ShaderSource::Wgsl(EASU_SHADER.into()),
        });

        let easu_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tacklecast-fsr-easu-pipeline-layout"),
            bind_group_layouts: &[&easu_bind_group_layout],
            push_constant_ranges: &[],
        });

        let easu_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tacklecast-fsr-easu-pipeline"),
            layout: Some(&easu_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &easu_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &easu_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let rcas_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tacklecast-fsr-rcas-bind-group-layout"),
                entries: &[
                    texture_layout_entry(0),
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
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

        let rcas_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tacklecast-fsr-rcas-shader"),
            source: wgpu::ShaderSource::Wgsl(RCAS_SHADER.into()),
        });

        let rcas_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tacklecast-fsr-rcas-pipeline-layout"),
            bind_group_layouts: &[&rcas_bind_group_layout],
            push_constant_ranges: &[],
        });

        let rcas_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tacklecast-fsr-rcas-pipeline"),
            layout: Some(&rcas_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &rcas_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &rcas_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let easu_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tacklecast-fsr-easu-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let easu_uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tacklecast-fsr-easu-uniforms"),
            size: std::mem::size_of::<EasuConstants>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let rcas_uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tacklecast-fsr-rcas-uniforms"),
            size: std::mem::size_of::<RcasConstants>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        UpscalePipelines {
            yuv_to_rgb_pipeline,
            easu_pipeline,
            rcas_pipeline,
            easu_bind_group_layout,
            rcas_bind_group_layout,
            easu_sampler,
            easu_uniforms,
            rcas_uniforms,
        }
    }

    fn create_upscale_textures(
        device: &wgpu::Device,
        pipelines: &UpscalePipelines,
        native_width: u32,
        native_height: u32,
        easu_width: u32,
        easu_height: u32,
    ) -> UpscaleTextures {
        let native_rgb_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tacklecast-fsr-native-rgb-texture"),
            size: wgpu::Extent3d {
                width: native_width.max(1),
                height: native_height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let native_rgb_view =
            native_rgb_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let easu_output_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tacklecast-fsr-easu-output-texture"),
            size: wgpu::Extent3d {
                width: easu_width.max(1),
                height: easu_height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let easu_output_view =
            easu_output_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let easu_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tacklecast-fsr-easu-bind-group"),
            layout: &pipelines.easu_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&native_rgb_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&pipelines.easu_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: pipelines.easu_uniforms.as_entire_binding(),
                },
            ],
        });

        let rcas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tacklecast-fsr-rcas-bind-group"),
            layout: &pipelines.rcas_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&easu_output_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: pipelines.rcas_uniforms.as_entire_binding(),
                },
            ],
        });

        UpscaleTextures {
            native_rgb_texture,
            native_rgb_view,
            native_width,
            native_height,
            easu_output_texture,
            easu_output_view,
            easu_width,
            easu_height,
            easu_bind_group,
            rcas_bind_group,
        }
    }
}

#[derive(Debug)]
pub enum RenderError {
    AdapterUnavailable,
    CreateSurface(wgpu::CreateSurfaceError),
    OutOfMemory,
    RequestDevice(wgpu::RequestDeviceError),
    Surface(wgpu::SurfaceError),
    SurfaceFormatUnavailable,
}

impl Display for RenderError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AdapterUnavailable => write!(f, "no suitable GPU adapter found"),
            Self::CreateSurface(error) => write!(f, "failed to create surface: {error}"),
            Self::OutOfMemory => write!(f, "GPU ran out of memory"),
            Self::RequestDevice(error) => write!(f, "failed to request device: {error}"),
            Self::Surface(error) => write!(f, "surface error: {error}"),
            Self::SurfaceFormatUnavailable => write!(f, "surface reported no usable formats"),
        }
    }
}

impl std::error::Error for RenderError {}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct VideoUniforms {
    format_mode: u32,
    filter_mode: u32,
    color_space: u32,
    color_range: u32,
    viewport_size: [f32; 2],
    // Uniform buffers round up to 16-byte alignment, so 32 + 16 = 48 bytes total.
    _padding: [f32; 2],
    brightness: f32,
    contrast: f32,
    saturation: f32,
    gamma: f32,
}

impl VideoUniforms {
    fn format_mode_for(format: PixelFormat) -> u32 {
        match format {
            PixelFormat::Nv12 => 0,
            PixelFormat::Yuvj422p => 1,
        }
    }
}

struct VideoFrameResources {
    width: u32,
    height: u32,
    format: PixelFormat,
    y_texture: wgpu::Texture,
    u_texture: wgpu::Texture,
    v_texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

impl VideoFrameResources {
    fn new(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        samplers: &VideoSamplers,
        uniforms: &wgpu::Buffer,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Self {
        let y_texture = create_plane_texture(device, width, height, "y");
        let (chroma_width, chroma_height) = match format {
            PixelFormat::Nv12 => (width / 2, height / 2),
            PixelFormat::Yuvj422p => (width / 2, height),
        };
        let u_texture = create_plane_texture(device, chroma_width, chroma_height, "u");
        let v_texture = create_plane_texture(device, chroma_width, chroma_height, "v");

        let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let u_view = u_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let v_view = v_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tacklecast-video-bind-group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&u_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&v_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&samplers.filtering),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::Sampler(&samplers.nearest),
                },
            ],
        });

        Self {
            width,
            height,
            format,
            y_texture,
            u_texture,
            v_texture,
            bind_group,
        }
    }
}

/// Align a byte count up to the next multiple of `alignment`.
#[cfg(all(target_os = "windows", feature = "gpu-decode"))]
fn align_up(value: u32, alignment: u32) -> u32 {
    (value + alignment - 1) & !(alignment - 1)
}

fn texture_layout_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            multisampled: false,
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    }
}

fn create_plane_texture(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    label: &str,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(&format!("tacklecast-{label}-plane")),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn upload_plane_r8(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    data: &[u8],
    scratch: &mut Vec<u8>,
) {
    let (padded_data, bytes_per_row) =
        pad_rows_into(data, width as usize, height as usize, scratch);
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        padded_data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

/// Pad rows to wgpu alignment using a reusable scratch buffer.
/// Returns the data slice to upload and the padded bytes-per-row.
/// When no padding is needed, returns the input data directly.
fn pad_rows_into<'a>(
    data: &'a [u8],
    row_bytes: usize,
    rows: usize,
    scratch: &'a mut Vec<u8>,
) -> (&'a [u8], u32) {
    let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
    let padded_row_bytes = row_bytes.next_multiple_of(alignment);

    if row_bytes == padded_row_bytes {
        return (data, row_bytes as u32);
    }

    let needed = padded_row_bytes * rows;
    scratch.resize(needed, 0);
    for row in 0..rows {
        let src_start = row * row_bytes;
        let dst_start = row * padded_row_bytes;
        scratch[dst_start..dst_start + row_bytes]
            .copy_from_slice(&data[src_start..src_start + row_bytes]);
        // Zero padding bytes (only needed on first use or if dimensions grew)
        for b in &mut scratch[dst_start + row_bytes..dst_start + padded_row_bytes] {
            *b = 0;
        }
    }

    (scratch, padded_row_bytes as u32)
}

fn deinterleave_nv12_into<'a>(
    width: u32,
    height: u32,
    data: &[u8],
    u_scratch: &'a mut Vec<u8>,
    v_scratch: &'a mut Vec<u8>,
) -> (&'a [u8], &'a [u8]) {
    let chroma_width = (width / 2) as usize;
    let chroma_height = (height / 2) as usize;
    let needed = chroma_width * chroma_height;
    u_scratch.resize(needed, 0);
    v_scratch.resize(needed, 0);

    for y in 0..chroma_height {
        for x in 0..chroma_width {
            let src_index = (y * chroma_width + x) * 2;
            let dst_index = y * chroma_width + x;
            u_scratch[dst_index] = data[src_index];
            v_scratch[dst_index] = data[src_index + 1];
        }
    }

    (&u_scratch[..needed], &v_scratch[..needed])
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq)]
pub struct EasuConstants {
    pub con0: [f32; 4],
    pub con1: [f32; 4],
    pub con2: [f32; 4],
    pub con3: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq)]
pub struct RcasConstants {
    pub con: [f32; 4],
    pub output_size: [f32; 2],
    pub _pad: [f32; 2],
}

impl RcasConstants {
    pub fn new(sharpness: f32, out_w: f32, out_h: f32) -> Self {
        Self {
            con: [(sharpness * 0.5).clamp(0.0, 1.0), 0.0, 0.0, 0.0],
            output_size: [out_w, out_h],
            _pad: [0.0; 2],
        }
    }
}

pub fn calculate_easu_constants(in_w: f32, in_h: f32, out_w: f32, out_h: f32) -> EasuConstants {
    EasuConstants {
        con0: [
            in_w / out_w,
            in_h / out_h,
            0.5 * in_w / out_w - 0.5,
            0.5 * in_h / out_h - 0.5,
        ],
        con1: [1.0 / in_w, 1.0 / in_h, 1.0 / in_w, -1.0 / in_h],
        con2: [-1.0 / in_w, 2.0 / in_h, 1.0 / in_w, 2.0 / in_h],
        con3: [0.0, 4.0 / in_h, out_w, out_h],
    }
}

struct UpscalePipelines {
    yuv_to_rgb_pipeline: wgpu::RenderPipeline,
    easu_pipeline: wgpu::RenderPipeline,
    rcas_pipeline: wgpu::RenderPipeline,
    easu_bind_group_layout: wgpu::BindGroupLayout,
    rcas_bind_group_layout: wgpu::BindGroupLayout,
    easu_sampler: wgpu::Sampler,
    easu_uniforms: wgpu::Buffer,
    rcas_uniforms: wgpu::Buffer,
}

#[allow(dead_code)]
struct UpscaleTextures {
    native_rgb_texture: wgpu::Texture,
    native_rgb_view: wgpu::TextureView,
    native_width: u32,
    native_height: u32,

    easu_output_texture: wgpu::Texture,
    easu_output_view: wgpu::TextureView,
    easu_width: u32,
    easu_height: u32,

    easu_bind_group: wgpu::BindGroup,
    rcas_bind_group: wgpu::BindGroup,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_deinterleave_splits_uv_pairs() {
        let mut u_scratch = Vec::new();
        let mut v_scratch = Vec::new();
        let (u_plane, v_plane) =
            deinterleave_nv12_into(4, 2, &[10, 20, 30, 40], &mut u_scratch, &mut v_scratch);
        assert_eq!(u_plane, &[10, 30]);
        assert_eq!(v_plane, &[20, 40]);
    }

    #[test]
    fn viewport_letterboxes_wider_surface() {
        let viewport = calculate_video_viewport(1920.0, 1080.0, 4.0, 3.0);
        assert!(viewport.width < 1920.0);
        assert_eq!(viewport.height, 1080.0);
    }

    #[test]
    fn video_uniforms_layout() {
        assert_eq!(std::mem::size_of::<VideoUniforms>(), 48);
    }

    #[test]
    fn easu_constants_layout() {
        assert_eq!(std::mem::size_of::<EasuConstants>(), 64);
    }

    #[test]
    fn rcas_constants_layout() {
        assert_eq!(std::mem::size_of::<RcasConstants>(), 32);
    }

    #[test]
    fn easu_constants_calculation() {
        let con = calculate_easu_constants(1920.0, 1080.0, 3840.0, 2160.0);
        assert!((con.con0[0] - 0.5).abs() < 1e-6);
        assert!((con.con0[1] - 0.5).abs() < 1e-6);
        assert!((con.con0[2] - (-0.25)).abs() < 1e-6);
        assert!((con.con0[3] - (-0.25)).abs() < 1e-6);
        assert!((con.con1[0] - (1.0 / 1920.0)).abs() < 1e-6);
        assert!((con.con1[1] - (1.0 / 1080.0)).abs() < 1e-6);
        assert_eq!(con.con3[2], 3840.0);
        assert_eq!(con.con3[3], 2160.0);
    }
}

/// The two samplers the video pipeline binds. `filtering` serves the hardware
/// bilinear path; `nearest` gives the manual filter kernels unblended texels,
/// which is the whole point of weighting the taps ourselves.
struct VideoSamplers {
    filtering: wgpu::Sampler,
    nearest: wgpu::Sampler,
}

#[derive(Clone, Copy)]
struct Viewport {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn calculate_video_viewport(
    surface_width: f32,
    surface_height: f32,
    video_width: f32,
    video_height: f32,
) -> Viewport {
    let surface_aspect = surface_width / surface_height;
    let video_aspect = video_width / video_height;

    if surface_aspect > video_aspect {
        let width = surface_height * video_aspect;
        Viewport {
            x: (surface_width - width) * 0.5,
            y: 0.0,
            width,
            height: surface_height,
        }
    } else {
        let height = surface_width / video_aspect;
        Viewport {
            x: 0.0,
            y: (surface_height - height) * 0.5,
            width: surface_width,
            height,
        }
    }
}

const VIDEO_SHADER: &str = r#"
struct VideoUniforms {
    format_mode: u32,
    filter_mode: u32,
    color_space: u32,
    color_range: u32,
    viewport_size: vec2<f32>,
    _padding: vec2<f32>,
    brightness: f32,
    contrast: f32,
    saturation: f32,
    gamma: f32,
};

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var u_tex: texture_2d<f32>;
@group(0) @binding(2) var v_tex: texture_2d<f32>;
@group(0) @binding(3) var tex_sampler: sampler;
@group(0) @binding(4) var<uniform> uniforms: VideoUniforms;
@group(0) @binding(5) var nearest_sampler: sampler;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOut {
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0),
    );

    var uvs = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );

    var out: VertexOut;
    out.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    out.uv = uvs[vertex_index];
    return out;
}

// ---------------------------------------------------------------------------
// Filter kernel math
// ---------------------------------------------------------------------------

fn cubic_weight(x: f32) -> f32 {
    // Catmull-Rom (a = -0.5): good balance of sharpness and ringing
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

// ---------------------------------------------------------------------------
// Separable kernels.
//
// All three filters below are separable: the 2D weight for tap (i,j) is just
// wx[i] * wy[j]. So the per-axis weights are computed once into small arrays
// rather than re-evaluated inside the tap loop — for Lanczos-3 that is 12
// weight evaluations instead of 72, per plane, per pixel.
//
// Normalization follows from separability too: the sum of all 2D weights is
// (sum of wx) * (sum of wy), so the two axis sums are all that's needed.
// ---------------------------------------------------------------------------

// Bicubic: 4x4 taps, Catmull-Rom.
fn sample_bicubic(tex: texture_2d<f32>, uv: vec2<f32>) -> f32 {
    let dims = vec2<f32>(textureDimensions(tex));
    let texel = uv * dims - 0.5;
    let base = floor(texel);
    let frac = texel - base;

    var wx: array<f32, 4>;
    var wy: array<f32, 4>;
    var wx_sum = 0.0;
    var wy_sum = 0.0;
    for (var k = 0; k < 4; k = k + 1) {
        let offset = f32(k - 1);
        wx[k] = cubic_weight(offset - frac.x);
        wy[k] = cubic_weight(offset - frac.y);
        wx_sum = wx_sum + wx[k];
        wy_sum = wy_sum + wy[k];
    }

    var sum = 0.0;
    for (var j = 0; j < 4; j = j + 1) {
        var row = 0.0;
        for (var i = 0; i < 4; i = i + 1) {
            let pos = (base + vec2<f32>(f32(i - 1), f32(j - 1)) + 0.5) / dims;
            row = row + textureSample(tex, nearest_sampler, pos).r * wx[i];
        }
        sum = sum + row * wy[j];
    }
    return sum / max(wx_sum * wy_sum, 1e-5);
}

// Lanczos, 2-lobe: 4x4 taps, for moderate upscale ratios.
fn sample_lanczos2(tex: texture_2d<f32>, uv: vec2<f32>) -> f32 {
    let a = 2.0;
    let dims = vec2<f32>(textureDimensions(tex));
    let texel = uv * dims - 0.5;
    let base = floor(texel);
    let frac = texel - base;

    var wx: array<f32, 4>;
    var wy: array<f32, 4>;
    var wx_sum = 0.0;
    var wy_sum = 0.0;
    for (var k = 0; k < 4; k = k + 1) {
        let offset = f32(k - 1);
        wx[k] = lanczos_weight(offset - frac.x, a);
        wy[k] = lanczos_weight(offset - frac.y, a);
        wx_sum = wx_sum + wx[k];
        wy_sum = wy_sum + wy[k];
    }

    var sum = 0.0;
    for (var j = 0; j < 4; j = j + 1) {
        var row = 0.0;
        for (var i = 0; i < 4; i = i + 1) {
            let pos = (base + vec2<f32>(f32(i - 1), f32(j - 1)) + 0.5) / dims;
            row = row + textureSample(tex, nearest_sampler, pos).r * wx[i];
        }
        sum = sum + row * wy[j];
    }
    return sum / max(wx_sum * wy_sum, 1e-5);
}

// Lanczos, 3-lobe: 6x6 taps, for large upscale ratios (>2x). The wider kernel
// avoids the aliasing/ringing the 2-lobe version shows at high magnification.
fn sample_lanczos3(tex: texture_2d<f32>, uv: vec2<f32>) -> f32 {
    let a = 3.0;
    let dims = vec2<f32>(textureDimensions(tex));
    let texel = uv * dims - 0.5;
    let base = floor(texel);
    let frac = texel - base;

    var wx: array<f32, 6>;
    var wy: array<f32, 6>;
    var wx_sum = 0.0;
    var wy_sum = 0.0;
    for (var k = 0; k < 6; k = k + 1) {
        let offset = f32(k - 2);
        wx[k] = lanczos_weight(offset - frac.x, a);
        wy[k] = lanczos_weight(offset - frac.y, a);
        wx_sum = wx_sum + wx[k];
        wy_sum = wy_sum + wy[k];
    }

    var sum = 0.0;
    for (var j = 0; j < 6; j = j + 1) {
        var row = 0.0;
        for (var i = 0; i < 6; i = i + 1) {
            let pos = (base + vec2<f32>(f32(i - 2), f32(j - 2)) + 0.5) / dims;
            row = row + textureSample(tex, nearest_sampler, pos).r * wx[i];
        }
        sum = sum + row * wy[j];
    }
    return sum / max(wx_sum * wy_sum, 1e-5);
}

// ---------------------------------------------------------------------------
// Filter dispatch — selects the algorithm from the filter_mode uniform and
// adapts kernel width to the viewport-to-source scale ratio.
//
// When the viewport is no larger than the source texture (downscaling or 1:1),
// the custom kernels are bypassed for hardware bilinear — an upscale kernel
// applied to minification undersamples the source and aliases.
//
// The ratio is per plane, deliberately. Each plane has its own sampling
// density: for 4:2:2 the chroma planes are half-width, so they are being
// magnified 2x horizontally even when the luma plane maps 1:1 to the screen,
// and they take the filtered path while luma takes the bilinear bypass. That
// is the correct reading of the ratio — chroma really is upscaled there, and
// filtering it properly is what keeps colour edges from bleeding. It does mean
// the bypass rarely applies to chroma.
// ---------------------------------------------------------------------------

fn sample_plane(tex: texture_2d<f32>, uv: vec2<f32>) -> f32 {
    let src_dims = vec2<f32>(textureDimensions(tex));
    let scale = uniforms.viewport_size / max(src_dims, vec2<f32>(1.0));
    let max_scale = max(scale.x, scale.y);

    if max_scale <= 1.0 || uniforms.filter_mode == 0u {
        // Bilinear: hardware-accelerated, or forced when downscaling
        return textureSample(tex, tex_sampler, uv).r;
    }

    if uniforms.filter_mode == 1u {
        return sample_bicubic(tex, uv);
    }

    // filter_mode == 2u: Lanczos with adaptive lobe count
    if max_scale > 2.0 {
        return sample_lanczos3(tex, uv);
    }
    return sample_lanczos2(tex, uv);
}

// ---------------------------------------------------------------------------
// YUV → RGB color conversion
// ---------------------------------------------------------------------------

fn decode_yuv_to_rgb(uv: vec2<f32>) -> vec3<f32> {
    let y_raw = sample_plane(y_tex, uv);
    let u_raw = sample_plane(u_tex, uv);
    let v_raw = sample_plane(v_tex, uv);

    var y: f32;
    var cb: f32;
    var cr: f32;

    // Range decompression
    // uniforms.color_range: 0u = Limited (16-235), 1u = Full (0-255)
    if uniforms.color_range == 0u {
        y = clamp((y_raw - (16.0 / 255.0)) * (255.0 / 219.0), 0.0, 1.0);
        cb = (u_raw - (128.0 / 255.0)) * (255.0 / 224.0);
        cr = (v_raw - (128.0 / 255.0)) * (255.0 / 224.0);
    } else {
        y = y_raw;
        cb = u_raw - (128.0 / 255.0);
        cr = v_raw - (128.0 / 255.0);
    }

    var rgb: vec3<f32>;

    // Color space matrix
    // uniforms.color_space: 0u = Rec.709, 1u = BT.601, 2u = BT.2020
    if uniforms.color_space == 0u {
        // Rec. 709 (BT.709)
        rgb = vec3<f32>(
            y + 1.5748 * cr,
            y - 0.187324 * cb - 0.468124 * cr,
            y + 1.8556 * cb,
        );
    } else if uniforms.color_space == 1u {
        // BT.601
        rgb = vec3<f32>(
            y + 1.402 * cr,
            y - 0.344136 * cb - 0.714136 * cr,
            y + 1.772 * cb,
        );
    } else {
        // BT.2020
        rgb = vec3<f32>(
            y + 1.4746 * cr,
            y - 0.164553 * cb - 0.571353 * cr,
            y + 1.8814 * cb,
        );
    }

    return rgb;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    var rgb = decode_yuv_to_rgb(in.uv);

    // Picture adjustments: brightness, contrast, saturation, gamma
    rgb = (rgb - 0.5) * uniforms.contrast + 0.5 + uniforms.brightness;
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    rgb = mix(vec3<f32>(luma), rgb, uniforms.saturation);
    rgb = pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(1.0 / max(uniforms.gamma, 0.01)));

    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

#[allow(dead_code)]
pub const QUAD_VERTEX_SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
    );
    var uvs = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );
    var out: VertexOutput;
    out.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    out.uv = uvs[vertex_index];
    return out;
}
"#;

pub const EASU_SHADER: &str = r#"
struct EasuConstants {
    con0: vec4<f32>,
    con1: vec4<f32>,
    con2: vec4<f32>,
    con3: vec4<f32>,
};

@group(0) @binding(0) var easu_tex: texture_2d<f32>;
@group(0) @binding(1) var easu_sampler: sampler;
@group(0) @binding(2) var<uniform> easu_con: EasuConstants;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
    );
    var uvs = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );
    var out: VertexOutput;
    out.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    out.uv = uvs[vertex_index];
    return out;
}

fn FsrEasuTapF(
    aC: ptr<function, vec3<f32>>,
    aW: ptr<function, f32>,
    off: vec2<f32>,
    dir: vec2<f32>,
    len: vec2<f32>,
    lob: f32,
    clp: f32,
    c: vec3<f32>,
) {
    var v: vec2<f32>;
    v.x = (off.x * dir.x) + (off.y * dir.y);
    v.y = (off.x * (-dir.y)) + (off.y * dir.x);
    v = v * len;
    var d2 = v.x * v.x + v.y * v.y;
    d2 = min(d2, clp);
    var wB = (2.0 / 5.0) * d2 - 1.0;
    var wA = lob * d2 - 1.0;
    wB = wB * wB;
    wA = wA * wA;
    wB = (25.0 / 16.0) * wB - (25.0 / 16.0 - 1.0);
    let w = wB * wA;
    *aC = *aC + c * w;
    *aW = *aW + w;
}

fn FsrEasuSetF(
    dir: ptr<function, vec2<f32>>,
    len: ptr<function, f32>,
    pp: vec2<f32>,
    biS: bool, biT: bool, biU: bool, biV: bool,
    lA: f32, lB: f32, lC: f32, lD: f32, lE: f32,
) {
    var w = 0.0;
    if (biS) { w = (1.0 - pp.x) * (1.0 - pp.y); }
    if (biT) { w = pp.x * (1.0 - pp.y); }
    if (biU) { w = (1.0 - pp.x) * pp.y; }
    if (biV) { w = pp.x * pp.y; }

    let dc = lD - lC;
    let cb = lC - lB;
    let lenX_raw = max(abs(dc), abs(cb));
    let dirX = lD - lB;
    (*dir).x = (*dir).x + dirX * w;
    if (lenX_raw > 0.0) {
        var lenX = clamp(abs(dirX) / lenX_raw, 0.0, 1.0);
        lenX = lenX * lenX;
        *len = *len + lenX * w;
    }

    let ec = lE - lC;
    let ca = lC - lA;
    let lenY_raw = max(abs(ec), abs(ca));
    let dirY = lE - lA;
    (*dir).y = (*dir).y + dirY * w;
    if (lenY_raw > 0.0) {
        var lenY = clamp(abs(dirY) / lenY_raw, 0.0, 1.0);
        lenY = lenY * lenY;
        *len = *len + lenY * w;
    }
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let ip = floor(in.position.xy);
    let pp = ip * easu_con.con0.xy + easu_con.con0.zw;
    let fp = floor(pp);
    let pp_rel = pp - fp;

    let p0 = fp * easu_con.con1.xy + easu_con.con1.zw;
    let p1 = p0 + easu_con.con2.xy;
    let p2 = p0 + easu_con.con2.zw;
    let p3 = p0 + easu_con.con3.xy;

    let bczzR = textureGather(0, easu_tex, easu_sampler, p0);
    let bczzG = textureGather(1, easu_tex, easu_sampler, p0);
    let bczzB = textureGather(2, easu_tex, easu_sampler, p0);

    let ijfeR = textureGather(0, easu_tex, easu_sampler, p1);
    let ijfeG = textureGather(1, easu_tex, easu_sampler, p1);
    let ijfeB = textureGather(2, easu_tex, easu_sampler, p1);

    let klhgR = textureGather(0, easu_tex, easu_sampler, p2);
    let klhgG = textureGather(1, easu_tex, easu_sampler, p2);
    let klhgB = textureGather(2, easu_tex, easu_sampler, p2);

    let zzonR = textureGather(0, easu_tex, easu_sampler, p3);
    let zzonG = textureGather(1, easu_tex, easu_sampler, p3);
    let zzonB = textureGather(2, easu_tex, easu_sampler, p3);

    let bczzL = bczzB * 0.5 + (bczzR * 0.5 + bczzG);
    let ijfeL = ijfeB * 0.5 + (ijfeR * 0.5 + ijfeG);
    let klhgL = klhgB * 0.5 + (klhgR * 0.5 + klhgG);
    let zzonL = zzonB * 0.5 + (zzonR * 0.5 + zzonG);

    let bL = bczzL.x;
    let cL = bczzL.y;
    let iL = ijfeL.x;
    let jL = ijfeL.y;
    let fL = ijfeL.z;
    let eL = ijfeL.w;
    let kL = klhgL.x;
    let lL = klhgL.y;
    let hL = klhgL.z;
    let gL = klhgL.w;
    let oL = zzonL.z;
    let nL = zzonL.w;

    var dir = vec2<f32>(0.0);
    var len = 0.0;
    FsrEasuSetF(&dir, &len, pp_rel, true, false, false, false, bL, eL, fL, gL, jL);
    FsrEasuSetF(&dir, &len, pp_rel, false, true, false, false, cL, fL, gL, hL, kL);
    FsrEasuSetF(&dir, &len, pp_rel, false, false, true, false, fL, iL, jL, kL, nL);
    FsrEasuSetF(&dir, &len, pp_rel, false, false, false, true, gL, jL, kL, lL, oL);

    let dir2 = dir * dir;
    var dirR = dir2.x + dir2.y;
    let zro = dirR < (1.0 / 32768.0);
    if (zro) {
        dirR = 1.0;
        dir.x = 1.0;
    } else {
        dirR = inverseSqrt(dirR);
    }
    dir = dir * dirR;

    len = len * 0.5;
    len = len * len;

    let stretch = (dir.x * dir.x + dir.y * dir.y) / max(max(abs(dir.x), abs(dir.y)), 1e-5);
    let len2 = vec2<f32>(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
    let lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    let clp = 1.0 / lob;

    let min4 = min(
        min(vec3<f32>(ijfeR.z, ijfeG.z, ijfeB.z), vec3<f32>(klhgR.w, klhgG.w, klhgB.w)),
        min(vec3<f32>(ijfeR.y, ijfeG.y, ijfeB.y), vec3<f32>(klhgR.x, klhgG.x, klhgB.x))
    );
    let max4 = max(
        max(vec3<f32>(ijfeR.z, ijfeG.z, ijfeB.z), vec3<f32>(klhgR.w, klhgG.w, klhgB.w)),
        max(vec3<f32>(ijfeR.y, ijfeG.y, ijfeB.y), vec3<f32>(klhgR.x, klhgG.x, klhgB.x))
    );

    var aC = vec3<f32>(0.0);
    var aW = 0.0;
    FsrEasuTapF(&aC, &aW, vec2<f32>( 0.0, -1.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(bczzR.x, bczzG.x, bczzB.x));
    FsrEasuTapF(&aC, &aW, vec2<f32>( 1.0, -1.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(bczzR.y, bczzG.y, bczzB.y));
    FsrEasuTapF(&aC, &aW, vec2<f32>(-1.0,  1.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(ijfeR.x, ijfeG.x, ijfeB.x));
    FsrEasuTapF(&aC, &aW, vec2<f32>( 0.0,  1.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(ijfeR.y, ijfeG.y, ijfeB.y));
    FsrEasuTapF(&aC, &aW, vec2<f32>( 0.0,  0.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(ijfeR.z, ijfeG.z, ijfeB.z));
    FsrEasuTapF(&aC, &aW, vec2<f32>(-1.0,  0.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(ijfeR.w, ijfeG.w, ijfeB.w));
    FsrEasuTapF(&aC, &aW, vec2<f32>( 1.0,  1.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(klhgR.x, klhgG.x, klhgB.x));
    FsrEasuTapF(&aC, &aW, vec2<f32>( 2.0,  1.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(klhgR.y, klhgG.y, klhgB.y));
    FsrEasuTapF(&aC, &aW, vec2<f32>( 2.0,  0.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(klhgR.z, klhgG.z, klhgB.z));
    FsrEasuTapF(&aC, &aW, vec2<f32>( 1.0,  0.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(klhgR.w, klhgG.w, klhgB.w));
    FsrEasuTapF(&aC, &aW, vec2<f32>( 1.0,  2.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(zzonR.z, zzonG.z, zzonB.z));
    FsrEasuTapF(&aC, &aW, vec2<f32>( 0.0,  2.0) - pp_rel, dir, len2, lob, clp, vec3<f32>(zzonR.w, zzonG.w, zzonB.w));

    let rgb = clamp(aC / max(aW, 1e-5), min4, max4);
    return vec4<f32>(rgb, 1.0);
}
"#;

pub const RCAS_SHADER: &str = r#"
struct RcasConstants {
    con: vec4<f32>,
    output_size: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var rcas_tex: texture_2d<f32>;
@group(0) @binding(1) var<uniform> rcas_con: RcasConstants;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
    );
    var uvs = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );
    var out: VertexOutput;
    out.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    out.uv = uvs[vertex_index];
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let max_coords = vec2<i32>(rcas_con.output_size) - vec2<i32>(1, 1);
    let sp = clamp(vec2<i32>(floor(in.uv * rcas_con.output_size)), vec2<i32>(0), max_coords);

    let b = textureLoad(rcas_tex, clamp(sp + vec2<i32>( 0, -1), vec2<i32>(0), max_coords), 0).rgb;
    let d = textureLoad(rcas_tex, clamp(sp + vec2<i32>(-1,  0), vec2<i32>(0), max_coords), 0).rgb;
    let e = textureLoad(rcas_tex, sp,                                                      0).rgb;
    let f = textureLoad(rcas_tex, clamp(sp + vec2<i32>( 1,  0), vec2<i32>(0), max_coords), 0).rgb;
    let h = textureLoad(rcas_tex, clamp(sp + vec2<i32>( 0,  1), vec2<i32>(0), max_coords), 0).rgb;

    let bL = b.b * 0.5 + (b.r * 0.5 + b.g);
    let dL = d.b * 0.5 + (d.r * 0.5 + d.g);
    let eL = e.b * 0.5 + (e.r * 0.5 + e.g);
    let fL = f.b * 0.5 + (f.r * 0.5 + f.g);
    let hL = h.b * 0.5 + (h.r * 0.5 + h.g);

    var nz = 0.25 * bL + 0.25 * dL + 0.25 * fL + 0.25 * hL - eL;
    let mxL = max(max(max(bL, dL), max(eL, fL)), hL);
    let mnL = min(min(min(bL, dL), min(eL, fL)), hL);
    nz = clamp(abs(nz) / max(mxL - mnL, 1e-5), 0.0, 1.0);
    nz = -0.5 * nz + 1.0;

    let mn4R = min(min(b.r, d.r), min(f.r, h.r));
    let mn4G = min(min(b.g, d.g), min(f.g, h.g));
    let mn4B = min(min(b.b, d.b), min(f.b, h.b));

    let mx4R = max(max(b.r, d.r), max(f.r, h.r));
    let mx4G = max(max(b.g, d.g), max(f.g, h.g));
    let mx4B = max(max(b.b, d.b), max(f.b, h.b));

    let peakC = vec2<f32>(1.0, -4.0);
    let hitMinR = min(mn4R, e.r) / max(4.0 * mx4R, 1e-5);
    let hitMinG = min(mn4G, e.g) / max(4.0 * mx4G, 1e-5);
    let hitMinB = min(mn4B, e.b) / max(4.0 * mx4B, 1e-5);

    // Retain negative sign: denominator 4.0 * mn4 + peakC.y is <= 0.0 for normalized inputs [0, 1].
    let hitMaxR = (peakC.x - max(mx4R, e.r)) / min(4.0 * mn4R + peakC.y, -1e-5);
    let hitMaxG = (peakC.x - max(mx4G, e.g)) / min(4.0 * mn4G + peakC.y, -1e-5);
    let hitMaxB = (peakC.x - max(mx4B, e.b)) / min(4.0 * mn4B + peakC.y, -1e-5);

    let lobeR = max(-hitMinR, hitMaxR);
    let lobeG = max(-hitMinG, hitMaxG);
    let lobeB = max(-hitMinB, hitMaxB);

    let FSR_RCAS_LIMIT = 0.25 - (1.0 / 16.0);
    var lobe = max(-FSR_RCAS_LIMIT, min(max(max(lobeR, lobeG), lobeB), 0.0)) * rcas_con.con.x;
    lobe = lobe * nz;

    let rcpL = 1.0 / (4.0 * lobe + 1.0);
    let pixR = (lobe * b.r + lobe * d.r + lobe * h.r + lobe * f.r + e.r) * rcpL;
    let pixG = (lobe * b.g + lobe * d.g + lobe * h.g + lobe * f.g + e.g) * rcpL;
    let pixB = (lobe * b.b + lobe * d.b + lobe * h.b + lobe * f.b + e.b) * rcpL;

    return vec4<f32>(clamp(vec3<f32>(pixR, pixG, pixB), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;
