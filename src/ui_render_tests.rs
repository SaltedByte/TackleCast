//! Optional offscreen review of the actual settings UI through egui-wgpu.
//! This draws static settings pages; zero-frame scheduling is tested separately.
use super::*;

#[test]
#[ignore = "requires a graphics adapter; optionally set TACKLECAST_UI_PREVIEWS to export PNGs"]
fn settings_pages_render_with_readable_controls() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let directory = std::env::var_os("TACKLECAST_UI_PREVIEWS").map(std::path::PathBuf::from);
    if let Some(directory) = &directory {
        std::fs::create_dir_all(directory).unwrap();
    }
    for theme in [Theme::Vice, Theme::Classic] {
        for (page, slow_usb) in [
            (PreviewPage::Video, false),
            (PreviewPage::Picture, false),
            (PreviewPage::Audio, false),
            (PreviewPage::FrameGen, false),
            (PreviewPage::Display, false),
            (PreviewPage::Overlay, false),
            (PreviewPage::Video, true),
            (PreviewPage::None, false),
            (PreviewPage::None, true),
        ] {
            let ctx = egui::Context::default();
            let mut renderer = egui_wgpu::Renderer::new(
                &device,
                wgpu::TextureFormat::Rgba8Unorm,
                egui_wgpu::RendererOptions {
                    dithering: false,
                    ..Default::default()
                },
            );
            let mut settings = Settings {
                theme,
                framegen_enabled: page == PreviewPage::FrameGen,
                ..Default::default()
            };
            if page == PreviewPage::Overlay {
                settings.set_overlay_mode(crate::overlay::Mode::Custom);
            }
            let applied = settings.clone();
            let videos = vec!["HD (ShadowCast 3)".to_owned()];
            capabilities::cache_test_device(&videos[0]);
            if slow_usb && page != PreviewPage::None {
                let mut caps = (*capabilities::get_device_capabilities(&videos[0])).clone();
                caps.usb_warning = crate::usb_link::warning_for_speed(3);
                capabilities::cache_test_capabilities(&videos[0], caps);
            }
            let audio = vec![AudioDevice {
                index: 0,
                name: "ShadowCast 3".into(),
            }];
            let overlay = OverlayInfo {
                width: Some(2560),
                height: Some(1440),
                fps: Some(60.0),
                framegen: crate::framegen::FgDiagnostics {
                    active: slow_usb && page == PreviewPage::None,
                    unique_fps: 30.0,
                    displayed_fps: 60.0,
                    ..Default::default()
                },
                filter: ScaleFilter::Bilinear,
                filter_label: "Bilinear".into(),
                color_space: ColorSpace::Auto,
                color_range: ColorRange::Auto,
                active_format: None,
                nominal_rate: None,
                color_info: ColorInfo::default(),
                show_overlay: page == PreviewPage::None,
                mode: crate::overlay::Mode::Compact,
                layout: Default::default(),
                status_message: None,
                status_is_alert: false,
            };
            let frame = UiFrame {
                overlay: &overlay,
                settings: &applied,
                video_devices: &videos,
                audio_inputs: &audio,
                audio_outputs: &[],
                is_fullscreen: false,
                audio_status: "Capture unavailable",
                devices_busy: false,
                framegen: &crate::framegen::FgDiagnostics::default(),
                framegen_forced: false,
                recorder_status: "",
                recorder_available: false,
                update_status: &crate::updater::UpdateStatus::Idle,
            };
            let mut tab = page.menu_tab().unwrap_or(MenuTab::Display);
            if page == PreviewPage::Overlay {
                ctx.data_mut(|data| data.insert_temp(egui::Id::new("test_overlay_focus"), true));
            }
            let width = 1280;
            let height = 720;
            let target = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("settings-review"),
                size: wgpu::Extent3d {
                    width,
                    height,
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
            let screen = ScreenDescriptor {
                size_in_pixels: [width, height],
                pixels_per_point: 1.0,
            };
            // Settle egui's sizing/scroll animation; this is visual review only.
            for pass in 0..12 {
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width as f32, height as f32),
                        )),
                        time: Some(pass as f64 / 60.0),
                        ..Default::default()
                    },
                    |root| {
                        configure_style(root.ctx(), theme);
                        if page == PreviewPage::None {
                            draw_overlay(root.ctx(), &overlay);
                        } else {
                            draw_menu(
                                root.ctx(),
                                &mut settings,
                                &mut tab,
                                &frame,
                                &mut UiOutput::default(),
                            );
                        }
                    },
                );
                for (id, delta) in output.textures_delta.set {
                    renderer.update_texture(&device, &queue, id, &delta);
                }
                let geometry = ctx
                    .data(|data| {
                        data.get_temp::<Vec<WidgetGeometry>>(egui::Id::new("settings_geometry"))
                    })
                    .unwrap_or_default();
                for shape in &output.shapes {
                    if let egui::Shape::Circle(circle) = &shape.shape {
                        if geometry.iter().any(|widget| {
                            widget.role == "slider"
                                && widget.rect.contains(circle.center)
                                && shape.clip_rect.contains_rect(widget.rect)
                        }) {
                            let outer_radius = circle.radius + circle.stroke.width * 0.5;
                            let painted = egui::Rect::from_center_size(
                                circle.center,
                                egui::Vec2::splat(outer_radius * 2.0),
                            );
                            assert!(
                                shape.clip_rect.contains_rect(painted),
                                "slider thumb border clipped: {painted:?} in {:?}",
                                shape.clip_rect
                            );
                        }
                    }
                }
                let jobs = ctx.tessellate(output.shapes, 1.0);
                let mut encoder = device.create_command_encoder(&Default::default());
                let commands =
                    renderer.update_buffers(&device, &queue, &mut encoder, &jobs, &screen);
                {
                    let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: None,
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        ..Default::default()
                    });
                    renderer.render(&mut pass.forget_lifetime(), &jobs, &screen);
                }
                queue.submit(commands.into_iter().chain([encoder.finish()]));
                for id in output.textures_delta.free {
                    renderer.free_texture(&id);
                }
            }
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: (width * height * 4) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = device.create_command_encoder(&Default::default());
            encoder.copy_texture_to_buffer(
                target.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(width * 4),
                        rows_per_image: Some(height),
                    },
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            queue.submit([encoder.finish()]);
            let (tx, rx) = std::sync::mpsc::channel();
            readback
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| tx.send(result).unwrap());
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            rx.recv().unwrap().unwrap();
            let pixels = readback.slice(..).get_mapped_range();
            let geometry = ctx
                .data(|data| {
                    data.get_temp::<Vec<WidgetGeometry>>(egui::Id::new("settings_geometry"))
                })
                .unwrap_or_default();
            // Each fully visible numeric field must contain bright text, not
            // merely a correctly allocated but blank or overpainted rectangle.
            for number in geometry.iter().filter(|record| record.role == "number") {
                let rect = number.rect.shrink(5.0);
                if rect.top() < 150.0 || rect.bottom() > 620.0 {
                    continue;
                }
                let bright_pixels = (rect.top() as usize..rect.bottom() as usize)
                    .flat_map(|y| {
                        (rect.left() as usize..rect.right() as usize)
                            .map(move |x| (y * width as usize + x) * 4)
                    })
                    .filter(|offset| {
                        pixels[*offset] > 150
                            && pixels[*offset + 1] > 150
                            && pixels[*offset + 2] > 150
                    })
                    .count();
                assert!(
                    bright_pixels > 10,
                    "numeric text must be visible: {theme:?} {page:?} {number:?}"
                );
            }
            if let Some(directory) = &directory {
                image::save_buffer(
                    directory.join(format!(
                        "{theme:?}-{page:?}{}.png",
                        if slow_usb {
                            if page == PreviewPage::None {
                                "-FG"
                            } else {
                                "-USB2"
                            }
                        } else {
                            ""
                        }
                    )),
                    &pixels,
                    width,
                    height,
                    image::ColorType::Rgba8,
                )
                .unwrap();
            }
            drop(pixels);
            readback.unmap();
        }
    }
}
