use super::*;
use wgpu::util::DeviceExt;

/// Exercise the production shader and plane resources, not a CPU copy of its
/// implementation. Run explicitly on test machines with a graphics adapter.
#[test]
#[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
fn gpu_shader_color_and_picture_reference() {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("A wgpu adapter is required for this explicit graphics test");
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
            .unwrap();
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("color-reference-layout"),
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
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("production-video-shader"),
        source: wgpu::ShaderSource::Wgsl(VIDEO_SHADER.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[&layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(&pipeline_layout),
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
                format: wgpu::TextureFormat::Rgba8Unorm,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview: None,
        cache: None,
    });
    let samplers = VideoSamplers {
        filtering: device.create_sampler(&wgpu::SamplerDescriptor::default()),
        nearest: device.create_sampler(&wgpu::SamplerDescriptor::default()),
    };
    // Golden values: limited black/white, full black/white, BT.709 red, BT.601 red
    // (converted to Rec. 709 primaries), BT.2020 white, plus isolated neutral/default
    // and picture-adjustment checks.
    let cases = [
        ([16, 128, 128], 0, 0, [0.0, 1.0, 1.0, 1.0], [0, 0, 0]),
        ([235, 128, 128], 0, 0, [0.0, 1.0, 1.0, 1.0], [255, 255, 255]),
        ([0, 128, 128], 1, 0, [0.0, 1.0, 1.0, 1.0], [0, 0, 0]),
        ([255, 128, 128], 1, 0, [0.0, 1.0, 1.0, 1.0], [255, 255, 255]),
        ([63, 102, 240], 0, 0, [0.0, 1.0, 1.0, 1.0], [255, 0, 0]),
        ([81, 90, 240], 0, 1, [0.0, 1.0, 1.0, 1.0], [247, 41, 0]),
        ([235, 128, 128], 0, 2, [0.0, 1.0, 1.0, 1.0], [255, 255, 255]),
        ([128, 128, 128], 1, 0, [0.0, 1.0, 1.0, 1.0], [128, 128, 128]),
        ([128, 128, 128], 1, 0, [0.1, 1.0, 1.0, 1.0], [154, 154, 154]),
        ([63, 102, 240], 0, 0, [0.0, 1.0, 0.0, 1.0], [54, 54, 54]),
        ([64, 128, 128], 1, 0, [0.0, 1.0, 1.0, 2.0], [128, 128, 128]),
    ];
    for (sample, range, space, picture, expected) in cases {
        let uniforms = VideoUniforms {
            format_mode: 0,
            filter_mode: 0,
            color_space: space,
            color_range: range,
            viewport_size: [2.0, 2.0],
            _padding: [0.0; 2],
            brightness: picture[0],
            contrast: picture[1],
            saturation: picture[2],
            gamma: picture[3],
        };
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::bytes_of(&uniforms),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let frame = VideoFrameResources::new(
            &device,
            &layout,
            &samplers,
            &uniform,
            2,
            2,
            PixelFormat::Nv12,
        );
        let mut scratch = Vec::new();
        upload_plane_r8(
            &queue,
            &frame.y_texture,
            2,
            2,
            &[sample[0]; 4],
            &mut scratch,
        );
        upload_plane_r8(&queue, &frame.u_texture, 1, 1, &[sample[1]], &mut scratch);
        upload_plane_r8(&queue, &frame.v_texture, 1, 1, &[sample[2]], &mut scratch);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 512,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
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
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &frame.bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(2),
                },
            },
            wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                tx.send(result).unwrap();
            });
        device.poll(wgpu::Maintain::Wait);
        rx.recv().unwrap().unwrap();
        let data = readback.slice(..).get_mapped_range();
        for offset in [0, 4, 256, 260] {
            for channel in 0..3 {
                assert!((data[offset+channel] as i32-expected[channel]).abs()<=2,"sample {sample:?}, range {range}, space {space}, picture {picture:?}: got {:?}, expected {expected:?}",&data[offset..offset+3]);
            }
        }
        drop(data);
        readback.unmap();
    }
}

#[test]
#[ignore = "requires a working wgpu adapter; run explicitly on desktop test machines"]
fn gpu_fsr_rcas_neutral_and_strong_reference() {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("A wgpu adapter is required for this explicit graphics test");
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
            .unwrap();

    let quad_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("test-quad-shader"),
        source: wgpu::ShaderSource::Wgsl(QUAD_VERTEX_SHADER.into()),
    });
    let rcas_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("test-rcas-shader"),
        source: wgpu::ShaderSource::Wgsl(RCAS_SHADER.into()),
    });

    let rcas_bind_group_layout =
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("test-rcas-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
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

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("test-rcas-pipeline-layout"),
        bind_group_layouts: &[&rcas_bind_group_layout],
        push_constant_ranges: &[],
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("test-rcas-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &quad_shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &rcas_shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8Unorm,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview: None,
        cache: None,
    });

    // Create 4x4 input texture with horizontal step edge:
    // Row 0-1: [64, 64, 64, 255]
    // Row 2-3: [192, 192, 192, 255]
    let mut input_pixels = Vec::with_capacity(4 * 4 * 4);
    for row in 0..4 {
        let val: u8 = if row < 2 { 64 } else { 192 };
        for _ in 0..4 {
            input_pixels.extend_from_slice(&[val, val, val, 255]);
        }
    }

    let input_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test-input-texture"),
        size: wgpu::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &input_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &input_pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * 4),
            rows_per_image: Some(4),
        },
        wgpu::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
    );
    let input_view = input_texture.create_view(&Default::default());

    let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test-rcas-uniforms"),
        size: std::mem::size_of::<RcasConstants>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("test-bind-group"),
        layout: &rcas_bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&input_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: uniform_buffer.as_entire_binding(),
            },
        ],
    });

    let output_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test-output-texture"),
        size: wgpu::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let output_view = output_texture.create_view(&Default::default());

    let readback_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test-readback"),
        size: 256 * 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    // Test 1: Neutral sharpening (sharpness = 0.0) -> Output must match input exactly (identity)
    let neutral_con = RcasConstants::new(0.0, 4.0, 4.0);
    queue.write_buffer(&uniform_buffer, 0, bytemuck::bytes_of(&neutral_con));

    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("test-neutral-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &output_view,
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
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..6, 0..1);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &output_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback_buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(4),
            },
        },
        wgpu::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let (tx, rx) = std::sync::mpsc::channel();
    readback_buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |res| tx.send(res).unwrap());
    device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();
    {
        let data = readback_buffer.slice(..).get_mapped_range();
        for y in 0..4 {
            let expected_val: u8 = if y < 2 { 64 } else { 192 };
            for x in 0..4 {
                let offset = (y * 256 + x * 4) as usize;
                let actual_r = data[offset];
                assert!(
                    (actual_r as i32 - expected_val as i32).abs() <= 1,
                    "Neutral sharpness must match input identity: at ({x},{y}) got {actual_r}, expected {expected_val}"
                );
            }
        }
    }
    readback_buffer.unmap();

    // Test 2: Strong sharpening (sharpness = 1.0) -> Contrast along edge transition must be enhanced.
    // If the hitMax denominator bug had been present, the lobe would be zero and values unchanged.
    let strong_con = RcasConstants::new(1.0, 4.0, 4.0);
    queue.write_buffer(&uniform_buffer, 0, bytemuck::bytes_of(&strong_con));

    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("test-strong-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &output_view,
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
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..6, 0..1);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &output_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback_buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(4),
            },
        },
        wgpu::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let (tx, rx) = std::sync::mpsc::channel();
    readback_buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |res| tx.send(res).unwrap());
    device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();
    {
        let data = readback_buffer.slice(..).get_mapped_range();
        // At row 1 (the dark side of transition), neighbor below is bright (192).
        // Sharpening subtracts neighbor weight, darkening the edge below 64!
        let row1_r = data[256 + 4];
        // At row 2 (the bright side of transition), neighbor above is dark (64).
        // Sharpening boosts the center above 192!
        let row2_r = data[2 * 256 + 4];

        assert!(
            row1_r < 64 || row2_r > 192,
            "RCAS sharpening must boost edge contrast: row1={row1_r} (should be <64), row2={row2_r} (should be >192)"
        );
    }
    readback_buffer.unmap();
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires an NVIDIA RTX GPU with DX12/D3D11 VSR support"]
fn gpu_rtx_vsr_interop_upscale_reference() {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .expect("A DX12 adapter is required for this test");
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
            .unwrap();

    let mut vsr = crate::vsr_interop::VsrContext::try_new(&device)
        .expect("NVIDIA RTX VSR should initialize on an RTX GPU");

    let video_bind_group_layout =
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vsr-test-video-layout"),
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

    let pipelines = Renderer::create_upscale_pipelines(
        &device,
        &video_bind_group_layout,
        wgpu::TextureFormat::Bgra8Unorm,
    );

    let in_w = 128u32;
    let in_h = 128u32;
    let out_w = 256u32;
    let out_h = 256u32;

    assert!(vsr
        .ensure_resources(
            &device,
            &pipelines.rcas_bind_group_layout,
            &pipelines.rcas_uniforms,
            in_w,
            in_h,
            out_w,
            out_h,
        )
        .is_some());

    // Render a bright green clear into input_view via wgpu (DX12)
    let mut pass1 = device.create_command_encoder(&Default::default());
    {
        let res = vsr.resources.as_ref().unwrap();
        let _pass = pass1.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("vsr-test-clear-input"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &res.input_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.0,
                        g: 1.0,
                        b: 0.0,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
    }
    queue.submit([pass1.finish()]);

    assert!(
        vsr.execute_upscale(),
        "RTX VSR execute_upscale must succeed"
    );

    let res = vsr.resources.as_ref().unwrap();
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vsr-test-readback"),
        size: (out_w * 4 * out_h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut copy_enc = device.create_command_encoder(&Default::default());
    copy_enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &res.output_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(out_w * 4),
                rows_per_image: Some(out_h),
            },
        },
        wgpu::Extent3d {
            width: out_w,
            height: out_h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([copy_enc.finish()]);

    let (tx, rx) = std::sync::mpsc::channel();
    readback
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
    device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();
    let data = readback.slice(..).get_mapped_range();
    // Center pixel of the 256x256 output should be bright green ([0, ~255, 0])
    let center = ((out_h / 2) * (out_w * 4) + (out_w / 2) * 4) as usize;
    let r = data[center];
    let g = data[center + 1];
    let b = data[center + 2];
    assert!(
        g > 240 && r < 15 && b < 15,
        "Upscaled output pixel should be bright green, got RGB=({r}, {g}, {b})"
    );
}
