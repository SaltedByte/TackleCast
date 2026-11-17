use crate::capabilities;
use crate::color::ColorInfo;
use crate::devices::AudioDevice;
use crate::frame_rate::FrameRate;
use crate::settings::{ColorRange, ColorSpace, ScaleFilter, Settings, Theme, VideoFormat};
use crate::theme::Palette;
use egui::{Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, Stroke};
use egui_components as sc;
use egui_extras::{Size, StripBuilder};
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
    #[cfg(all(test, target_os = "windows"))]
    last_texts: Vec<(String, egui::Pos2)>,
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
    pub devices_busy: bool,
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
    pub reinitialize_devices: Option<Settings>,
    /// Live picture and playback controls; device routes still apply on close/reinitialize.
    pub live_settings: Option<Settings>,
    pub video_adjustment_active: bool,
    pub toggle_fullscreen: bool,
    pub exit_requested: bool,
    pub repaint_after: Duration,
}

impl UiState {
    pub fn refresh_device_settings(&mut self, settings: &Settings) {
        self.draft_settings = settings.clone();
    }
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
            #[cfg(all(test, target_os = "windows"))]
            last_texts: Vec::new(),
        }
    }

    pub fn on_window_event(
        &mut self,
        window: &Window,
        event: &WindowEvent,
        request_redraw: impl FnOnce(),
    ) -> bool {
        let response = self.egui_winit.on_window_event(window, event);
        // Drawing and input ownership are independent: cursor movement often
        // requests a repaint while egui is not consuming the pointer.
        if response.repaint {
            request_redraw();
        }
        response.consumed
    }

    pub fn toggle_menu(
        &mut self,
        settings: &Settings,
        request_redraw: impl FnOnce(),
    ) -> Option<Settings> {
        let applied = if self.menu_open {
            self.close_menu_and_apply()
        } else {
            self.open_menu(settings);
            None
        };
        request_redraw();
        applied
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
        let full_output = self.egui_ctx.run_ui(raw_input, |root_ui| {
            let ctx = root_ui.ctx();
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
            ui_output.live_settings = Some(self.draft_settings.clone());
        }
        ui_output.repaint_after = full_output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.repaint_delay)
            .unwrap_or(Duration::MAX);

        #[cfg(all(test, target_os = "windows"))]
        {
            self.last_texts = full_output
                .shapes
                .iter()
                .filter_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape {
                        let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
                        return shape
                            .clip_rect
                            .intersects(rect)
                            .then(|| (text.galley.job.text.clone(), rect.center()));
                    }
                    None
                })
                .collect();
        }

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
    let theme_changed =
        ctx.data(|data| data.get_temp::<Theme>(egui::Id::new("edition_theme"))) != Some(theme);
    ctx.data_mut(|data| data.insert_temp(egui::Id::new("edition_theme"), theme));
    let p = Palette::for_theme(theme);
    if theme_changed {
        let mut components = egui_components::theme::Theme::dark();
        components.colors.background = p.panel_bg;
        components.colors.foreground = p.text_primary;
        components.colors.border = p.border;
        components.colors.ring = p.accent;
        components.colors.primary_background = p.accent;
        components.colors.primary_foreground = p.menu_bg;
        components.colors.primary_hover_background = p.accent_hover;
        components.colors.primary_active_background = p.accent;
        components.colors.secondary_background = p.panel_bg;
        components.colors.secondary_foreground = p.text_primary;
        components.colors.accent_background = p.panel_hover;
        components.colors.accent_foreground = p.text_primary;
        components.colors.muted_background = p.panel_bg;
        components.colors.muted_foreground = p.text_muted;
        components.colors.input_border = p.border;
        components.colors.popover_background = p.menu_bg.to_opaque();
        components.colors.popover_foreground = p.text_primary;
        components.colors.slider_bar_background = p.accent;
        components.colors.slider_thumb_background = p.text_primary;
        components.colors.switch_background = p.panel_bg;
        components.colors.danger_background = p.exit_bg;
        components.colors.danger_foreground = p.exit_text;
        components.install(ctx);
    }
    // Keep the app's branding when the desktop reports a light system theme.
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
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
    style.visuals.widgets.inactive.weak_bg_fill = p.panel_bg;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    style.visuals.widgets.inactive.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.inactive.fg_stroke.color = p.text_primary;
    style.visuals.widgets.hovered.bg_fill = p.panel_hover;
    style.visuals.widgets.hovered.weak_bg_fill = p.panel_hover;
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent_hover);
    style.visuals.widgets.hovered.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.hovered.fg_stroke.color = p.text_primary;
    style.visuals.widgets.active.bg_fill = p.panel_hover;
    style.visuals.widgets.active.weak_bg_fill = p.panel_hover;
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
    style.visuals.widgets.active.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.active.fg_stroke.color = p.text_primary;
    style.visuals.selection.bg_fill = p.accent;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, p.accent);
    style.visuals.slider_trailing_fill = true;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    ctx.set_style_of(egui::Theme::Dark, style);
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
    let max_width = (ctx.content_rect().width() - 48.0).clamp(64.0, 560.0);
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
    #[cfg(test)]
    ctx.data_mut(|data| data.remove::<Vec<WidgetGeometry>>(egui::Id::new("settings_geometry")));
    let p = Palette::current(ctx);
    let screen = ctx.content_rect();
    let scale = (screen.width() / 1280.0).clamp(0.85, 1.3);
    let menu_width = (680.0_f32 * scale).min((screen.width() - 56.0).max(180.0));
    let text_scale = scale.clamp(0.9, 1.15);
    // Reserve the same space for every tab. Shorter pages keep their empty
    // space, while small windows scroll instead of growing the outer menu.
    let chrome_height = if menu_width < 340.0 { 310.0 } else { 260.0 };
    egui::Area::new("settings_menu".into())
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .order(egui::Order::Foreground)
        .movable(false)
        .show(ctx, |ui| {
            Frame::new()
                .fill(p.menu_bg.to_opaque())
                .stroke(Stroke::new(1.0_f32, p.menu_border))
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::same(18))
                .show(ui, |ui| {
                    ui.set_width(menu_width);
                    ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                    ui.spacing_mut().scroll = egui::style::ScrollStyle {
                        bar_inner_margin: 12.0,
                        ..egui::style::ScrollStyle::solid()
                    };
                    ui.vertical(|ui| {
                        let header_height = edition_header(ui, draft.theme);
                        let content_height = (460.0 * text_scale).min(
                            (screen.height() - chrome_height - (header_height - 70.0).max(0.0))
                                .max(40.0),
                        );
                        tab_bar(ui, tab, text_scale);
                        let scroll_right = ui.available_rect_before_wrap().right();
                        let scroll = egui::ScrollArea::vertical()
                            .id_salt("settings_content")
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysVisible,
                            )
                            .max_height(content_height)
                            .min_scrolled_height(content_height)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if frame.devices_busy {
                                    ui.disable();
                                }
                                match *tab {
                                    MenuTab::Video => {
                                        settings_section(ui, "CAPTURE SOURCE", text_scale, |ui| {
                                            let selection = crate::devices::select_capture_devices(
                                                &draft.video_device,
                                                draft.audio_input,
                                                frame.video_devices,
                                                frame.audio_inputs,
                                            );
                                            let automatic = if selection.video_device.is_empty() {
                                                "Automatic (match capture audio)".into()
                                            } else {
                                                format!(
                                                    "Automatic · {}",
                                                    crate::devices::video_device_label(
                                                        &selection.video_device
                                                    )
                                                )
                                            };
                                            let old = draft.video_device.clone();
                                            labeled_combo_string(
                                                ui,
                                                "Device",
                                                &mut draft.video_device,
                                                video_device_options(frame.video_devices, &old),
                                                &automatic,
                                            );
                                            let selection = crate::devices::select_capture_devices(
                                                &draft.video_device,
                                                draft.audio_input,
                                                frame.video_devices,
                                                frame.audio_inputs,
                                            );
                                            if let Some(message) = selection.message() {
                                                warning_text(ui, message, text_scale);
                                            }
                                            let caps = capabilities::get_device_capabilities(
                                                &selection.video_device,
                                            );
                                            if let Some(message) = &caps.message {
                                                warning_text(ui, message, text_scale);
                                                if message.starts_with("Reading") {
                                                    ctx.request_repaint_after(
                                                        Duration::from_millis(100),
                                                    );
                                                }
                                            }
                                            if let Some(message) = &caps.usb_warning {
                                                warning_text(ui, message, text_scale);
                                            }
                                            capabilities::sanitize_draft_settings(draft, &caps);
                                            let resolutions =
                                                capabilities::supported_resolutions(&caps);
                                            let refs: Vec<_> =
                                                resolutions.iter().map(String::as_str).collect();
                                            ui.add_enabled_ui(!caps.modes.is_empty(), |ui| {
                                                labeled_combo_static(
                                                    ui,
                                                    "Resolution",
                                                    &mut draft.resolution,
                                                    &refs,
                                                );
                                                let rates = capabilities::supported_rates(
                                                    &caps,
                                                    &draft.resolution,
                                                    VideoFormat::Auto,
                                                );
                                                fps_mode_combo(ui, &mut draft.fps_mode, &rates);
                                            });
                                            capabilities::sanitize_draft_settings(draft, &caps);
                                        });
                                        settings_section(
                                            ui,
                                            "FORMAT & SCALING",
                                            text_scale,
                                            |ui| {
                                                let selection =
                                                    crate::devices::select_capture_devices(
                                                        &draft.video_device,
                                                        draft.audio_input,
                                                        frame.video_devices,
                                                        frame.audio_inputs,
                                                    );
                                                let caps = capabilities::get_device_capabilities(
                                                    &selection.video_device,
                                                );
                                                let formats = capabilities::supported_video_formats(
                                                    &caps,
                                                    &draft.resolution,
                                                    draft.get_fps(),
                                                );
                                                let live = draft.video_device
                                                    == frame.settings.video_device
                                                    && draft.resolution
                                                        == frame.settings.resolution
                                                    && draft
                                                        .get_fps()
                                                        .equivalent(frame.settings.get_fps());
                                                let active = if live {
                                                    frame.overlay.active_format.as_deref()
                                                } else {
                                                    None
                                                };
                                                video_format_combo(
                                                    ui,
                                                    "Pixel Format",
                                                    &mut draft.video_format,
                                                    &formats,
                                                    active,
                                                    live,
                                                );
                                                scaling_filter_combo(
                                                    ui,
                                                    "Upscale Filter",
                                                    &mut draft.scaling_filter,
                                                );
                                                if matches!(
                                                    draft.scaling_filter,
                                                    ScaleFilter::Fsr1 | ScaleFilter::RtxVsr
                                                ) {
                                                    output.video_adjustment_active |= picture_slider(
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
                                            },
                                        );
                                    }
                                    MenuTab::Picture => {
                                        settings_section(
                                            ui,
                                            "IMAGE ADJUSTMENTS",
                                            text_scale,
                                            |ui| {
                                                output.video_adjustment_active |= picture_slider(
                                                    ui,
                                                    "Brightness",
                                                    &mut draft.brightness,
                                                    -1.0..=1.0,
                                                    0.02,
                                                    text_scale,
                                                );
                                                output.video_adjustment_active |= picture_slider(
                                                    ui,
                                                    "Contrast",
                                                    &mut draft.contrast,
                                                    0.0..=2.0,
                                                    0.02,
                                                    text_scale,
                                                );
                                                output.video_adjustment_active |= picture_slider(
                                                    ui,
                                                    "Saturation",
                                                    &mut draft.saturation,
                                                    0.0..=2.0,
                                                    0.02,
                                                    text_scale,
                                                );
                                                output.video_adjustment_active |= picture_slider(
                                                    ui,
                                                    "Gamma",
                                                    &mut draft.gamma,
                                                    0.2..=3.0,
                                                    0.05,
                                                    text_scale,
                                                );
                                            },
                                        );
                                        settings_row(ui, "Reset picture", |ui| {
                                            if styled_button(ui, "Reset to defaults").clicked() {
                                                draft.brightness = 0.0;
                                                draft.contrast = 1.0;
                                                draft.saturation = 1.0;
                                                draft.gamma = 1.0;
                                                output.video_adjustment_active = true;
                                            }
                                        });
                                    }
                                    MenuTab::Audio => {
                                        settings_section(ui, "DEVICE ROUTING", text_scale, |ui| {
                                            let selection = crate::devices::select_capture_devices(
                                                &draft.video_device,
                                                draft.audio_input,
                                                frame.video_devices,
                                                frame.audio_inputs,
                                            );
                                            let dshow_option = if cfg!(target_os = "windows")
                                                && !selection.video_device.is_empty()
                                            {
                                                Some(format!(
                                                    "DirectShow: {}",
                                                    crate::devices::video_device_label(
                                                        &selection.video_device
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
                                                draft.audio_input_name.as_deref(),
                                                &selection
                                                    .audio_input
                                                    .and_then(|index| {
                                                        frame
                                                            .audio_inputs
                                                            .iter()
                                                            .find(|input| input.index == index)
                                                    })
                                                    .map(|input| {
                                                        format!("Automatic · {}", input.name)
                                                    })
                                                    .unwrap_or_else(|| {
                                                        "Automatic (match video)".into()
                                                    }),
                                            );
                                            labeled_audio_combo(
                                                ui,
                                                "Output Device",
                                                &mut draft.audio_output,
                                                frame.audio_outputs,
                                                None,
                                                draft.audio_output_name.as_deref(),
                                                "System default",
                                            );
                                            if let Some(message) = selection.message() {
                                                warning_text(ui, message, text_scale);
                                            }
                                        });
                                        settings_section(ui, "PLAYBACK VOLUME", text_scale, |ui| {
                                            toggle_row(ui, "Mute audio", &mut draft.audio_muted);
                                            labeled_volume(ui, draft);
                                            settings_row(ui, "Status", |ui| {
                                                ui.label(frame.audio_status);
                                            });
                                        });
                                    }
                                    MenuTab::Display => {
                                        settings_section(
                                            ui,
                                            "WINDOW & DISPLAY",
                                            text_scale,
                                            |ui| {
                                                settings_row(ui, "Theme", |ui| {
                                                    let mut selected =
                                                        usize::from(draft.theme == Theme::Classic);
                                                    let response = sc::Tabs::new(&mut selected)
                                                        .tabs(["Vice", "Classic"])
                                                        .segmented()
                                                        .show(ui);
                                                    control_geometry(ui, "control", &response);
                                                    draft.theme = if selected == 0 {
                                                        Theme::Vice
                                                    } else {
                                                        Theme::Classic
                                                    };
                                                });
                                                settings_row(ui, "Fullscreen", |ui| {
                                                    if styled_button(
                                                        ui,
                                                        if frame.is_fullscreen {
                                                            "Exit fullscreen"
                                                        } else {
                                                            "Enter fullscreen"
                                                        },
                                                    )
                                                    .clicked()
                                                    {
                                                        output.toggle_fullscreen = true;
                                                    }
                                                });
                                            },
                                        );
                                        settings_section(
                                            ui,
                                            "ON-SCREEN OVERLAY",
                                            text_scale,
                                            |ui| {
                                                toggle_row(
                                                    ui,
                                                    "Show performance pill",
                                                    &mut draft.show_overlay,
                                                );
                                                let legacy = draft.detailed_overlay;
                                                let layout =
                                                    draft.overlay.get_or_insert_with(|| {
                                                        crate::overlay::OverlayLayout::legacy(
                                                            legacy,
                                                        )
                                                    });
                                                overlay_corner_combo(ui, &mut layout.corner);
                                                let focus = ctx.data_mut(|data| {
                                                    data.get_temp::<bool>(egui::Id::new(
                                                        "test_overlay_focus",
                                                    ))
                                                    .unwrap_or(false)
                                                });
                                                let editor = egui::CollapsingHeader::new(
                                                    "Customize statistics",
                                                )
                                                .default_open(focus)
                                                .show(ui, |ui| overlay_editor(ui, layout));
                                                if focus {
                                                    if let Some(body) = &editor.body_response {
                                                        let too_tall = body.rect.height() > ui.clip_rect().height();
                                                        if editor.fully_open() && (ui.clip_rect().contains_rect(body.rect) || too_tall) {
                                                            if too_tall { body.scroll_to_me_animation(Some(Align::Min), egui::style::ScrollAnimation::none()); }
                                                            ctx.data_mut(|data| {
                                                                data.remove::<bool>(egui::Id::new(
                                                                    "test_overlay_focus",
                                                                ))
                                                            });
                                                        } else {
                                                            body.scroll_to_me_animation(Some(Align::Center), egui::style::ScrollAnimation::none());
                                                            ctx.request_repaint();
                                                        }
                                                    }
                                                }
                                            },
                                        );
                                        settings_section(ui, "SESSION", text_scale, |ui| {
                                            settings_row(ui, "Close application", |ui| {
                                                if exit_button(ui).clicked() {
                                                    output.exit_requested = true;
                                                }
                                            });
                                        });
                                    }
                                }
                            });
                        record_geometry(ui, "Settings", "viewport", scroll.inner_rect);
                        record_geometry(
                            ui,
                            "Settings",
                            "scroll_outer",
                            egui::Rect::from_min_max(
                                scroll.inner_rect.min,
                                egui::pos2(scroll_right, scroll.inner_rect.bottom()),
                            ),
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add(
                                    sc::Button::outline(if frame.devices_busy {
                                        "Reinitializing…"
                                    } else {
                                        "Reinitialize devices"
                                    })
                                    .small()
                                    .disabled(frame.devices_busy),
                                )
                                .clicked()
                            {
                                output.reinitialize_devices = Some(draft.clone());
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add(
                                        sc::Button::primary("Apply & close")
                                            .small()
                                            .disabled(frame.devices_busy),
                                    )
                                    .clicked()
                                {
                                    output.apply_settings = Some(draft.clone());
                                }
                            });
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
            if !output.video_adjustment_active {
                ui.painter().rect_filled(screen, 0.0, p.dim_overlay);
            }
            if response.clicked() {
                output.apply_settings = Some(draft.clone());
            }
        });
}

fn edition_header(ui: &mut egui::Ui, theme: Theme) -> f32 {
    let p = Palette::current(ui.ctx());
    let texture = (theme == Theme::Vice).then(|| vice_header_texture(ui.ctx()));
    // Include the full-bleed side/top padding when fitting the source aspect ratio.
    let height = texture
        .as_ref()
        .map(|image| (ui.available_width() + 36.0) / image.aspect_ratio() - 18.0)
        .unwrap_or(70.0);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    if theme == Theme::Classic {
        ui.painter().rect_filled(rect, 8.0, p.card_bg);
    }
    if let Some(texture) = texture {
        // Frame reserves the one-point stroke in addition to its 18-point
        // padding. Bleed to the border's inner edge; keep text and controls inset.
        let art_rect = egui::Rect::from_min_max(
            rect.min - egui::vec2(18.0, 18.0),
            rect.max + egui::vec2(18.0, 0.0),
        );
        egui::Image::new((texture.id(), texture.size_vec2()))
            .uv(egui::Rect::from_min_max(
                egui::Pos2::ZERO,
                egui::pos2(1.0, 1.0),
            ))
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
    height
}

fn vice_header_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let id = egui::Id::new("vice_header_texture");
    if let Some(texture) = ctx.data_mut(|data| data.get_temp::<egui::TextureHandle>(id)) {
        return texture;
    }
    let image = image::load_from_memory(include_bytes!("../assets/vice-header.png"))
        .expect("embedded Vice header PNG")
        .to_rgba8();
    let pixels = egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    );
    let texture = ctx.load_texture("vice_header", pixels, egui::TextureOptions::LINEAR);
    ctx.data_mut(|data| data.insert_temp(id, texture.clone()));
    texture
}

fn overlay_corner_combo(ui: &mut egui::Ui, corner: &mut crate::overlay::Corner) {
    use crate::overlay::Corner;
    settings_row(ui, "Position", |ui| {
        choice(
            ui,
            corner,
            vec![
                (Corner::TopLeft, "Top left".into()),
                (Corner::TopRight, "Top right".into()),
                (Corner::BottomLeft, "Bottom left".into()),
                (Corner::BottomRight, "Bottom right".into()),
            ],
            "Select position".into(),
        );
    });
}

fn overlay_editor(ui: &mut egui::Ui, layout: &mut crate::overlay::OverlayLayout) {
    use crate::overlay::Stat;
    layout.normalize();
    let mut movement = None;
    let count = layout.stats.len();
    for (index, choice) in layout.stats.iter_mut().enumerate() {
        let stat = choice.stat;
        ui.push_id(stat, |ui| {
            let row = Frame::new()
                .inner_margin(3)
                .show(ui, |ui| {
                    StripBuilder::new(ui)
                        .size(Size::exact(28.0))
                        .vertical(|mut rows| {
                            rows.cell(|ui| {
                                StripBuilder::new(ui)
                                    .size(Size::exact(12.0))
                                    .size(Size::remainder())
                                    .size(Size::exact(40.0))
                                    .size(Size::exact(56.0))
                                    .clip(true)
                                    .cell_layout(Layout::left_to_right(Align::Center))
                                    .horizontal(|mut columns| {
                                        columns.cell(|ui| {
                                            // Only the grip starts a drag; the other controls retain their input.
                                            ui.dnd_drag_source(
                                                egui::Id::new(("overlay_stat_drag", stat)),
                                                stat,
                                                |ui| {
                                                    ui.label(RichText::new("::").color(
                                                            Palette::current(ui.ctx()).text_muted,
                                                        ))
                                                        .on_hover_text(
                                                            "Drag to reorder this statistic",
                                                        );
                                                },
                                            );
                                        });
                                        columns.cell(|ui| {
                                            ui.add(sc::Checkbox::new(
                                                &mut choice.enabled,
                                                stat.label(),
                                            ))
                                            .on_hover_text(stat.label());
                                        });
                                        columns.cell(|ui| {
                                            if ui
                                                .add(
                                                    sc::Button::outline("Up")
                                                        .small()
                                                        .full_width()
                                                        .disabled(index == 0),
                                                )
                                                .clicked()
                                            {
                                                movement = Some((stat, index.saturating_sub(1)));
                                            }
                                        });
                                        columns.cell(|ui| {
                                            if ui
                                                .add(
                                                    sc::Button::outline("Down")
                                                        .small()
                                                        .full_width()
                                                        .disabled(index + 1 == count),
                                                )
                                                .clicked()
                                            {
                                                movement = Some((stat, index + 1));
                                            }
                                        });
                                    });
                            })
                        });
                })
                .response;
            if row.dnd_hover_payload::<Stat>().is_some() {
                ui.painter().rect_stroke(
                    row.rect,
                    4,
                    Stroke::new(1.0, Palette::current(ui.ctx()).accent),
                    egui::StrokeKind::Inside,
                );
            }
            if let Some(payload) = row.dnd_release_payload::<Stat>() {
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

fn tab_bar(ui: &mut egui::Ui, current_tab: &mut MenuTab, _text_scale: f32) {
    let tabs = [
        MenuTab::Video,
        MenuTab::Picture,
        MenuTab::Audio,
        MenuTab::Display,
    ];
    let mut selected = tabs.iter().position(|tab| tab == current_tab).unwrap_or(0);
    sc::Tabs::new(&mut selected)
        .tabs(["Video", "Picture", "Audio", "Display"])
        .segmented()
        .show(ui);
    *current_tab = tabs[selected];
}

fn settings_section<R>(
    ui: &mut egui::Ui,
    title: &str,
    text_scale: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let p = Palette::current(ui.ctx());
    ui.add_space(10.0);
    ui.label(
        RichText::new(title)
            .size(11.0 * text_scale)
            .strong()
            .color(p.text_muted),
    );
    ui.add_space(4.0);
    ui.vertical(add_contents).inner
}

// Strip cells have fixed bounds: a label or a widget cannot resize a column.
// Every tab uses the same pane width, including its reserved scrollbar gutter.
fn settings_row<R>(ui: &mut egui::Ui, label: &str, controls: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let label_width = (ui.available_width() * 0.32).clamp(88.0, 196.0);
    let control_width =
        (ui.available_width() - label_width - ui.spacing().item_spacing.x).min(360.0);
    let mut result = None;
    ui.push_id(label, |ui| {
        StripBuilder::new(ui)
            .size(Size::exact(48.0))
            .vertical(|mut rows| {
                rows.cell(|ui| {
                    StripBuilder::new(ui)
                        .size(Size::exact(label_width))
                        .size(Size::exact(control_width))
                        .clip(true)
                        .cell_layout(Layout::left_to_right(Align::Center))
                        .horizontal(|mut columns| {
                            columns.cell(|ui| {
                                record_geometry(ui, label, "label_cell", ui.max_rect());
                                let response = ui
                                    .add(egui::Label::new(RichText::new(label).size(14.0)).wrap());
                                record_geometry(ui, label, "label", response.rect);
                            });
                            columns.cell(|ui| {
                                record_geometry(ui, label, "cell", ui.max_rect());
                                #[cfg(test)]
                                ui.data_mut(|data| {
                                    data.insert_temp(
                                        egui::Id::new("settings_row_label"),
                                        label.to_owned(),
                                    )
                                });
                                result = Some(controls(ui));
                                #[cfg(test)]
                                ui.data_mut(|data| {
                                    data.remove::<String>(egui::Id::new("settings_row_label"))
                                });
                            });
                        });
                })
            });
    });
    result.expect("settings control cell")
}

#[cfg(test)]
#[derive(Clone, Debug)]
struct WidgetGeometry {
    label: String,
    role: &'static str,
    rect: egui::Rect,
}

fn record_geometry(_ui: &egui::Ui, _label: &str, _role: &'static str, _rect: egui::Rect) {
    #[cfg(test)]
    _ui.data_mut(|data| {
        let records =
            data.get_temp_mut_or_default::<Vec<WidgetGeometry>>(egui::Id::new("settings_geometry"));
        records.push(WidgetGeometry {
            label: _label.to_owned(),
            role: _role,
            rect: _rect,
        });
    });
}

fn control_geometry(_ui: &egui::Ui, _role: &'static str, _response: &egui::Response) {
    #[cfg(test)]
    if let Some(label) =
        _ui.data(|data| data.get_temp::<String>(egui::Id::new("settings_row_label")))
    {
        record_geometry(_ui, &label, _role, _response.rect);
    }
}

fn toggle_row(ui: &mut egui::Ui, label: &str, value: &mut bool) {
    settings_row(ui, label, |ui| {
        let response = ui.add(sc::Switch::new(value).disabled(!ui.is_enabled()));
        control_geometry(ui, "control", &response);
    });
}

fn choice<T: Clone + PartialEq>(
    ui: &mut egui::Ui,
    value: &mut T,
    options: Vec<(T, String)>,
    unavailable_label: String,
) {
    let mut selected = options.iter().position(|(option, _)| option == value);
    let response = sc::Select::new("choice", &mut selected)
        .options(options.iter().map(|(_, text)| text.clone()))
        .placeholder(unavailable_label)
        .width(ui.available_width())
        .disabled(!ui.is_enabled())
        .show(ui);
    control_geometry(ui, "control", &response);
    if response.changed() {
        if let Some(index) = selected {
            *value = options[index].0.clone();
        }
    }
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
    automatic: &str,
) {
    settings_row(ui, label, |ui| {
        let text = crate::devices::video_device_label(selected);
        let options = options
            .into_iter()
            .map(|value| {
                let text = if value.is_empty() {
                    automatic.into()
                } else {
                    crate::devices::video_device_label(&value)
                };
                (value, text)
            })
            .collect();
        choice(ui, selected, options, text);
    });
}

fn labeled_combo_static(ui: &mut egui::Ui, label: &str, selected: &mut String, options: &[&str]) {
    settings_row(ui, label, |ui| {
        choice(
            ui,
            selected,
            options
                .iter()
                .map(|text| (text.to_string(), text.to_string()))
                .collect(),
            selected.clone(),
        );
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
    settings_row(ui, label, |ui| {
        let automatic = if !live {
            "Auto (pending apply)".into()
        } else {
            format!(
                "Auto ({})",
                active
                    .map(|v| v.split(" (").next().unwrap_or(v))
                    .unwrap_or("connecting…")
            )
        };
        let options = available
            .iter()
            .map(|format| {
                (
                    *format,
                    if *format == VideoFormat::Auto {
                        automatic.clone()
                    } else {
                        format.to_string()
                    },
                )
            })
            .collect();
        choice(ui, selected, options, selected.to_string());
    });
}

fn scaling_filter_combo(ui: &mut egui::Ui, label: &str, selected: &mut ScaleFilter) {
    settings_row(ui, label, |ui| {
        choice(
            ui,
            selected,
            ScaleFilter::ALL
                .into_iter()
                .map(|filter| (filter, filter.to_string()))
                .collect(),
            selected.to_string(),
        );
    });
}

fn color_space_combo(
    ui: &mut egui::Ui,
    label: &str,
    selected: &mut ColorSpace,
    info: ColorInfo,
    height: u32,
) {
    settings_row(ui, label, |ui| {
        choice(
            ui,
            selected,
            ColorSpace::ALL
                .into_iter()
                .map(|space| (space, info.space_label(space, height)))
                .collect(),
            info.space_label(*selected, height),
        );
    });
}

fn color_range_combo(ui: &mut egui::Ui, label: &str, selected: &mut ColorRange, info: ColorInfo) {
    settings_row(ui, label, |ui| {
        choice(
            ui,
            selected,
            ColorRange::ALL
                .into_iter()
                .map(|range| (range, info.range_label(range)))
                .collect(),
            info.range_label(*selected),
        );
    });
}

fn fps_mode_combo(ui: &mut egui::Ui, selected: &mut String, rates: &[FrameRate]) {
    settings_row(ui, "Frame Rate", |ui| {
        let text = selected
            .parse::<FrameRate>()
            .map(|rate| format!("{rate} FPS"))
            .unwrap_or_else(|_| "Custom request".into());
        choice(
            ui,
            selected,
            rates
                .iter()
                .map(|rate| (rate.key(), format!("{rate} FPS")))
                .collect(),
            text,
        );
    });
}

fn labeled_audio_combo(
    ui: &mut egui::Ui,
    label: &str,
    selected: &mut i32,
    devices: &[AudioDevice],
    dshow_option: Option<&str>,
    remembered_name: Option<&str>,
    automatic: &str,
) {
    settings_row(ui, label, |ui| {
        let mut options = vec![(-1, automatic.to_owned())];
        if let Some(text) = dshow_option {
            options.push((crate::devices::DIRECTSHOW_AUDIO_INDEX, text.into()));
        }
        options.extend(
            devices
                .iter()
                .map(|device| (device.index, device.name.clone())),
        );
        let text = if *selected == crate::devices::UNAVAILABLE_AUDIO_INDEX {
            format!(
                "Unavailable: {}",
                remembered_name.unwrap_or("saved audio device")
            )
        } else {
            audio_device_name(*selected, devices, dshow_option)
        };
        choice(ui, selected, options, text);
    });
}

fn labeled_volume(ui: &mut egui::Ui, draft: &mut Settings) {
    let mut volume = (draft.volume * 100.0) as f32;
    settings_row(ui, "Volume (%)", |ui| {
        ui.add_enabled_ui(!draft.audio_muted, |ui| {
            slider_and_number(ui, &mut volume, 0.0..=100.0, 1.0, 0)
        })
        .inner
    });
    draft.volume = volume as f64 / 100.0;
}

fn picture_slider(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f64,
    _text_scale: f32,
) -> bool {
    settings_row(ui, label, |ui| slider_and_number(ui, value, range, step, 2))
}

fn slider_and_number(
    ui: &mut egui::Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f64,
    precision: usize,
) -> bool {
    let mut active = false;
    let number_width = if precision == 0 { 56.0 } else { 96.0 };
    ui.spacing_mut().item_spacing.x = 16.0;
    StripBuilder::new(ui)
        .size(Size::remainder())
        .size(Size::exact(number_width))
        .clip(true)
        .cell_layout(Layout::left_to_right(Align::Center))
        .horizontal(|mut columns| {
            columns.cell(|ui| {
                // The component paints its thumb stroke outside its response.
                // Keep both endpoint strokes inside the strip cell's clip rect.
                ui.add_space(2.0);
                let response = ui.add(
                    sc::Slider::new(value, range.clone())
                        .width((ui.available_width() - 2.0).max(0.0))
                        .disabled(!ui.is_enabled()),
                );
                if response.changed() {
                    *value = ((*value as f64 / step).round() * step) as f32;
                    *value = value.clamp(*range.start(), *range.end());
                }
                active |= response.dragged() || response.has_focus() || response.changed();
                control_geometry(ui, "slider", &response);
            });
            columns.cell(|ui| {
                let response = ui.add_sized(
                    [number_width, 32.0],
                    egui::DragValue::new(value)
                        .range(range.clone())
                        .speed(step)
                        .fixed_decimals(precision),
                );
                active |= response.dragged() || response.has_focus() || response.changed();
                control_geometry(ui, "number", &response);
            });
        });
    active
}

fn styled_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let response = ui.add(sc::Button::outline(text));
    control_geometry(ui, "action", &response);
    response
}

fn exit_button(ui: &mut egui::Ui) -> egui::Response {
    let response = ui.add(sc::Button::danger("Exit TackleCast"));
    control_geometry(ui, "action", &response);
    response
}

fn video_device_options(video_devices: &[String], current: &str) -> Vec<String> {
    let mut options = vec![String::new()];
    options.extend_from_slice(video_devices);
    if !current.is_empty() && !options.iter().any(|device| device == current) {
        options.push(current.to_string());
    }
    options
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
#[path = "ui_render_tests.rs"]
mod render_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmed_usb_warning_and_single_reinitialize_action_are_visible() {
        let ctx = egui::Context::default();
        let warning =
            "USB 2.0 High-Speed (480 Mb/s): connection speed may limit available capture modes.";
        let mut settings = Settings {
            video_device: "USB warning fixture".into(),
            ..Default::default()
        };
        capabilities::cache_test_capabilities(
            &settings.video_device,
            capabilities::DeviceCapabilities {
                modes: vec![capabilities::CaptureMode {
                    format: VideoFormat::Mjpeg,
                    width: 1920,
                    height: 1080,
                    rate: 60.into(),
                }],
                usb_warning: Some(warning.into()),
                ..Default::default()
            },
        );
        let applied = settings.clone();
        let overlay = sample();
        let videos = vec![settings.video_device.clone()];
        for mut tab in [MenuTab::Video, MenuTab::Audio] {
            let mut texts = Vec::new();
            for _ in 0..3 {
                let rendered = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1280.0, 720.0),
                        )),
                        ..Default::default()
                    },
                    |root| {
                        configure_style(root.ctx(), settings.theme);
                        draw_menu(
                            root.ctx(),
                            &mut settings,
                            &mut tab,
                            &UiFrame {
                                overlay: &overlay,
                                settings: &applied,
                                video_devices: &videos,
                                audio_inputs: &[],
                                audio_outputs: &[],
                                is_fullscreen: false,
                                audio_status: "Disconnected",
                                devices_busy: false,
                            },
                            &mut UiOutput::default(),
                        );
                    },
                );
                texts = rendered
                    .shapes
                    .iter()
                    .filter_map(|shape| {
                        if let egui::Shape::Text(text) = &shape.shape {
                            Some(text.galley.job.text.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
            }
            if tab == MenuTab::Video {
                assert!(texts.iter().any(|text| text == warning));
            }
            assert_eq!(
                texts
                    .iter()
                    .filter(|text| text.as_str() == "Reinitialize devices")
                    .count(),
                1
            );
            for removed in ["Reconnect capture", "Restart audio", "Refresh device modes"] {
                assert!(!texts.iter().any(|text| text == removed));
            }
        }
    }

    #[test]
    fn component_theme_switch_and_slider_bind_to_application_values() {
        fn component_frame(
            ctx: &egui::Context,
            theme: Theme,
            enabled: &mut bool,
            value: &mut f32,
            events: Vec<egui::Event>,
        ) -> [egui::Rect; 2] {
            let mut rects = [egui::Rect::NOTHING; 2];
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 240.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    configure_style(ui.ctx(), theme);
                    rects[0] = ui.add(egui_components::Switch::new(enabled)).rect;
                    rects[1] = ui
                        .add(egui_components::Slider::new(value, 0.0..=1.0).width(240.0))
                        .rect;
                },
            );
            rects
        }
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            pressed,
            button: egui::PointerButton::Primary,
            modifiers: egui::Modifiers::NONE,
        };
        for theme in [Theme::Vice, Theme::Classic] {
            let ctx = egui::Context::default();
            let mut enabled = false;
            let mut value = 0.25;
            let rects = component_frame(&ctx, theme, &mut enabled, &mut value, vec![]);
            let installed = egui_components::theme::Theme::get(&ctx);
            assert_eq!(
                installed.colors.primary_background,
                Palette::for_theme(theme).accent
            );
            assert_eq!(
                installed.colors.foreground,
                Palette::for_theme(theme).text_primary
            );
            let point = rects[0].center();
            component_frame(
                &ctx,
                theme,
                &mut enabled,
                &mut value,
                vec![egui::Event::PointerMoved(point), button(point, true)],
            );
            let rects = component_frame(
                &ctx,
                theme,
                &mut enabled,
                &mut value,
                vec![button(point, false)],
            );
            assert!(
                enabled,
                "the library switch must update the application's bound value"
            );
            let point = egui::pos2(rects[1].right() - 10.0, rects[1].center().y);
            component_frame(
                &ctx,
                theme,
                &mut enabled,
                &mut value,
                vec![egui::Event::PointerMoved(point), button(point, true)],
            );
            component_frame(
                &ctx,
                theme,
                &mut enabled,
                &mut value,
                vec![button(point, false)],
            );
            assert!(
                value > 0.9 && value <= 1.0,
                "the library slider must update and constrain the bound value: {value}"
            );
        }
    }

    // No synthetic capture, renderer, timers that continuously draw, or OS
    // input injection. A hidden native window exercises the real egui-winit
    // event bridge; each UI pass requires a production redraw request.
    #[cfg(target_os = "windows")]
    #[test]
    #[allow(deprecated)]
    fn settings_input_schedules_redraw_with_zero_capture_frames() {
        use std::cell::Cell;
        use winit::event::{DeviceId, ElementState, MouseButton};
        use winit::event_loop::EventLoop;
        use winit::platform::windows::EventLoopBuilderExtWindows;

        struct Harness {
            ui: UiState,
            window: Window,
            settings: Settings,
            videos: Vec<String>,
            pending: Cell<bool>,
            draws: usize,
            reinitialize_requests: usize,
            audio: crate::audio::AudioPassthrough,
            saw_undimmed_picture: bool,
        }
        impl Harness {
            fn pump(&mut self) {
                let mut passes = 0;
                while self.pending.replace(false) {
                    passes += 1;
                    assert!(passes <= 60, "UI must settle without continuous rendering");
                    let mut overlay = sample();
                    overlay.width = None;
                    overlay.height = None;
                    overlay.fps = None;
                    overlay.status_message = Some("Capture failed; zero frames".into());
                    overlay.status_is_alert = true;
                    let output = self.ui.prepare(
                        &self.window,
                        UiFrame {
                            overlay: &overlay,
                            settings: &self.settings,
                            video_devices: &self.videos,
                            audio_inputs: &[],
                            audio_outputs: &[],
                            is_fullscreen: false,
                            audio_status: "No capture audio selected",
                            devices_busy: false,
                        },
                    );
                    if let Some(live) = &output.output.live_settings {
                        self.audio.set_volume(live.volume);
                        self.audio.set_muted(live.audio_muted);
                    }
                    let dim_color = Palette::for_theme(self.ui.draft_settings.theme).dim_overlay;
                    let backdrop_drawn = output.paint_jobs.iter().any(|job| match &job.primitive {
                        egui::epaint::Primitive::Mesh(mesh) => {
                            mesh.vertices.iter().any(|vertex| vertex.color == dim_color)
                        }
                        _ => false,
                    });
                    if self.ui.menu_open && self.ui.tab == MenuTab::Audio {
                        assert!(
                            !output.output.video_adjustment_active && backdrop_drawn,
                            "audio controls must keep the video dimmed"
                        );
                    }
                    if output.output.video_adjustment_active {
                        assert!(
                            !backdrop_drawn,
                            "picture adjustments must keep their live image preview"
                        );
                        self.saw_undimmed_picture = true;
                    }
                    if let Some(settings) = output.output.apply_settings {
                        self.settings = settings;
                    }
                    if let Some(settings) = output.output.reinitialize_devices {
                        assert_eq!(settings.video_device, self.videos[1]);
                        self.reinitialize_requests += 1;
                    }
                    self.draws += 1;
                    // Only egui's immediate repaint request can schedule a
                    // follow-up pass. No FrameReady events are ever supplied.
                    self.pending.set(output.output.repaint_after.is_zero());
                    if self.pending.get() {
                        std::thread::sleep(Duration::from_millis(16));
                    }
                }
            }
            fn point(&self, text: &str) -> egui::Pos2 {
                self.ui
                    .last_texts
                    .iter()
                    .find(|(label, _)| label == text)
                    .unwrap_or_else(|| panic!("Missing {text:?}: {:?}", self.ui.last_texts))
                    .1
            }
            fn event(&mut self, event: WindowEvent) -> bool {
                self.ui
                    .on_window_event(&self.window, &event, || self.pending.set(true))
            }
            fn click(&mut self, text: &str) {
                let point = self.point(text);
                self.click_at(point);
            }
            fn control(&self, label: &str, role: &str) -> egui::Rect {
                self.ui.egui_ctx.data(|data| {
                    data.get_temp::<Vec<WidgetGeometry>>(egui::Id::new("settings_geometry"))
                        .unwrap()
                        .iter()
                        .find(|record| record.label == label && record.role == role)
                        .unwrap_or_else(|| panic!("Missing control {label} {role}"))
                        .rect
                })
            }
            fn click_at(&mut self, point: egui::Pos2) {
                let scale = egui_winit::pixels_per_point(&self.ui.egui_ctx, &self.window) as f64;
                self.event(WindowEvent::CursorMoved {
                    device_id: DeviceId::dummy(),
                    position: (point.x as f64 * scale, point.y as f64 * scale).into(),
                });
                self.pump();
                for state in [ElementState::Pressed, ElementState::Released] {
                    self.event(WindowEvent::MouseInput {
                        device_id: DeviceId::dummy(),
                        state,
                        button: MouseButton::Left,
                    });
                    assert!(
                        self.pending.get(),
                        "input must request a UI redraw without video"
                    );
                    self.pump();
                }
            }
            fn escape(&mut self) -> Option<Settings> {
                let applied = self
                    .ui
                    .toggle_menu(&self.settings, || self.pending.set(true));
                assert!(
                    self.pending.get(),
                    "both Escape transitions must schedule drawing"
                );
                applied
            }
        }
        let mut builder = EventLoop::<crate::AppEvent>::with_user_event();
        builder.with_any_thread(true);
        let event_loop = builder.build().unwrap();
        let window = event_loop
            .create_window(
                Window::default_attributes()
                    .with_visible(false)
                    .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0)),
            )
            .unwrap();
        let ui = UiState::new(&window, 4096);
        let videos: Vec<String> = vec![
            "No-frame test capture A".into(),
            "No-frame test capture B".into(),
        ];
        for video in &videos {
            capabilities::cache_test_device(video);
        }
        let mut test = Harness {
            ui,
            window,
            videos,
            settings: Settings::default(),
            pending: Cell::new(false),
            draws: 0,
            reinitialize_requests: 0,
            audio: crate::audio::AudioPassthrough::new(),
            saw_undimmed_picture: false,
        };
        assert_eq!(test.draws, 0);
        assert!(test.escape().is_none());
        test.pump();
        assert!(test.ui.is_menu_open());
        let before = test.draws;
        let consumed = test.event(WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: (2.0, 2.0).into(),
        });
        assert!(
            !consumed,
            "this regression specifically exercises unconsumed pointer movement"
        );
        assert!(
            test.pending.get(),
            "repaint=true, consumed=false must still schedule redraw"
        );
        test.pump();
        assert!(test.draws > before);
        test.click("Display");
        assert_eq!(test.ui.tab, MenuTab::Display);
        let overlay_was_enabled = test.ui.draft_settings.show_overlay;
        test.click_at(test.control("Show performance pill", "control").center());
        assert_ne!(test.ui.draft_settings.show_overlay, overlay_was_enabled);
        test.click("Classic");
        assert_eq!(test.ui.draft_settings.theme, Theme::Classic);
        test.click("Picture");
        assert_eq!(test.ui.tab, MenuTab::Picture);
        let slider = test.control("Brightness", "slider");
        test.click_at(egui::pos2(
            slider.left() + slider.width() * 0.75,
            slider.center().y,
        ));
        assert!(test.ui.draft_settings.brightness > 0.2);
        test.click("Reset to defaults");
        assert_eq!(test.ui.draft_settings.brightness, 0.0);
        assert!(test.saw_undimmed_picture);
        test.click("Audio");
        let slider = test.control("Volume (%)", "slider");
        test.click_at(egui::pos2(
            slider.left() + slider.width() * 0.35,
            slider.center().y,
        ));
        let volume = test.ui.draft_settings.volume;
        assert!(volume > 0.2 && volume < 0.5);
        assert!(
            (test.audio.volume() - volume).abs() < 0.00001,
            "volume must reach the playback controller before Apply"
        );
        assert_eq!(
            test.settings.volume, 1.0,
            "live preview must not require applying the settings draft"
        );
        assert!(test.ui.is_menu_open());
        test.click_at(test.control("Mute audio", "control").center());
        assert!(test.audio.is_muted(), "mute must also preview immediately");
        test.click_at(test.control("Mute audio", "control").center());
        assert!(!test.audio.is_muted());
        assert!(
            !test.audio.has_route(),
            "changing playback gain must not reopen device routes"
        );
        test.click("Video");
        assert_eq!(test.ui.tab, MenuTab::Video);
        test.click("Automatic (match capture audio)");
        test.click("No-frame test capture B");
        assert_eq!(test.ui.draft_settings.video_device, test.videos[1]);
        test.click("Reinitialize devices");
        assert_eq!(test.reinitialize_requests, 1);
        assert!(
            test.ui.is_menu_open(),
            "reinitialization keeps settings open"
        );
        test.settings = test
            .escape()
            .expect("closing must apply the selected device");
        assert_eq!(test.settings.video_device, test.videos[1]);
        test.pump();
        assert!(!test.ui.is_menu_open());
        assert!(test.escape().is_none());
        test.pump();
        test.click("Apply & close");
        assert!(!test.ui.is_menu_open());
        assert_eq!(test.settings.video_device, test.videos[1]);
        let idle_draws = test.draws;
        test.pump();
        assert_eq!(
            test.draws, idle_draws,
            "idle with zero frames must not continuously redraw"
        );
        crate::cli_tests::check_soft_reinitialize_without_capture_frames(event_loop.create_proxy());
    }

    #[test]
    fn vice_header_art_reaches_panel_edges_while_title_stays_padded() {
        let ctx = egui::Context::default();
        let mut panel = egui::Rect::NOTHING;
        let mut content = egui::Rect::NOTHING;
        let output = ctx.run_ui(egui::RawInput::default(), |root_ui| {
            let ctx = root_ui.ctx();
            configure_style(ctx, Theme::Vice);
            let p = Palette::current(ctx);
            egui::CentralPanel::default().show(root_ui, |ui| {
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
            egui::pos2(
                panel.right() - 1.0,
                panel.top() + 1.0 + (panel.width() - 2.0) / texture.aspect_ratio(),
            ),
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
            art.brush.as_ref().unwrap().uv,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            "entire source image must be visible"
        );
        assert!(
            (art.rect.aspect_ratio() - texture.aspect_ratio()).abs() < 0.001,
            "banner must retain its source proportions"
        );
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
        let output = ctx.run_ui(egui::RawInput::default(), |root_ui| {
            egui::CentralPanel::default().show(root_ui, |ui| overlay_editor(ui, &mut layout));
        });
        for clipped in output.shapes {
            if let egui::Shape::Text(text) = clipped.shape {
                for section in &text.galley.job.sections {
                    let label =
                        &text.galley.job.text[section.byte_range.start.0..section.byte_range.end.0];
                    assert!(
                        ctx.fonts_mut(|fonts| fonts.has_glyphs(&section.format.font_id, label)),
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
            devices_busy: false,
        };
        let mut tab = MenuTab::Display;
        ctx.data_mut(|data| data.insert_temp(egui::Id::new("test_overlay_focus"), true));
        let mut visible = Vec::new();
        for _ in 0..12 {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 720.0),
                    )),
                    ..Default::default()
                },
                |root_ui| {
                    let ctx = root_ui.ctx();
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
            devices_busy: false,
        };
        let mut tab = MenuTab::Display;
        let mut texts = String::new();
        for _ in 0..3 {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 720.0),
                    )),
                    ..Default::default()
                },
                |root_ui| {
                    let ctx = root_ui.ctx();
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
                let _ = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |root_ui| draw_overlay(root_ui.ctx(), &overlay),
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
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(420.0, 500.0),
                )),
                events,
                ..Default::default()
            },
            |root_ui| {
                let ctx = root_ui.ctx();
                configure_style(ctx, Theme::Vice);
                egui::CentralPanel::default().show(root_ui, |ui| overlay_editor(ui, layout));
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
    fn picture_number_fields_accept_keyboard_edits_and_clamp_values() {
        fn frame(
            ctx: &egui::Context,
            settings: &mut Settings,
            events: Vec<egui::Event>,
        ) -> egui::Rect {
            let applied = settings.clone();
            let overlay = sample();
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 720.0),
                    )),
                    events,
                    ..Default::default()
                },
                |root| {
                    configure_style(root.ctx(), settings.theme);
                    draw_menu(
                        root.ctx(),
                        settings,
                        &mut MenuTab::Picture,
                        &UiFrame {
                            overlay: &overlay,
                            settings: &applied,
                            video_devices: &[],
                            audio_inputs: &[],
                            audio_outputs: &[],
                            is_fullscreen: false,
                            audio_status: "Not running",
                            devices_busy: false,
                        },
                        &mut UiOutput::default(),
                    );
                },
            );
            ctx.data(|data| {
                data.get_temp::<Vec<WidgetGeometry>>(egui::Id::new("settings_geometry"))
            })
            .unwrap()
            .iter()
            .find(|record| record.label == "Brightness" && record.role == "number")
            .unwrap()
            .rect
        }
        for theme in [Theme::Vice, Theme::Classic] {
            let ctx = egui::Context::default();
            let mut settings = Settings {
                theme,
                ..Default::default()
            };
            for (text, expected) in [("0.25", 0.25), ("99", 1.0), ("-99", -1.0)] {
                let mut rect = egui::Rect::NOTHING;
                for _ in 0..3 {
                    rect = frame(&ctx, &mut settings, vec![]);
                }
                for pressed in [true, false] {
                    frame(
                        &ctx,
                        &mut settings,
                        vec![
                            egui::Event::PointerMoved(rect.center()),
                            egui::Event::PointerButton {
                                pos: rect.center(),
                                pressed,
                                button: egui::PointerButton::Primary,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                    );
                }
                let key = |key, modifiers| egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                };
                frame(
                    &ctx,
                    &mut settings,
                    vec![
                        key(egui::Key::A, egui::Modifiers::COMMAND),
                        egui::Event::Text(text.into()),
                        key(egui::Key::Enter, egui::Modifiers::NONE),
                    ],
                );
                assert!(
                    (settings.brightness - expected).abs() < 0.001,
                    "{theme:?} {text}: {}",
                    settings.brightness
                );
            }
        }
    }

    #[test]
    fn menus_keep_stable_bounds_across_tabs_themes_and_small_windows() {
        for size in [
            egui::vec2(320.0, 360.0),
            egui::vec2(640.0, 480.0),
            egui::vec2(1280.0, 720.0),
            egui::vec2(1920.0, 1080.0),
            egui::vec2(1920.0, 480.0),
            egui::vec2(3840.0, 2160.0),
        ] {
            for (theme, pixels_per_point) in [Theme::Vice, Theme::Classic]
                .into_iter()
                .flat_map(|theme| [1.0, 1.25, 1.5, 2.0].map(move |dpi| (theme, dpi)))
            {
                let ctx = egui::Context::default();
                ctx.set_pixels_per_point(pixels_per_point);
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
                    devices_busy: false,
                };
                let mut previous_size = None;
                let mut label_left = None;
                let mut control_left = None;
                let mut control_right = None;
                let mut slider_edges = None;
                let mut number_edges = None;
                for mut tab in [
                    MenuTab::Video,
                    MenuTab::Picture,
                    MenuTab::Audio,
                    MenuTab::Display,
                ] {
                    for pass in 0..3 {
                        let rendered = ctx.run_ui(
                            egui::RawInput {
                                screen_rect: Some(screen),
                                ..Default::default()
                            },
                            |root_ui| {
                                let ctx = root_ui.ctx();
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
                        for shape in rendered.shapes {
                            if let egui::Shape::Rect(rect) = &shape.shape {
                                if let Some(brush) = &rect.brush {
                                    if let Some(texture) = ctx.data(|data| {
                                        data.get_temp::<egui::TextureHandle>(egui::Id::new(
                                            "vice_header_texture",
                                        ))
                                    }) {
                                        if brush.fill_texture_id == texture.id() {
                                            assert_eq!(brush.uv, egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), "cropped banner at {size:?} scale={pixels_per_point}");
                                            assert!((rect.rect.height() - rect.rect.width() / texture.aspect_ratio()).abs() * pixels_per_point <= 1.01, "distorted banner at {size:?} scale={pixels_per_point}: {:?}", rect.rect);
                                            if pass == 2 {
                                                assert!(shape.clip_rect.expand(1.0 / pixels_per_point).contains_rect(rect.rect), "clipped banner at {size:?} scale={pixels_per_point}: {:?} in {:?}", rect.rect, shape.clip_rect);
                                            }
                                        }
                                    }
                                }
                            }
                            if let egui::Shape::Text(text) = shape.shape {
                                let text = &text.galley.job.text;
                                for subtitle in [
                                    "EDITION  /  SETTINGS",
                                    "CLASSIC  /  SETTINGS",
                                    "Escape also saves",
                                    "Choose your capture card",
                                    "Modes reported by",
                                    "Requested capture rate",
                                    "Automatic uses a format",
                                    "How the captured picture",
                                    "Capture audio. Automatic",
                                    "Speakers or headphones",
                                    "Preview Vice",
                                    "Display the capture statistics",
                                    "Retry the selected video",
                                    "Reopen the selected audio",
                                    "Drag the grip to reorder",
                                    "Matched input:",
                                    "Matched:",
                                ] {
                                    assert!(
                                        !text.contains(subtitle),
                                        "removed subtitle returned: {text}"
                                    );
                                }
                            }
                        }
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
                    let geometry = ctx.data(|data| {
                        data.get_temp::<Vec<WidgetGeometry>>(egui::Id::new("settings_geometry"))
                            .unwrap()
                    });
                    assert!(!geometry.is_empty());
                    let viewport = geometry
                        .iter()
                        .find(|record| record.role == "viewport")
                        .unwrap()
                        .rect;
                    let scroll_outer = geometry
                        .iter()
                        .find(|record| record.role == "scroll_outer")
                        .unwrap()
                        .rect;
                    assert!(scroll_outer.right() - viewport.right() >= 17.0,
                        "scrollbar must reserve its width and padding: {theme:?} {tab:?} {size:?} {viewport:?} in {scroll_outer:?}");
                    let close = |a: f32, b: f32| (a - b).abs() <= 1.0;
                    for widget in &geometry {
                        let bounds = |role| {
                            geometry
                                .iter()
                                .find(|record| record.label == widget.label && record.role == role)
                                .unwrap()
                                .rect
                        };
                        match widget.role {
                            "label" => {
                                assert!(
                                    bounds("label_cell").expand(1.0).contains_rect(widget.rect),
                                    "clipped label {theme:?} {size:?}: {widget:?}"
                                );
                                if let Some(left) = label_left {
                                    assert!(
                                        close(widget.rect.left(), left),
                                        "label alignment: {widget:?}"
                                    );
                                }
                                label_left = Some(widget.rect.left());
                                assert!(
                                    close(widget.rect.center().y, bounds("cell").center().y),
                                    "label centering: {widget:?}"
                                );
                            }
                            "cell" => {
                                assert!(
                                    widget.rect.right() <= viewport.right() + 1.0,
                                    "control enters scrollbar gutter: {widget:?}"
                                );
                                assert!(
                                    widget.rect.width() <= 361.0,
                                    "overwide control column: {widget:?}"
                                );
                                if let Some(left) = control_left {
                                    assert!(
                                        close(widget.rect.left(), left),
                                        "control column: {theme:?} {tab:?} {size:?} {widget:?}"
                                    );
                                }
                                if let Some(right) = control_right {
                                    assert!(
                                        close(widget.rect.right(), right),
                                        "control column right: {widget:?}"
                                    );
                                }
                                control_left = Some(widget.rect.left());
                                control_right = Some(widget.rect.right());
                            }
                            "control" | "action" | "slider" | "number" => {
                                let cell = bounds("cell");
                                if widget.role == "action" && size.x >= 640.0 {
                                    assert!(
                                        widget.rect.width() < cell.width() - 24.0,
                                        "action should fit its text: {widget:?}"
                                    );
                                }
                                assert!(
                                    cell.expand(1.0).contains_rect(widget.rect),
                                    "overflow: {theme:?} {size:?} {widget:?} in {cell:?}"
                                );
                                assert!(
                                    close(widget.rect.center().y, cell.center().y),
                                    "vertical alignment: {widget:?} in {cell:?}"
                                );
                                if widget.role != "number" {
                                    assert!(
                                        close(
                                            widget.rect.left(),
                                            cell.left()
                                                + if widget.role == "slider" { 2.0 } else { 0.0 }
                                        ),
                                        "widget alignment: {widget:?}"
                                    );
                                }
                                if widget.role == "slider" {
                                    let edges = (widget.rect.left(), widget.rect.right());
                                    if widget.label != "Volume (%)" {
                                        if let Some((left, right)) = slider_edges {
                                            assert!(
                                                close(edges.0, left) && close(edges.1, right),
                                                "slider alignment: {widget:?}"
                                            );
                                        }
                                        slider_edges = Some(edges);
                                    }
                                    assert!(
                                        widget.rect.width() >= 24.0,
                                        "usable slider: {widget:?}"
                                    );
                                    assert!(
                                        bounds("number").left() - widget.rect.right() >= 15.0,
                                        "slider needs clearance from value: {widget:?}"
                                    );
                                }
                                if widget.role == "number" {
                                    let edges = (widget.rect.left(), widget.rect.right());
                                    if widget.label == "Volume (%)" {
                                        assert!(
                                            widget.rect.width() <= 57.0,
                                            "compact volume value: {widget:?}"
                                        );
                                    } else {
                                        if let Some((left, right)) = number_edges {
                                            assert!(
                                                close(edges.0, left) && close(edges.1, right),
                                                "value alignment: {widget:?}"
                                            );
                                        }
                                        number_edges = Some(edges);
                                    }
                                    assert!(
                                        close(widget.rect.right(), cell.right()),
                                        "value right edge: {widget:?}"
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
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
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(320.0, 360.0),
                    )),
                    ..Default::default()
                },
                |root_ui| draw_overlay(root_ui.ctx(), &overlay),
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
