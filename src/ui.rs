use crate::capabilities;
use crate::color::ColorInfo;
use crate::devices::AudioDevice;
use crate::frame_rate::FrameRate;
use crate::settings::{ColorRange, ColorSpace, ScaleFilter, Settings, Theme, VideoFormat};
use crate::theme::Palette;
use egui::{
    Align, Align2, Button, Color32, ComboBox, CornerRadius, FontId, Frame, Layout, Margin,
    RichText, Slider, Stroke,
};
use egui_wgpu::ScreenDescriptor;
use egui_winit::State;
use std::time::Duration;
use winit::event::WindowEvent;
use winit::window::Window;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MenuTab {
    #[default]
    Video,
    Picture,
    Audio,
    Display,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewPage {
    None,
    Video,
    Picture,
    Audio,
    Display,
    Overlay,
}

impl PreviewPage {
    fn menu_tab(self) -> Option<MenuTab> {
        match self {
            Self::None => None,
            Self::Video => Some(MenuTab::Video),
            Self::Picture => Some(MenuTab::Picture),
            Self::Audio => Some(MenuTab::Audio),
            Self::Display | Self::Overlay => Some(MenuTab::Display),
        }
    }
}

pub struct UiState {
    egui_ctx: egui::Context,
    egui_winit: State,
    menu_open: bool,
    draft_settings: Settings,
    tab: MenuTab,
    icon_theme: Option<Theme>,
}

#[derive(Clone)]
pub struct OverlayInfo {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f32>,
    pub filter: ScaleFilter,
    pub filter_label: String,
    pub color_space: ColorSpace,
    pub color_range: ColorRange,
    pub active_format: Option<String>,
    pub nominal_rate: Option<FrameRate>,
    pub color_info: ColorInfo,
    pub show_overlay: bool,
    pub layout: crate::overlay::OverlayLayout,
    pub status_message: Option<String>,
    pub status_is_alert: bool,
}

pub struct UiFrame<'a> {
    pub overlay: &'a OverlayInfo,
    pub settings: &'a Settings,
    pub video_devices: &'a [String],
    pub audio_inputs: &'a [AudioDevice],
    pub audio_outputs: &'a [AudioDevice],
    pub is_fullscreen: bool,
    pub audio_status: &'a str,
}

pub struct PreparedUi {
    pub paint_jobs: Vec<egui::ClippedPrimitive>,
    pub textures_delta: egui::TexturesDelta,
    pub screen_descriptor: ScreenDescriptor,
    pub output: UiOutput,
}

#[derive(Default)]
pub struct UiOutput {
    pub apply_settings: Option<Settings>,
    pub restart_capture: bool,
    pub restart_audio: bool,
    pub live_video: Option<Settings>,
    pub slider_active: bool,
    pub toggle_fullscreen: bool,
    pub exit_requested: bool,
    pub repaint_after: Duration,
}

impl UiState {
    pub fn new(window: &Window, max_texture_side: usize) -> Self {
        let egui_ctx = egui::Context::default();
        configure_style(&egui_ctx, Theme::default());

        let egui_winit = State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(max_texture_side),
        );

        Self {
            egui_ctx,
            egui_winit,
            menu_open: false,
            draft_settings: Settings::default(),
            tab: MenuTab::default(),
            icon_theme: None,
        }
    }

    pub fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        self.egui_winit.on_window_event(window, event).consumed
    }

    pub fn is_menu_open(&self) -> bool {
        self.menu_open
    }

    pub fn open_menu(&mut self, settings: &Settings) {
        self.menu_open = true;
        self.draft_settings = settings.clone();
    }

    /// Startup-only selection for an explicitly gated synthetic capture preview.
    pub fn open_preview(&mut self, settings: &Settings, page: PreviewPage) {
        self.draft_settings = settings.clone();
        self.menu_open = page.menu_tab().is_some();
        if let Some(tab) = page.menu_tab() {
            self.tab = tab;
        }
        if page == PreviewPage::Overlay {
            self.egui_ctx.data_mut(|data| {
                data.insert_temp(egui::Id::new("test_overlay_focus"), true);
            });
        }
    }

    pub fn close_menu_and_apply(&mut self) -> Option<Settings> {
        if !self.menu_open {
            return None;
        }

        self.menu_open = false;
        Some(self.draft_settings.clone())
    }

    pub fn prepare(&mut self, window: &Window, frame: UiFrame<'_>) -> PreparedUi {
        if !self.menu_open {
            self.draft_settings = frame.settings.clone();
            if self.draft_settings.video_device.is_empty() && !frame.video_devices.is_empty() {
                self.draft_settings.video_device = frame.video_devices[0].clone();
            }
        }

        configure_style(&self.egui_ctx, self.draft_settings.theme);
        if self.icon_theme != Some(self.draft_settings.theme) {
            if let Ok(icon) = theme_icon(self.draft_settings.theme) {
                #[cfg(target_os = "windows")]
                {
                    use winit::platform::windows::WindowExtWindows;
                    window.set_taskbar_icon(Some(icon.clone()));
                }
                window.set_window_icon(Some(icon));
            }
            self.icon_theme = Some(self.draft_settings.theme);
        }
        let raw_input = self.egui_winit.take_egui_input(window);
        let mut ui_output = UiOutput::default();
        let full_output = self.egui_ctx.run(raw_input, |ctx| {
            if self.menu_open {
                draw_menu(
                    ctx,
                    &mut self.draft_settings,
                    &mut self.tab,
                    &frame,
                    &mut ui_output,
                );
            }
            let mut overlay = frame.overlay.clone();
            if self.menu_open {
                overlay.color_space = self.draft_settings.color_space;
                overlay.color_range = self.draft_settings.color_range;
                overlay.show_overlay = self.draft_settings.show_overlay;
                overlay.layout = self.draft_settings.overlay.clone().unwrap_or_default();
            }
            draw_overlay(ctx, &overlay);
        });
        if self.menu_open {
            ui_output.live_video = Some(self.draft_settings.clone());
        }
        ui_output.repaint_after = full_output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.repaint_delay)
            .unwrap_or(Duration::MAX);

        self.egui_winit
            .handle_platform_output(window, full_output.platform_output);

        if ui_output.apply_settings.is_some() {
            self.menu_open = false;
        }

        let pixels_per_point = egui_winit::pixels_per_point(&self.egui_ctx, window);
        let clipped_primitives = self
            .egui_ctx
            .tessellate(full_output.shapes, pixels_per_point);
        let size = window.inner_size();

        PreparedUi {
            paint_jobs: clipped_primitives,
            textures_delta: full_output.textures_delta,
            screen_descriptor: ScreenDescriptor {
                size_in_pixels: [size.width.max(1), size.height.max(1)],
                pixels_per_point,
            },
            output: ui_output,
        }
    }
}

fn theme_icon(theme: Theme) -> Result<winit::window::Icon, String> {
    let bytes: &[u8] = match theme {
        Theme::Vice => include_bytes!("../assets/icon.png"),
        Theme::Classic => include_bytes!("../assets/icon-classic.png"),
    };
    let image = image::load_from_memory(bytes)
        .map_err(|error| error.to_string())?
        .to_rgba8();
    let (width, height) = image.dimensions();
    winit::window::Icon::from_rgba(image.into_raw(), width, height)
        .map_err(|error| error.to_string())
}

fn configure_style(ctx: &egui::Context, theme: Theme) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::new("edition_theme"), theme));
    let p = Palette::for_theme(theme);
    // Keep the app's branding when the desktop reports a light system theme.
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(p.text_primary);
    style.visuals.panel_fill = Color32::TRANSPARENT;
    style.visuals.window_fill = p.menu_bg;
    style.visuals.window_stroke = Stroke::new(1.0_f32, p.menu_border);
    style.visuals.window_corner_radius = CornerRadius::same(12);
    style.visuals.menu_corner_radius = CornerRadius::same(8);
    style.visuals.widgets.noninteractive.bg_fill = p.panel_bg;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border_subtle);
    style.visuals.widgets.noninteractive.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.noninteractive.fg_stroke.color = p.text_primary;
    style.visuals.widgets.inactive.bg_fill = p.panel_bg;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    style.visuals.widgets.inactive.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.inactive.fg_stroke.color = p.text_primary;
    style.visuals.widgets.hovered.bg_fill = p.panel_hover;
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent_hover);
    style.visuals.widgets.hovered.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.hovered.fg_stroke.color = p.text_primary;
    style.visuals.widgets.active.bg_fill = p.panel_hover;
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
    style.visuals.widgets.active.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.active.fg_stroke.color = p.text_primary;
    style.visuals.selection.bg_fill = p.accent;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, p.accent);
    style.visuals.slider_trailing_fill = true;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    ctx.set_style(style);
}

fn draw_overlay(ctx: &egui::Context, overlay: &OverlayInfo) {
    let p = Palette::current(ctx);
    // Recovery status is independent of the performance master and choices.
    let text = overlay
        .status_message
        .clone()
        .or_else(|| overlay_text(overlay));
    let color = if overlay.status_is_alert {
        p.exit_border
    } else {
        p.text_primary
    };

    if text.is_none() {
        return;
    }

    use crate::overlay::Corner;
    let (anchor, offset) = match overlay.layout.corner {
        Corner::TopLeft => (Align2::LEFT_TOP, [12.0, 12.0]),
        Corner::TopRight => (Align2::RIGHT_TOP, [-12.0, 12.0]),
        Corner::BottomLeft => (Align2::LEFT_BOTTOM, [12.0, -12.0]),
        Corner::BottomRight => (Align2::RIGHT_BOTTOM, [-12.0, -12.0]),
    };
    let max_width = (ctx.screen_rect().width() - 48.0).clamp(64.0, 560.0);
    egui::Area::new(
        if overlay.status_message.is_some() {
            "capture_status"
        } else {
            "fps_overlay"
        }
        .into(),
    )
    .anchor(anchor, offset)
    .interactable(false)
    .movable(false)
    .show(ctx, |ui| {
        Frame::new()
            .fill(p.pill_bg)
            .stroke(Stroke::new(1.0_f32, Color32::from_white_alpha(30)))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(12, 6))
            .show(ui, |ui| {
                ui.set_max_width(max_width);
                ui.spacing_mut().item_spacing.y = 3.0;
                for (index, line) in text.unwrap_or_default().lines().enumerate() {
                    ui.add(
                        egui::Label::new(
                            RichText::new(line)
                                .font(FontId::monospace(if index == 0 { 13.0 } else { 11.0 }))
                                .color(if index == 0 { color } else { p.text_secondary }),
                        )
                        .wrap(),
                    );
                }
            });
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_menu(
    ctx: &egui::Context,
    draft: &mut Settings,
    tab: &mut MenuTab,
    frame: &UiFrame<'_>,
    output: &mut UiOutput,
) {
    let p = Palette::current(ctx);
    let screen = ctx.screen_rect();
    let scale = (screen.width() / 1280.0).clamp(0.85, 1.3);
    let menu_width = (480.0_f32 * scale).min((screen.width() - 56.0).max(180.0));
    let text_scale = scale.clamp(0.9, 1.15);
    // Reserve the same space for every tab. Shorter pages keep their empty
    // space, while small windows scroll instead of growing the outer menu.
    let content_height = (400.0 * text_scale).min((screen.height() - 250.0).max(60.0));
    egui::Area::new("settings_menu".into())
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .order(egui::Order::Foreground)
        .movable(false)
        .show(ctx, |ui| {
            Frame::new()
                .fill(p.menu_bg)
                .stroke(Stroke::new(1.0_f32, p.menu_border))
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::same(18))
                .show(ui, |ui| {
                    ui.set_width(menu_width);
                    ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                    ui.vertical(|ui| {
                        edition_header(ui, draft.theme);
                        tab_bar(ui, tab, text_scale);
                        egui::ScrollArea::vertical()
                            .id_salt("settings_content")
                            .max_height(content_height)
                            .min_scrolled_height(content_height)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                match *tab {
                                    MenuTab::Video => {
                                        card_section(ui, "CAPTURE SOURCE", text_scale, |ui| {
                                            if styled_button(ui, "Reconnect capture").clicked() {
                                                output.restart_capture = true;
                                            }
                                            let old = draft.video_device.clone();
                                            labeled_combo_string(
                                                ui,
                                                "Device",
                                                &mut draft.video_device,
                                                video_device_options(frame.video_devices, &old),
                                            );
                                            let caps = capabilities::get_device_capabilities(
                                                &draft.video_device,
                                            );
                                            if let Some(message) = &caps.message {
                                                warning_text(ui, message, text_scale);
                                                if message.starts_with("Reading") {
                                                    ctx.request_repaint_after(
                                                        Duration::from_millis(100),
                                                    );
                                                } else if ui
                                                    .small_button("Refresh device modes")
                                                    .clicked()
                                                {
                                                    capabilities::refresh(&draft.video_device);
                                                    ctx.request_repaint();
                                                }
                                            }
                                            capabilities::sanitize_draft_settings(draft, &caps);
                                            let resolutions =
                                                capabilities::supported_resolutions(&caps);
                                            let refs: Vec<_> =
                                                resolutions.iter().map(String::as_str).collect();
                                            ui.add_enabled_ui(!caps.modes.is_empty(), |ui| {
                                                ui.columns(2, |columns| {
                                                    labeled_combo_static(
                                                        &mut columns[0],
                                                        "Resolution",
                                                        &mut draft.resolution,
                                                        &refs,
                                                    );
                                                    let rates = capabilities::supported_rates(
                                                        &caps,
                                                        &draft.resolution,
                                                        VideoFormat::Auto,
                                                    );
                                                    fps_mode_combo(
                                                        &mut columns[1],
                                                        &mut draft.fps_mode,
                                                        &rates,
                                                    );
                                                });
                                            });
                                            capabilities::sanitize_draft_settings(draft, &caps);
                                        });
                                        card_section(ui, "FORMAT & SCALING", text_scale, |ui| {
                                            let caps = capabilities::get_device_capabilities(
                                                &draft.video_device,
                                            );
                                            let formats = capabilities::supported_video_formats(
                                                &caps,
                                                &draft.resolution,
                                                draft.get_fps(),
                                            );
                                            let live = draft.video_device
                                                == frame.settings.video_device
                                                && draft.resolution == frame.settings.resolution
                                                && draft
                                                    .get_fps()
                                                    .equivalent(frame.settings.get_fps());
                                            let active = if live {
                                                frame.overlay.active_format.as_deref()
                                            } else {
                                                None
                                            };
                                            ui.columns(2, |columns| {
                                                video_format_combo(
                                                    &mut columns[0],
                                                    "Pixel Format",
                                                    &mut draft.video_format,
                                                    &formats,
                                                    active,
                                                    live,
                                                );
                                                scaling_filter_combo(
                                                    &mut columns[1],
                                                    "Upscale Filter",
                                                    &mut draft.scaling_filter,
                                                );
                                            });
                                            if matches!(
                                                draft.scaling_filter,
                                                ScaleFilter::Fsr1 | ScaleFilter::RtxVsr
                                            ) {
                                                output.slider_active |= picture_slider(
                                                    ui,
                                                    "Sharpness",
                                                    &mut draft.sharpness,
                                                    0.0..=2.0,
                                                    0.05,
                                                    text_scale,
                                                );
                                            }
                                            // Color overrides affect the active picture immediately.
                                            color_space_combo(
                                                ui,
                                                "Color Space",
                                                &mut draft.color_space,
                                                frame.overlay.color_info,
                                                frame.overlay.height.unwrap_or(1080),
                                            );
                                            color_range_combo(
                                                ui,
                                                "Color Range",
                                                &mut draft.color_range,
                                                frame.overlay.color_info,
                                            );
                                        });
                                    }
                                    MenuTab::Picture => {
                                        card_section(ui, "IMAGE ADJUSTMENTS", text_scale, |ui| {
                                            output.slider_active |= picture_slider(
                                                ui,
                                                "Brightness",
                                                &mut draft.brightness,
                                                -1.0..=1.0,
                                                0.02,
                                                text_scale,
                                            );
                                            output.slider_active |= picture_slider(
                                                ui,
                                                "Contrast",
                                                &mut draft.contrast,
                                                0.0..=2.0,
                                                0.02,
                                                text_scale,
                                            );
                                            output.slider_active |= picture_slider(
                                                ui,
                                                "Saturation",
                                                &mut draft.saturation,
                                                0.0..=2.0,
                                                0.02,
                                                text_scale,
                                            );
                                            output.slider_active |= picture_slider(
                                                ui,
                                                "Gamma",
                                                &mut draft.gamma,
                                                0.2..=3.0,
                                                0.05,
                                                text_scale,
                                            );
                                        });
                                        if styled_button(ui, "Reset Picture to Defaults").clicked()
                                        {
                                            draft.brightness = 0.0;
                                            draft.contrast = 1.0;
                                            draft.saturation = 1.0;
                                            draft.gamma = 1.0;
                                            output.slider_active = true;
                                        }
                                    }
                                    MenuTab::Audio => {
                                        card_section(ui, "DEVICE ROUTING", text_scale, |ui| {
                                            let dshow_option = if cfg!(target_os = "windows")
                                                && !draft.video_device.is_empty()
                                            {
                                                Some(format!(
                                                    "DirectShow: {}",
                                                    crate::devices::video_device_label(
                                                        &draft.video_device
                                                    )
                                                ))
                                            } else {
                                                None
                                            };
                                            labeled_audio_combo(
                                                ui,
                                                "Input Device",
                                                &mut draft.audio_input,
                                                frame.audio_inputs,
                                                dshow_option.as_deref(),
                                            );
                                            labeled_audio_combo(
                                                ui,
                                                "Output Device",
                                                &mut draft.audio_output,
                                                frame.audio_outputs,
                                                None,
                                            );
                                        });
                                        card_section(ui, "PLAYBACK VOLUME", text_scale, |ui| {
                                            ui.checkbox(&mut draft.audio_muted, "Mute audio");
                                            output.slider_active |=
                                                labeled_volume(ui, draft, text_scale);
                                            ui.label(
                                                RichText::new(frame.audio_status)
                                                    .size(12.0 * text_scale)
                                                    .color(p.text_secondary),
                                            );
                                            if styled_button(ui, "Restart audio").clicked() {
                                                output.restart_audio = true;
                                            }
                                        });
                                    }
                                    MenuTab::Display => {
                                        card_section(ui, "WINDOW & DISPLAY", text_scale, |ui| {
                                            ui.horizontal(|ui| {
                                                ui.label("Theme");
                                                ui.selectable_value(
                                                    &mut draft.theme,
                                                    Theme::Vice,
                                                    "Vice",
                                                );
                                                ui.selectable_value(
                                                    &mut draft.theme,
                                                    Theme::Classic,
                                                    "Classic",
                                                );
                                            });
                                            if styled_button(
                                                ui,
                                                if frame.is_fullscreen {
                                                    "Exit Fullscreen (F11)"
                                                } else {
                                                    "Enter Fullscreen (F11)"
                                                },
                                            )
                                            .clicked()
                                            {
                                                output.toggle_fullscreen = true;
                                            }
                                        });
                                        card_section(ui, "ON-SCREEN OVERLAY", text_scale, |ui| {
                                            ui.checkbox(
                                                &mut draft.show_overlay,
                                                "Show performance pill",
                                            );
                                            let legacy = draft.detailed_overlay;
                                            let layout = draft.overlay.get_or_insert_with(|| {
                                                crate::overlay::OverlayLayout::legacy(legacy)
                                            });
                                            overlay_corner_combo(ui, &mut layout.corner);
                                            let focus = ctx.data_mut(|data| {
                                                data.remove_temp::<bool>(egui::Id::new(
                                                    "test_overlay_focus",
                                                ))
                                                .unwrap_or(false)
                                            });
                                            let editor =
                                                egui::CollapsingHeader::new("Customize statistics")
                                                    .default_open(focus)
                                                    .show(ui, |ui| overlay_editor(ui, layout));
                                            if focus {
                                                if let Some(body) = editor.body_response {
                                                    body.scroll_to_me(Some(Align::Center));
                                                }
                                            }
                                        });
                                        card_section(ui, "SESSION", text_scale, |ui| {
                                            if exit_button(ui).clicked() {
                                                output.exit_requested = true;
                                            }
                                        });
                                    }
                                }
                            });
                        ui.with_layout(Layout::top_down(Align::Center), |ui| {
                            ui.label(
                                RichText::new("Escape or click outside to save and close")
                                    .size(12.0 * text_scale)
                                    .color(p.text_muted),
                            );
                        });
                    });
                });
        });
    // Submit the background after controls have reported interaction; layer order
    // still places it behind the menu, with no one-frame dimming lag on a slider.
    egui::Area::new("settings_backdrop".into())
        .anchor(Align2::LEFT_TOP, [0.0, 0.0])
        .order(egui::Order::Background)
        .movable(false)
        .show(ctx, |ui| {
            let response = ui.allocate_rect(screen, egui::Sense::click());
            if !output.slider_active {
                ui.painter().rect_filled(screen, 0.0, p.dim_overlay);
            }
            if response.clicked() {
                output.apply_settings = Some(draft.clone());
            }
        });
}

fn edition_header(ui: &mut egui::Ui, theme: Theme) {
    let p = Palette::current(ui.ctx());
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 70.0), egui::Sense::hover());
    if theme == Theme::Classic {
        ui.painter().rect_filled(rect, 8.0, p.card_bg);
    }
    if theme == Theme::Vice {
        // Frame reserves the one-point stroke in addition to its 18-point
        // padding. Bleed to the border's inner edge; keep text and controls inset.
        let art_rect = egui::Rect::from_min_max(
            rect.min - egui::vec2(18.0, 18.0),
            rect.max + egui::vec2(18.0, 0.0),
        );
        let id = egui::Id::new("vice_header_texture");
        let texture = ui
            .ctx()
            .data_mut(|data| data.get_temp::<egui::TextureHandle>(id));
        let texture = texture.unwrap_or_else(|| {
            let image = image::load_from_memory(include_bytes!("../assets/vice-header.png"))
                .expect("embedded Vice header PNG")
                .to_rgba8();
            let pixels = egui::ColorImage::from_rgba_unmultiplied(
                [image.width() as usize, image.height() as usize],
                image.as_raw(),
            );
            let texture =
                ui.ctx()
                    .load_texture("vice_header", pixels, egui::TextureOptions::LINEAR);
            ui.ctx()
                .data_mut(|data| data.insert_temp(id, texture.clone()));
            texture
        });
        // Cover the wider header without distorting the supplied illustration.
        let visible = (art_rect.height() * texture.aspect_ratio() / art_rect.width()).min(1.0);
        let uv = egui::Rect::from_min_max(
            egui::pos2(0.0, (1.0 - visible) * 0.5),
            egui::pos2(1.0, (1.0 + visible) * 0.5),
        );
        egui::Image::new((texture.id(), texture.size_vec2()))
            .uv(uv)
            .tint(Color32::from_white_alpha(180))
            .corner_radius(CornerRadius {
                nw: 11,
                ne: 11,
                sw: 0,
                se: 0,
            })
            .paint_at(ui, art_rect);
    }
    ui.painter().text(
        rect.left_top() + egui::vec2(14.0, 10.0),
        Align2::LEFT_TOP,
        "TackleCast VI",
        FontId::proportional(23.0),
        p.text_primary,
    );
    ui.painter().text(
        rect.left_top() + egui::vec2(15.0, 42.0),
        Align2::LEFT_TOP,
        if theme == Theme::Vice {
            "VICE EDITION  /  SETTINGS"
        } else {
            "CLASSIC  /  SETTINGS"
        },
        FontId::proportional(10.0),
        p.text_secondary,
    );
}

fn overlay_corner_combo(ui: &mut egui::Ui, corner: &mut crate::overlay::Corner) {
    use crate::overlay::Corner;
    let options = [
        (Corner::TopLeft, "Top left"),
        (Corner::TopRight, "Top right"),
        (Corner::BottomLeft, "Bottom left"),
        (Corner::BottomRight, "Bottom right"),
    ];
    ui.horizontal(|ui| {
        ui.label("Position");
        ComboBox::from_id_salt("overlay_corner")
            .selected_text(options.iter().find(|(value, _)| value == corner).unwrap().1)
            .show_ui(ui, |ui| {
                for (value, label) in options {
                    ui.selectable_value(corner, value, label);
                }
            });
    });
}

fn overlay_editor(ui: &mut egui::Ui, layout: &mut crate::overlay::OverlayLayout) {
    use crate::overlay::Stat;
    layout.normalize();
    ui.label(RichText::new("Drag the grip to reorder, or use Up / Down. All off hides the pill; recovery messages stay visible.").small().color(Palette::current(ui.ctx()).text_secondary));
    let mut movement = None;
    let count = layout.stats.len();
    for (index, choice) in layout.stats.iter_mut().enumerate() {
        let stat = choice.stat;
        ui.push_id(stat, |ui| {
            let (_, dropped) = ui.dnd_drop_zone::<Stat, _>(Frame::new().inner_margin(3), |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    // Restrict drag hit-testing to the handle. A drag source
                    // over the completed row would mask checkbox/button clicks
                    // in egui 0.31 (the last drag-only widget wins hit-testing).
                    ui.dnd_drag_source(egui::Id::new(("overlay_stat_drag", stat)), stat, |ui| {
                        ui.label(RichText::new("::").color(Palette::current(ui.ctx()).text_muted))
                            .on_hover_text("Drag to reorder this statistic");
                    });
                    ui.checkbox(&mut choice.enabled, stat.label());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(index + 1 < count, Button::new("Down").small())
                            .on_hover_text("Move down")
                            .clicked()
                        {
                            movement = Some((stat, index + 1));
                        }
                        if ui
                            .add_enabled(index > 0, Button::new("Up").small())
                            .on_hover_text("Move up")
                            .clicked()
                        {
                            movement = Some((stat, index.saturating_sub(1)));
                        }
                    });
                });
            });
            if let Some(payload) = dropped {
                movement = Some((*payload, index));
            }
        });
    }
    if let Some((stat, to)) = movement {
        if let Some(from) = layout.stats.iter().position(|row| row.stat == stat) {
            layout.move_row(from, to);
        }
    }
}

fn tab_bar(ui: &mut egui::Ui, current_tab: &mut MenuTab, text_scale: f32) {
    let p = Palette::current(ui.ctx());
    let tabs = [
        (MenuTab::Video, "Video"),
        (MenuTab::Picture, "Picture"),
        (MenuTab::Audio, "Audio"),
        (MenuTab::Display, "Display"),
    ];

    Frame::new()
        .fill(p.tab_bar_bg)
        .stroke(Stroke::new(1.0_f32, p.border_subtle))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            let available_w = ui.available_width();
            let count = tabs.len() as f32;
            let tab_w = ((available_w - ((count - 1.0) * 4.0)) / count).floor();
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);

            ui.horizontal(|ui| {
                for (tab, label) in tabs {
                    let is_active = *current_tab == tab;
                    let (bg, text_color, stroke) = if is_active {
                        (
                            p.tab_active_bg,
                            p.text_primary,
                            Stroke::new(1.0_f32, p.accent),
                        )
                    } else {
                        (Color32::TRANSPARENT, p.text_muted, Stroke::NONE)
                    };

                    let btn = Button::new(
                        RichText::new(label)
                            .size(13.0 * text_scale)
                            .strong()
                            .color(text_color),
                    )
                    .fill(bg)
                    .stroke(stroke)
                    .corner_radius(CornerRadius::same(6))
                    .min_size(egui::vec2(tab_w, 28.0 * text_scale));

                    if ui.add(btn).clicked() {
                        *current_tab = tab;
                    }
                }
            });
        });
}

fn card_section<R>(
    ui: &mut egui::Ui,
    title: &str,
    text_scale: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let p = Palette::current(ui.ctx());
    ui.add_space(3.0);
    ui.label(
        RichText::new(title)
            .size(11.0 * text_scale)
            .strong()
            .color(p.text_muted),
    );
    ui.add_space(1.0);
    Frame::new()
        .fill(p.card_bg)
        .stroke(Stroke::new(1.0_f32, p.border_subtle))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(8.0 * text_scale, 8.0 * text_scale);
            ui.vertical(add_contents).inner
        })
        .inner
}

fn warning_text(ui: &mut egui::Ui, text: &str, text_scale: f32) {
    let p = Palette::current(ui.ctx());
    ui.label(RichText::new(text).size(12.0 * text_scale).color(p.warning));
}

fn labeled_combo_string(
    ui: &mut egui::Ui,
    label: &str,
    selected: &mut String,
    options: Vec<String>,
) {
    let p = Palette::current(ui.ctx());
    ui.label(RichText::new(label).color(p.text_secondary));
    ComboBox::from_id_salt(label)
        .width(ui.available_width())
        .selected_text(display_or_default(&crate::devices::video_device_label(
            selected,
        )))
        .show_ui(ui, |ui| {
            for option in options {
                ui.selectable_value(
                    selected,
                    option.clone(),
                    crate::devices::video_device_label(&option),
                );
            }
        });
}

fn labeled_combo_static(ui: &mut egui::Ui, label: &str, selected: &mut String, options: &[&str]) {
    let p = Palette::current(ui.ctx());
    ui.label(RichText::new(label).color(p.text_secondary));
    ComboBox::from_id_salt(label)
        .width(ui.available_width())
        .selected_text(selected.clone())
        .show_ui(ui, |ui| {
            for option in options {
                ui.selectable_value(selected, (*option).to_string(), *option);
            }
        });
}

fn video_format_combo(
    ui: &mut egui::Ui,
    label: &str,
    selected: &mut VideoFormat,
    available: &[VideoFormat],
    active: Option<&str>,
    live: bool,
) {
    let p = Palette::current(ui.ctx());
    ui.label(RichText::new(label).color(p.text_secondary));
    let text = if *selected == VideoFormat::Auto {
        if !live {
            "Auto (pending apply)".into()
        } else {
            format!(
                "Auto ({})",
                active
                    .map(|v| v.split(" (").next().unwrap_or(v))
                    .unwrap_or("connecting…")
            )
        }
    } else {
        selected.to_string()
    };
    ComboBox::from_id_salt(label)
        .width(ui.available_width())
        .selected_text(text)
        .show_ui(ui, |ui| {
            for format in available {
                ui.selectable_value(selected, *format, format.to_string());
            }
        });
}

fn scaling_filter_combo(ui: &mut egui::Ui, label: &str, selected: &mut ScaleFilter) {
    let p = Palette::current(ui.ctx());
    ui.label(RichText::new(label).color(p.text_secondary));
    ComboBox::from_id_salt(label)
        .width(ui.available_width())
        .selected_text(selected.to_string())
        .show_ui(ui, |ui| {
            for filter in ScaleFilter::ALL {
                ui.selectable_value(selected, filter, filter.to_string());
            }
        });
}

fn color_space_combo(
    ui: &mut egui::Ui,
    label: &str,
    selected: &mut ColorSpace,
    info: ColorInfo,
    height: u32,
) {
    let p = Palette::current(ui.ctx());
    ui.label(RichText::new(label).color(p.text_secondary));
    ComboBox::from_id_salt(label)
        .width(ui.available_width())
        .selected_text(info.space_label(*selected, height))
        .show_ui(ui, |ui| {
            for space in ColorSpace::ALL {
                ui.selectable_value(selected, space, info.space_label(space, height));
            }
        });
}

fn color_range_combo(ui: &mut egui::Ui, label: &str, selected: &mut ColorRange, info: ColorInfo) {
    let p = Palette::current(ui.ctx());
    ui.label(RichText::new(label).color(p.text_secondary));
    ComboBox::from_id_salt(label)
        .width(ui.available_width())
        .selected_text(info.range_label(*selected))
        .show_ui(ui, |ui| {
            for range in ColorRange::ALL {
                ui.selectable_value(selected, range, info.range_label(range));
            }
        });
}

fn fps_mode_combo(ui: &mut egui::Ui, selected: &mut String, rates: &[FrameRate]) {
    let p = Palette::current(ui.ctx());
    ui.label(RichText::new("Frame Rate").color(p.text_secondary));
    let text = selected
        .parse::<FrameRate>()
        .map(|rate| format!("{rate} FPS"))
        .unwrap_or_else(|_| "Custom request".into());
    ComboBox::from_id_salt("fps_mode")
        .width(ui.available_width())
        .selected_text(text)
        .show_ui(ui, |ui| {
            for rate in rates {
                ui.selectable_value(selected, rate.key(), format!("{rate} FPS"));
            }
        });
}

fn labeled_audio_combo(
    ui: &mut egui::Ui,
    label: &str,
    selected: &mut i32,
    devices: &[AudioDevice],
    dshow_option: Option<&str>,
) {
    let p = Palette::current(ui.ctx());
    ui.label(RichText::new(label).color(p.text_secondary));
    ComboBox::from_id_salt(label)
        .width(ui.available_width())
        .selected_text(audio_device_name(*selected, devices, dshow_option))
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, -1, "Default");
            if let Some(dshow_label) = dshow_option {
                ui.selectable_value(
                    selected,
                    crate::devices::DIRECTSHOW_AUDIO_INDEX,
                    dshow_label,
                );
            }
            for device in devices {
                ui.selectable_value(selected, device.index, device.name.clone());
            }
        });
}

fn labeled_volume(ui: &mut egui::Ui, draft: &mut Settings, _text_scale: f32) -> bool {
    let mut volume = (draft.volume * 100.0) as f32;
    let response = ui.add_enabled(
        !draft.audio_muted,
        Slider::new(&mut volume, 0.0..=100.0)
            .text("Volume")
            .suffix("%"),
    );
    if response.changed() {
        draft.volume = volume as f64 / 100.0;
    }
    response.dragged() || response.has_focus() || response.changed()
}

fn picture_slider(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f64,
    text_scale: f32,
) -> bool {
    let p = Palette::current(ui.ctx());
    let available_width = ui.available_width();
    let label_width = (86.0 * text_scale).min(available_width * 0.28);
    let value_width = 64.0 * text_scale;
    let slider_width =
        (available_width - label_width - value_width - 2.0 * ui.spacing().item_spacing.x).max(24.0);
    let row_height = 26.0 * text_scale;
    ui.push_id(label, |ui| {
        ui.horizontal(|ui| {
            let label_response = ui.add_sized(
                [label_width, row_height],
                egui::Label::new(
                    RichText::new(label)
                        .size(12.0 * text_scale)
                        .color(p.text_secondary),
                )
                .halign(Align::Min),
            );
            ui.spacing_mut().slider_width = slider_width;
            let slider = ui
                .add_sized(
                    [slider_width, row_height],
                    Slider::new(&mut *value, range.clone())
                        .step_by(step)
                        .show_value(false),
                )
                .labelled_by(label_response.id);
            let number = ui
                .add_sized(
                    [value_width, row_height],
                    egui::DragValue::new(value)
                        .range(range)
                        .speed(step)
                        .fixed_decimals(2),
                )
                .labelled_by(label_response.id);
            slider.dragged()
                || slider.has_focus()
                || slider.changed()
                || number.dragged()
                || number.has_focus()
                || number.changed()
        })
        .inner
    })
    .inner
}

fn styled_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let p = Palette::current(ui.ctx());
    ui.add(
        Button::new(RichText::new(text).color(p.text_primary).strong())
            .fill(p.panel_bg)
            .stroke(Stroke::new(1.0_f32, p.border))
            .corner_radius(CornerRadius::same(6))
            .min_size(egui::vec2(0.0, 28.0)),
    )
}

fn exit_button(ui: &mut egui::Ui) -> egui::Response {
    let p = Palette::current(ui.ctx());
    ui.with_layout(Layout::top_down(Align::Center), |ui| {
        ui.add(
            Button::new(RichText::new("Exit TackleCast").color(p.exit_text).strong())
                .fill(p.exit_bg)
                .stroke(Stroke::new(1.0_f32, p.exit_border))
                .corner_radius(CornerRadius::same(6))
                .min_size(egui::vec2(ui.available_width(), 32.0)),
        )
    })
    .inner
}

fn video_device_options(video_devices: &[String], current: &str) -> Vec<String> {
    let mut options = video_devices.to_vec();
    if !current.is_empty() && !options.iter().any(|device| device == current) {
        options.push(current.to_string());
    }
    if options.is_empty() {
        options.push(String::new());
    }
    options
}

fn display_or_default(value: &str) -> String {
    if value.is_empty() {
        "No Devices Found".to_string()
    } else {
        value.to_string()
    }
}

fn audio_device_name(index: i32, devices: &[AudioDevice], dshow_option: Option<&str>) -> String {
    if index == crate::devices::DIRECTSHOW_AUDIO_INDEX {
        return dshow_option
            .unwrap_or("DirectShow: Capture Device")
            .to_string();
    }
    if index < 0 {
        return "Default".to_string();
    }

    devices
        .iter()
        .find(|device| device.index == index)
        .map(|device| device.name.clone())
        .unwrap_or_else(|| format!("Saved Device #{index}"))
}

fn overlay_text(overlay: &OverlayInfo) -> Option<String> {
    if overlay.status_is_alert || overlay.status_message.is_some() {
        return None;
    }

    if !overlay.show_overlay {
        return None;
    }

    use crate::overlay::Stat;
    let height = overlay.height.unwrap_or(1080);
    let mut cells = Vec::new();
    for row in overlay.layout.stats.iter().filter(|row| row.enabled) {
        let value = match row.stat {
            Stat::Resolution => match (overlay.width, overlay.height) {
                (Some(w), Some(h)) => format!("{w}×{h}"),
                _ => "—".into(),
            },
            Stat::DeliveredFps => overlay
                .fps
                .filter(|fps| fps.is_finite())
                .map(|fps| format!("{fps:.1}"))
                .unwrap_or_else(|| "—".into()),
            Stat::NominalFps => overlay
                .nominal_rate
                .map(|rate| format!("{rate} ({})", rate.key()))
                .unwrap_or_else(|| "unknown".into()),
            Stat::PixelFormat => overlay
                .active_format
                .clone()
                .unwrap_or_else(|| "unknown".into()),
            Stat::ScalingFilter => {
                if overlay.filter_label.is_empty() {
                    overlay.filter.to_string()
                } else {
                    overlay.filter_label.clone()
                }
            }
            Stat::ColorSpace => overlay.color_info.space_label(overlay.color_space, height),
            Stat::ColorRange => overlay.color_info.range_label(overlay.color_range),
        };
        let label = match row.stat {
            Stat::Resolution => "Video",
            Stat::DeliveredFps => "FPS",
            Stat::NominalFps => "Target",
            Stat::PixelFormat => "Format",
            Stat::ScalingFilter => "Filter",
            Stat::ColorSpace => "Color",
            Stat::ColorRange => "Range",
        };
        cells.push(format!("{label}  {value}"));
    }
    if cells.is_empty() {
        return None;
    }
    // Two values per line: a compact primary pair, followed by secondary data.
    Some(
        cells
            .chunks(2)
            .map(|row| row.join("  ·  "))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vice_header_art_reaches_panel_edges_while_title_stays_padded() {
        let ctx = egui::Context::default();
        let mut panel = egui::Rect::NOTHING;
        let mut content = egui::Rect::NOTHING;
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            configure_style(ctx, Theme::Vice);
            let p = Palette::current(ctx);
            egui::CentralPanel::default().show(ctx, |ui| {
                panel = Frame::new()
                    .fill(p.menu_bg)
                    .stroke(Stroke::new(1.0_f32, p.menu_border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(18))
                    .show(ui, |ui| {
                        ui.set_width(480.0);
                        content = ui.available_rect_before_wrap();
                        edition_header(ui, Theme::Vice);
                    })
                    .response
                    .rect;
            });
        });
        let title = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.job.text == "TackleCast VI" => {
                    Some(text.pos)
                }
                _ => None,
            })
            .expect("header title");
        // Frame reserves both its stroke and padding before laying out content.
        assert_eq!(content.left() - panel.left(), 19.0);
        assert_eq!(content.top() - panel.top(), 19.0);
        assert_eq!(title, content.left_top() + egui::vec2(14.0, 10.0));
        let texture = ctx
            .data(|data| data.get_temp::<egui::TextureHandle>(egui::Id::new("vice_header_texture")))
            .unwrap();
        // Artwork must meet the border's inner edge, not just approach the
        // outer frame within a tolerance broad enough to hide a one-point seam.
        let intended = egui::Rect::from_min_max(
            panel.left_top() + egui::vec2(1.0, 1.0),
            egui::pos2(panel.right() - 1.0, content.top() + 70.0),
        );
        let (art, clip) = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect)
                    if rect
                        .brush
                        .as_ref()
                        .is_some_and(|brush| brush.fill_texture_id == texture.id()) =>
                {
                    Some((rect, shape.clip_rect))
                }
                _ => None,
            })
            .expect("header image paint shape");
        assert_eq!(art.rect, intended, "art must reach the border's inner edge");
        assert_eq!(
            art.corner_radius,
            CornerRadius {
                nw: 11,
                ne: 11,
                sw: 0,
                se: 0
            }
        );
        assert!(
            clip.contains_rect(intended),
            "clip {clip:?} cuts off intended art {intended:?}"
        );
        let jobs = ctx.tessellate(output.shapes, 1.0);
        let (art, clip) = jobs
            .iter()
            .find_map(|job| match &job.primitive {
                egui::epaint::Primitive::Mesh(mesh) if mesh.texture_id == texture.id() => {
                    Some((mesh.calc_bounds(), job.clip_rect))
                }
                _ => None,
            })
            .expect("header image mesh");
        assert!(
            art.contains_rect(intended),
            "mesh {art:?} misses intended art {intended:?}"
        );
        assert!(
            clip.contains_rect(intended),
            "mesh clip {clip:?} cuts off intended art {intended:?}"
        );
    }

    #[test]
    fn editor_controls_have_glyphs_in_bundled_fonts() {
        let ctx = egui::Context::default();
        let mut layout = crate::overlay::OverlayLayout::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| overlay_editor(ui, &mut layout));
        });
        for clipped in output.shapes {
            if let egui::Shape::Text(text) = clipped.shape {
                for section in &text.galley.job.sections {
                    let label = &text.galley.job.text[section.byte_range.clone()];
                    assert!(
                        ctx.fonts(|fonts| fonts.has_glyphs(&section.format.font_id, label)),
                        "editor label is not renderable with bundled fonts: {label:?}"
                    );
                }
            }
        }
    }

    fn sample() -> OverlayInfo {
        OverlayInfo {
            width: Some(1920),
            height: Some(1080),
            fps: Some(59.8),
            filter: ScaleFilter::Bilinear,
            filter_label: "Bilinear (RTX VSR unavailable)".into(),
            color_space: ColorSpace::Auto,
            color_range: ColorRange::Auto,
            active_format: Some("NV12".into()),
            nominal_rate: Some("60000/1001".parse().unwrap()),
            color_info: ColorInfo::default(),
            show_overlay: true,
            layout: crate::overlay::OverlayLayout::default(),
            status_message: None,
            status_is_alert: false,
        }
    }
    #[test]
    fn overlay_choices_control_order_exact_rate_and_real_fallback_label() {
        let mut overlay = sample();
        overlay.layout.stats.reverse();
        let text = overlay_text(&overlay).unwrap();
        assert!(text.starts_with("Format  NV12"));
        assert!(text.contains("60000/1001"));
        assert!(text.contains("59.8"));
        for row in &mut overlay.layout.stats {
            row.enabled = row.stat == crate::overlay::Stat::ScalingFilter;
        }
        assert_eq!(
            overlay_text(&overlay).unwrap(),
            "Filter  Bilinear (RTX VSR unavailable)"
        );
        for row in &mut overlay.layout.stats {
            row.enabled = false;
        }
        assert_eq!(overlay_text(&overlay), None);
    }

    #[test]
    fn preview_pages_select_expected_menu_without_changing_settings() {
        for (page, tab) in [
            (PreviewPage::None, None),
            (PreviewPage::Video, Some(MenuTab::Video)),
            (PreviewPage::Picture, Some(MenuTab::Picture)),
            (PreviewPage::Audio, Some(MenuTab::Audio)),
            (PreviewPage::Display, Some(MenuTab::Display)),
            (PreviewPage::Overlay, Some(MenuTab::Display)),
        ] {
            assert_eq!(page.menu_tab(), tab);
        }
    }

    #[test]
    fn overlay_preview_opens_editor_and_scrolls_all_rows_into_view() {
        let ctx = egui::Context::default();
        let mut settings = Settings::default();
        let applied = settings.clone();
        let overlay = sample();
        let frame = UiFrame {
            overlay: &overlay,
            settings: &applied,
            video_devices: &[],
            audio_inputs: &[],
            audio_outputs: &[],
            is_fullscreen: false,
            audio_status: "Not running",
        };
        let mut tab = MenuTab::Display;
        ctx.data_mut(|data| data.insert_temp(egui::Id::new("test_overlay_focus"), true));
        let mut visible = Vec::new();
        for _ in 0..12 {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 720.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    configure_style(ctx, settings.theme);
                    draw_menu(
                        ctx,
                        &mut settings,
                        &mut tab,
                        &frame,
                        &mut UiOutput::default(),
                    );
                },
            );
            visible = output
                .shapes
                .into_iter()
                .filter_map(|shape| {
                    if let egui::Shape::Text(text) = shape.shape {
                        let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
                        if shape.clip_rect.contains_rect(rect) {
                            return Some(text.galley.job.text.clone());
                        }
                    }
                    None
                })
                .collect();
        }
        for stat in &crate::overlay::OverlayLayout::default().stats {
            assert!(
                visible.iter().any(|text| text == stat.stat.label()),
                "missing {}: {visible:?}",
                stat.stat.label()
            );
        }
        assert!(!ctx
            .data(|data| data.get_temp::<bool>(egui::Id::new("test_overlay_focus")))
            .unwrap_or(false));
    }

    #[test]
    fn display_menu_exposes_theme_and_collapsible_editor_in_headless_frame() {
        let ctx = egui::Context::default();
        let mut settings = Settings::default();
        let overlay = sample();
        let frame = UiFrame {
            overlay: &overlay,
            settings: &Settings::default(),
            video_devices: &[],
            audio_inputs: &[],
            audio_outputs: &[],
            is_fullscreen: false,
            audio_status: "Not running",
        };
        let mut tab = MenuTab::Display;
        let mut texts = String::new();
        for _ in 0..3 {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(640.0, 480.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    configure_style(ctx, settings.theme);
                    draw_menu(
                        ctx,
                        &mut settings,
                        &mut tab,
                        &frame,
                        &mut UiOutput::default(),
                    );
                },
            );
            texts.clear();
            for shape in output.shapes {
                if let egui::Shape::Text(text) = shape.shape {
                    texts.push_str(&text.galley.job.text);
                    texts.push('\n');
                }
            }
        }
        assert!(texts.contains("TackleCast VI"), "{texts}");
        assert!(texts.contains("Theme"), "{texts}");
        assert!(texts.contains("Customize statistics"), "{texts}");
    }

    #[test]
    fn pill_respects_all_corners_and_small_screen_bounds() {
        use crate::overlay::Corner;
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(320.0, 360.0));
        for corner in [
            Corner::TopLeft,
            Corner::TopRight,
            Corner::BottomLeft,
            Corner::BottomRight,
        ] {
            let ctx = egui::Context::default();
            let mut overlay = sample();
            overlay.layout.corner = corner;
            for row in &mut overlay.layout.stats {
                row.enabled = true;
            }
            for _ in 0..3 {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |ctx| draw_overlay(ctx, &overlay),
                );
            }
            let rect = ctx
                .memory(|memory| memory.area_rect(egui::Id::new("fps_overlay")))
                .unwrap();
            assert!(screen.contains_rect(rect), "{corner:?}: {rect:?}");
            match corner {
                Corner::TopLeft => assert!(rect.left() < 20.0 && rect.top() < 20.0),
                Corner::TopRight => assert!(rect.right() > 300.0 && rect.top() < 20.0),
                Corner::BottomLeft => assert!(rect.left() < 20.0 && rect.bottom() > 340.0),
                Corner::BottomRight => assert!(rect.right() > 300.0 && rect.bottom() > 340.0),
            }
        }
    }

    fn editor_frame(
        ctx: &egui::Context,
        layout: &mut crate::overlay::OverlayLayout,
        events: Vec<egui::Event>,
    ) -> Vec<(String, egui::Pos2)> {
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(420.0, 500.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                configure_style(ctx, Theme::Vice);
                egui::CentralPanel::default().show(ctx, |ui| overlay_editor(ui, layout));
            },
        );
        output
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.job.text.clone(),
                    text.pos + text.galley.size() * 0.5,
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn editor_checkbox_arrows_and_whole_row_drag_preserve_choices() {
        use crate::overlay::{OverlayLayout, Stat};
        let ctx = egui::Context::default();
        let mut layout = OverlayLayout::default();
        let mut texts = editor_frame(&ctx, &mut layout, vec![]);
        let point = texts
            .iter()
            .find(|(text, _)| text == "Resolution")
            .unwrap()
            .1;
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        editor_frame(
            &ctx,
            &mut layout,
            vec![egui::Event::PointerMoved(point), button(point, true)],
        );
        texts = editor_frame(&ctx, &mut layout, vec![button(point, false)]);
        assert!(
            !layout.stats[0].enabled,
            "checkbox click must not start a row drag"
        );
        let down = texts.iter().find(|(text, _)| text == "Down").unwrap().1;
        editor_frame(
            &ctx,
            &mut layout,
            vec![egui::Event::PointerMoved(down), button(down, true)],
        );
        editor_frame(&ctx, &mut layout, vec![button(down, false)]);
        assert_eq!(layout.stats[1].stat, Stat::Resolution);
        assert!(!layout.stats[1].enabled);
        texts = editor_frame(&ctx, &mut layout, vec![]);
        let handles: Vec<_> = texts
            .iter()
            .filter(|(text, _)| text == "::")
            .map(|(_, pos)| *pos)
            .collect();
        assert_eq!(handles.len(), 7);
        let (from, to) = (handles[0], handles[6]);
        editor_frame(
            &ctx,
            &mut layout,
            vec![egui::Event::PointerMoved(from), button(from, true)],
        );
        editor_frame(&ctx, &mut layout, vec![egui::Event::PointerMoved(to)]);
        editor_frame(&ctx, &mut layout, vec![]);
        editor_frame(&ctx, &mut layout, vec![button(to, false)]);
        assert_eq!(layout.stats[6].stat, Stat::DeliveredFps);
        assert!(layout.stats[6].enabled);
        assert!(
            !layout
                .stats
                .iter()
                .find(|row| row.stat == Stat::Resolution)
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn menus_keep_stable_bounds_across_tabs_themes_and_small_windows() {
        for size in [
            egui::vec2(320.0, 360.0),
            egui::vec2(640.0, 480.0),
            egui::vec2(1280.0, 720.0),
        ] {
            for theme in [Theme::Vice, Theme::Classic] {
                let ctx = egui::Context::default();
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                let mut settings = Settings {
                    theme,
                    ..Settings::default()
                };
                let overlay = sample();
                let applied = settings.clone();
                let frame = UiFrame {
                    overlay: &overlay,
                    settings: &applied,
                    video_devices: &[],
                    audio_inputs: &[],
                    audio_outputs: &[],
                    is_fullscreen: false,
                    audio_status: "Not running",
                };
                let mut previous_size = None;
                for mut tab in [
                    MenuTab::Video,
                    MenuTab::Picture,
                    MenuTab::Audio,
                    MenuTab::Display,
                ] {
                    for _ in 0..3 {
                        let _ = ctx.run(
                            egui::RawInput {
                                screen_rect: Some(screen),
                                ..Default::default()
                            },
                            |ctx| {
                                configure_style(ctx, theme);
                                draw_menu(
                                    ctx,
                                    &mut settings,
                                    &mut tab,
                                    &frame,
                                    &mut UiOutput::default(),
                                );
                            },
                        );
                    }
                    let rect = ctx
                        .memory(|memory| memory.area_rect(egui::Id::new("settings_menu")))
                        .unwrap();
                    assert!(
                        screen.contains_rect(rect),
                        "{theme:?} {tab:?} {size:?}: {rect:?}"
                    );
                    if let Some(previous) = previous_size {
                        assert_eq!(rect.size(), previous, "{theme:?} {tab:?}");
                    }
                    previous_size = Some(rect.size());
                }
            }
        }
    }

    #[test]
    fn embedded_theme_icons_decode_to_winit_icons() {
        assert!(theme_icon(Theme::Vice).is_ok());
        assert!(theme_icon(Theme::Classic).is_ok());
    }

    #[test]
    fn recovery_status_renders_when_master_and_all_statistics_are_off() {
        let ctx = egui::Context::default();
        let mut overlay = sample();
        overlay.show_overlay = false;
        overlay
            .layout
            .stats
            .iter_mut()
            .for_each(|row| row.enabled = false);
        overlay.status_message = Some("Reconnecting capture…".into());
        overlay.status_is_alert = true;
        let mut text = String::new();
        for _ in 0..3 {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(320.0, 360.0),
                    )),
                    ..Default::default()
                },
                |ctx| draw_overlay(ctx, &overlay),
            );
            text = output
                .shapes
                .into_iter()
                .filter_map(|shape| match shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.job.text.clone()),
                    _ => None,
                })
                .collect();
        }
        assert!(text.contains("Reconnecting capture…"));
        assert!(!text.contains("59.8"));
        assert!(ctx
            .memory(|memory| memory.area_rect(egui::Id::new("capture_status")))
            .is_some());
        assert!(ctx
            .memory(|memory| memory.area_rect(egui::Id::new("fps_overlay")))
            .is_none());
    }

    #[test]
    fn stats_never_replace_status_or_show_stale_fps_during_failure() {
        let mut overlay = sample();
        overlay.status_message = Some("Reconnecting capture…".into());
        overlay.status_is_alert = true;
        assert_eq!(overlay_text(&overlay), None);
        overlay.show_overlay = false;
        assert_eq!(overlay_text(&overlay), None);
        assert_eq!(
            overlay.status_message.as_deref(),
            Some("Reconnecting capture…")
        );
    }
}
