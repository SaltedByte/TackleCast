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
    // Golden values: limited black/white, full black/white, BT.709 red and
    // BT.601 red, plus isolated neutral/default and picture-adjustment checks.
    let cases = [
        ([16, 128, 128], 0, 0, [0.0, 1.0, 1.0, 1.0], [0, 0, 0]),
        ([235, 128, 128], 0, 0, [0.0, 1.0, 1.0, 1.0], [255, 255, 255]),
        ([0, 128, 128], 1, 0, [0.0, 1.0, 1.0, 1.0], [0, 0, 0]),
        ([255, 128, 128], 1, 0, [0.0, 1.0, 1.0, 1.0], [255, 255, 255]),
        ([63, 102, 240], 0, 0, [0.0, 1.0, 1.0, 1.0], [255, 0, 0]),
        ([81, 90, 240], 0, 1, [0.0, 1.0, 1.0, 1.0], [254, 0, 0]),
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
