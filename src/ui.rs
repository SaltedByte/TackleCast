use crate::capabilities;
use crate::color::ColorInfo;
use crate::devices::AudioDevice;
use crate::frame_rate::FrameRate;
use crate::framegen::FgDiagnostics;
use crate::settings::{
    ColorRange, ColorSpace, FrameGenArtifactMode, FrameGenBackend, FrameGenDebugView, FrameGenMode, RifeModel,
    FrameGenTarget, ScaleFilter, Settings, VideoFormat,
};
use egui::{
    Align, Align2, Button, Color32, ComboBox, CornerRadius, FontId, Frame, Layout, Margin,
    RichText, Slider, Stroke,
};
use egui_wgpu::ScreenDescriptor;
use egui_winit::State;
use std::time::Duration;
use winit::event::WindowEvent;
use winit::window::Window;

const COLOR_TEXT_PRIMARY: Color32 = Color32::from_rgb(0xE0, 0xE0, 0xE0);
const COLOR_TEXT_SECONDARY: Color32 = Color32::from_rgb(0x88, 0x99, 0xAA);
const COLOR_TEXT_MUTED: Color32 = Color32::from_rgb(0x88, 0x99, 0xAA);
const COLOR_ACCENT: Color32 = Color32::from_rgb(0xE9, 0x45, 0x60);
const COLOR_ACCENT_HOVER: Color32 = Color32::from_rgb(0xF4, 0x6A, 0x81);
const COLOR_CARD_BG: Color32 = Color32::from_rgb(0x10, 0x19, 0x2F);
const COLOR_PANEL_BG: Color32 = Color32::from_rgb(0x16, 0x21, 0x3E);
const COLOR_PANEL_HOVER: Color32 = Color32::from_rgb(0x1A, 0x2A, 0x50);
const COLOR_BORDER: Color32 = Color32::from_rgb(0x0F, 0x34, 0x60);
const COLOR_BORDER_SUBTLE: Color32 = Color32::from_rgb(0x1A, 0x2A, 0x50);
const COLOR_MENU_BORDER: Color32 = Color32::from_rgb(0x1A, 0x2A, 0x50);
const COLOR_TAB_BAR_BG: Color32 = Color32::from_rgb(0x0C, 0x10, 0x22);
const COLOR_TAB_ACTIVE_BG: Color32 = Color32::from_rgb(0x16, 0x21, 0x3E);
const COLOR_DIM_OVERLAY: Color32 = Color32::from_black_alpha(120);
const COLOR_PILL_BG: Color32 = Color32::from_black_alpha(180);
const COLOR_EXIT_BG: Color32 = Color32::from_rgb(0x3A, 0x10, 0x20);
const COLOR_EXIT_BORDER: Color32 = Color32::from_rgb(0xE9, 0x45, 0x60);
const COLOR_EXIT_TEXT: Color32 = Color32::from_rgb(0xE0, 0xE0, 0xE0);
const COLOR_WARNING: Color32 = Color32::from_rgb(0xF5, 0x9E, 0x0B);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MenuTab {
    #[default]
    Video,
    Picture,
    Audio,
    FrameGen,
    Display,
}

pub struct UiState {
    egui_ctx: egui::Context,
    egui_winit: State,
    menu_open: bool,
    draft_settings: Settings,
    tab: MenuTab,
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
    /// Stack resolution, filter, and FPS over three lines instead of one.
    pub detailed: bool,
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
    pub framegen: &'a FgDiagnostics,
    /// Enabled from the command line for this run.
    pub framegen_forced: bool,
    /// Diagnostic clip recorder status ("" when idle and never used).
    pub recorder_status: &'a str,
    /// Whether a clip can be recorded right now (CPU frames available).
    pub recorder_available: bool,
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
    pub retry_framegen: bool,
    /// Start recording a diagnostic clip (raw captured frames).
    pub record_clip: bool,
    pub repaint_after: Duration,
}

impl UiState {
    pub fn new(window: &Window, max_texture_side: usize) -> Self {
        let egui_ctx = egui::Context::default();
        configure_style(&egui_ctx);

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
                overlay.filter = self.draft_settings.scaling_filter;
                overlay.filter_label = self.draft_settings.scaling_filter.to_string();
            }
            draw_overlay(ctx, &overlay, frame.framegen);
            if frame.settings.framegen_overlay && !self.menu_open {
                draw_framegen_overlay(ctx, frame.framegen);
            }
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

fn configure_style(ctx: &egui::Context) {
    // Keep the app's branding when the desktop reports a light system theme.
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(COLOR_TEXT_PRIMARY);
    style.visuals.panel_fill = Color32::TRANSPARENT;
    style.visuals.window_fill = menu_background();
    style.visuals.window_stroke = Stroke::new(1.0_f32, COLOR_MENU_BORDER);
    style.visuals.window_corner_radius = CornerRadius::same(12);
    style.visuals.menu_corner_radius = CornerRadius::same(8);
    style.visuals.widgets.noninteractive.bg_fill = COLOR_PANEL_BG;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, COLOR_BORDER_SUBTLE);
    style.visuals.widgets.noninteractive.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.noninteractive.fg_stroke.color = COLOR_TEXT_PRIMARY;
    style.visuals.widgets.inactive.bg_fill = COLOR_PANEL_BG;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, COLOR_BORDER);
    style.visuals.widgets.inactive.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.inactive.fg_stroke.color = COLOR_TEXT_PRIMARY;
    style.visuals.widgets.hovered.bg_fill = COLOR_PANEL_HOVER;
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, COLOR_ACCENT_HOVER);
    style.visuals.widgets.hovered.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.hovered.fg_stroke.color = COLOR_TEXT_PRIMARY;
    style.visuals.widgets.active.bg_fill = COLOR_PANEL_HOVER;
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, COLOR_ACCENT);
    style.visuals.widgets.active.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.active.fg_stroke.color = COLOR_TEXT_PRIMARY;
    style.visuals.selection.bg_fill = COLOR_ACCENT;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, COLOR_ACCENT);
    style.visuals.slider_trailing_fill = true;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    ctx.set_style(style);
}

fn draw_overlay(ctx: &egui::Context, overlay: &OverlayInfo, framegen: &FgDiagnostics) {
    let mut text = overlay_text(overlay);
    if let Some(text) = text.as_mut() {
        if overlay.status_message.is_none() && framegen.active && !framegen.paused {
            text.push_str(&format!(
                "  \u{2022}  FG 2\u{00D7} {:.1} FPS",
                framegen.displayed_fps
            ));
        }
    }
    let color = if overlay.status_is_alert {
        COLOR_EXIT_BORDER
    } else {
        COLOR_TEXT_PRIMARY
    };

    if text.is_none() {
        return;
    }

    egui::Area::new("fps_overlay".into())
        .anchor(Align2::LEFT_TOP, [12.0, 12.0])
        .interactable(false)
        .movable(false)
        .show(ctx, |ui| {
            Frame::new()
                .fill(COLOR_PILL_BG)
                .stroke(Stroke::new(1.0_f32, Color32::from_white_alpha(30)))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(12, 6))
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(text.unwrap_or_default())
                                .font(FontId::proportional(13.0))
                                .strong()
                                .color(color),
                        )
                        .wrap_mode(egui::TextWrapMode::Extend),
                    );
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
    let screen = ctx.screen_rect();
    let scale = (screen.width() / 1280.0).clamp(0.85, 1.3);
    let menu_width = (480.0_f32 * scale).min((screen.width() - 56.0).max(180.0));
    let text_scale = scale.clamp(0.9, 1.15);
    // Reserve the same space for every tab. Shorter pages keep their empty
    // space, while small windows scroll instead of growing the outer menu.
    let content_height = (400.0 * text_scale).min((screen.height() - 190.0).max(90.0));
    egui::Area::new("settings_menu".into())
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .order(egui::Order::Foreground)
        .movable(false)
        .show(ctx, |ui| {
            Frame::new()
                .fill(menu_background())
                .stroke(Stroke::new(1.0_f32, COLOR_MENU_BORDER))
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::same(18))
                .show(ui, |ui| {
                    ui.set_width(menu_width);
                    ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                    ui.vertical(|ui| {
                        ui.with_layout(Layout::top_down(Align::Center), |ui| {
                            ui.label(
                                RichText::new("Settings")
                                    .font(FontId::proportional(20.0 * text_scale))
                                    .strong(),
                            );
                        });
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
                                                    .color(COLOR_TEXT_SECONDARY),
                                            );
                                            if styled_button(ui, "Restart audio").clicked() {
                                                output.restart_audio = true;
                                            }
                                        });
                                    }
                                    MenuTab::FrameGen => {
                                        framegen_tab(ui, draft, frame, output, text_scale);
                                    }
                                    MenuTab::Display => {
                                        card_section(ui, "WINDOW & DISPLAY", text_scale, |ui| {
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
                                                "Show Performance Overlay (FPS & Resolution)",
                                            );
                                            if draft.show_overlay {
                                                ui.checkbox(
                                                    &mut draft.detailed_overlay,
                                                    "Include Scaling & Color Info",
                                                );
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
                                    .color(COLOR_TEXT_MUTED),
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
                ui.painter().rect_filled(screen, 0.0, COLOR_DIM_OVERLAY);
            }
            if response.clicked() {
                output.apply_settings = Some(draft.clone());
            }
        });
}

fn tab_bar(ui: &mut egui::Ui, current_tab: &mut MenuTab, text_scale: f32) {
    let tabs = [
        (MenuTab::Video, "Video"),
        (MenuTab::Picture, "Picture"),
        (MenuTab::Audio, "Audio"),
        (MenuTab::FrameGen, "Frame Gen"),
        (MenuTab::Display, "Display"),
    ];

    Frame::new()
        .fill(COLOR_TAB_BAR_BG)
        .stroke(Stroke::new(1.0_f32, COLOR_BORDER_SUBTLE))
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
                            COLOR_TAB_ACTIVE_BG,
                            COLOR_TEXT_PRIMARY,
                            Stroke::new(1.0_f32, COLOR_ACCENT),
                        )
                    } else {
                        (Color32::TRANSPARENT, COLOR_TEXT_MUTED, Stroke::NONE)
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
    ui.add_space(3.0);
    ui.label(
        RichText::new(title)
            .size(11.0 * text_scale)
            .strong()
            .color(COLOR_TEXT_MUTED),
    );
    ui.add_space(1.0);
    Frame::new()
        .fill(COLOR_CARD_BG)
        .stroke(Stroke::new(1.0_f32, COLOR_BORDER_SUBTLE))
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
    ui.label(
        RichText::new(text)
            .size(12.0 * text_scale)
            .color(COLOR_WARNING),
    );
}

fn labeled_combo_string(
    ui: &mut egui::Ui,
    label: &str,
    selected: &mut String,
    options: Vec<String>,
) {
    ui.label(RichText::new(label).color(COLOR_TEXT_SECONDARY));
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
    ui.label(RichText::new(label).color(COLOR_TEXT_SECONDARY));
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
    ui.label(RichText::new(label).color(COLOR_TEXT_SECONDARY));
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
    ui.label(RichText::new(label).color(COLOR_TEXT_SECONDARY));
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
    ui.label(RichText::new(label).color(COLOR_TEXT_SECONDARY));
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
    ui.label(RichText::new(label).color(COLOR_TEXT_SECONDARY));
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
    ui.label(RichText::new("Frame Rate").color(COLOR_TEXT_SECONDARY));
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
    ui.label(RichText::new(label).color(COLOR_TEXT_SECONDARY));
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
                        .color(COLOR_TEXT_SECONDARY),
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
    ui.add(
        Button::new(RichText::new(text).color(COLOR_TEXT_PRIMARY).strong())
            .fill(COLOR_PANEL_BG)
            .stroke(Stroke::new(1.0_f32, COLOR_BORDER))
            .corner_radius(CornerRadius::same(6))
            .min_size(egui::vec2(0.0, 28.0)),
    )
}

fn exit_button(ui: &mut egui::Ui) -> egui::Response {
    ui.with_layout(Layout::top_down(Align::Center), |ui| {
        ui.add(
            Button::new(
                RichText::new("Exit TackleCast")
                    .color(COLOR_EXIT_TEXT)
                    .strong(),
            )
            .fill(COLOR_EXIT_BG)
            .stroke(Stroke::new(1.0_f32, COLOR_EXIT_BORDER))
            .corner_radius(CornerRadius::same(6))
            .min_size(egui::vec2(ui.available_width(), 32.0)),
        )
    })
    .inner
}

fn menu_background() -> Color32 {
    Color32::from_rgba_unmultiplied(12, 12, 28, 240)
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
    if let Some(message) = overlay.status_message.as_ref() {
        return Some(message.clone());
    }

    if !overlay.show_overlay {
        return None;
    }

    match (overlay.width, overlay.height, overlay.fps) {
        (Some(width), Some(height), Some(fps)) => Some({
            let fmt_suffix = overlay
                .active_format
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!(" | {s}"))
                .unwrap_or_default();
            let nominal = overlay
                .nominal_rate
                .map(|rate| format!(" | {rate} nominal FPS"))
                .unwrap_or_else(|| " | nominal FPS unknown".into());
            let fmt_suffix = format!("{fmt_suffix}{nominal}");
            if overlay.detailed {
                format!(
                    "{width}x{height}{fmt_suffix}\n{} | {} {}\n{fps:.1} delivered FPS",
                    overlay.filter_label,
                    overlay.color_info.space_label(overlay.color_space, height),
                    overlay.color_info.range_label(overlay.color_range)
                )
            } else {
                format!("{width}x{height}{fmt_suffix} | {fps:.1} delivered FPS")
            }
        }),
        _ => Some("Waiting For Video...".to_string()),
    }
}

fn framegen_tab(
    ui: &mut egui::Ui,
    draft: &mut Settings,
    frame: &UiFrame<'_>,
    output: &mut UiOutput,
    text_scale: f32,
) {
    let diag = frame.framegen;
    card_section(ui, "FRAME GENERATION", text_scale, |ui| {
        ui.checkbox(&mut draft.framegen_enabled, "Enable Frame Generation");
        if frame.framegen_forced && !draft.framegen_enabled {
            warning_text(
                ui,
                "Enabled for this run by --framegen; this checkbox sets the saved default.",
                text_scale,
            );
        }
        ui.add_enabled_ui(draft.framegen_enabled, |ui| {
            ui.columns(2, |columns| {
                columns[0].label(RichText::new("Backend").color(COLOR_TEXT_SECONDARY));
                ComboBox::from_id_salt("framegen_backend")
                    .width(columns[0].available_width())
                    .selected_text(draft.framegen_backend.to_string())
                    .show_ui(&mut columns[0], |ui| {
                        for backend in [FrameGenBackend::Rife, FrameGenBackend::NvidiaFruc] {
                            ui.selectable_value(
                                &mut draft.framegen_backend,
                                backend,
                                backend.to_string(),
                            );
                        }
                    });
                if draft.framegen_backend == FrameGenBackend::Rife {
                    columns[0].label(RichText::new("RIFE model").color(COLOR_TEXT_SECONDARY));
                    ComboBox::from_id_salt("framegen_rife_model")
                        .width(columns[0].available_width())
                        .selected_text(draft.framegen_rife_model.to_string())
                        .show_ui(&mut columns[0], |ui| {
                            for model in RifeModel::ALL {
                                ui.selectable_value(
                                    &mut draft.framegen_rife_model,
                                    model,
                                    model.to_string(),
                                );
                            }
                        });
                }
                columns[1].label(RichText::new("Mode").color(COLOR_TEXT_SECONDARY));
                ComboBox::from_id_salt("framegen_mode")
                    .width(columns[1].available_width())
                    .selected_text(draft.framegen_mode.to_string())
                    .show_ui(&mut columns[1], |ui| {
                        for mode in [FrameGenMode::Off, FrameGenMode::Double] {
                            ui.selectable_value(&mut draft.framegen_mode, mode, mode.to_string());
                        }
                    });
            });
            ui.label(RichText::new("Output target").color(COLOR_TEXT_SECONDARY));
            ComboBox::from_id_salt("framegen_target")
                .width(ui.available_width())
                .selected_text(draft.framegen_target.to_string())
                .show_ui(ui, |ui| {
                    for target in [FrameGenTarget::Fps60, FrameGenTarget::AlwaysDouble] {
                        ui.selectable_value(&mut draft.framegen_target, target, target.to_string());
                    }
                });
            ui.checkbox(&mut draft.framegen_overlay, "Diagnostics overlay");
            ui.checkbox(
                &mut draft.framegen_hud_guard,
                "Keep HUD steady (crosshair, icons, text stay sharp while the camera moves)",
            );
            ui.add_space(4.0);
            ui.label(RichText::new("Artifact reduction").color(COLOR_TEXT_SECONDARY));
            ComboBox::from_id_salt("framegen_artifact_mode")
                .width(ui.available_width())
                .selected_text(draft.framegen_artifact_mode.to_string())
                .show_ui(ui, |ui| {
                    for mode in [
                        FrameGenArtifactMode::Off,
                        FrameGenArtifactMode::Dynamic,
                        FrameGenArtifactMode::Advanced,
                    ] {
                        ui.selectable_value(
                            &mut draft.framegen_artifact_mode,
                            mode,
                            mode.to_string(),
                        );
                    }
                });
            if draft.framegen_artifact_mode != FrameGenArtifactMode::Off {
                ui.checkbox(
                    &mut draft.framegen_fix_static,
                    "Dynamic HUD protection (HUD and still areas warped by interpolation)",
                );
                ui.checkbox(
                    &mut draft.framegen_fix_motion,
                    "Occlusion-aware correction (HUD ghosts and smearing on panning scenery)",
                );
                ui.checkbox(
                    &mut draft.framegen_fix_edges,
                    "Dynamic edge correction (content entering the screen during camera motion)",
                );
                ui.checkbox(
                    &mut draft.framegen_fix_transitions,
                    "UI transition stabilisation (fading UI, scene cuts)",
                );
            }
            egui::CollapsingHeader::new("Advanced / debug")
                .id_salt("framegen_debug")
                .show(ui, |ui| {
                    ui.label(RichText::new("Show in generated frames").color(COLOR_TEXT_SECONDARY));
                    ComboBox::from_id_salt("framegen_debug_view")
                        .width(ui.available_width())
                        .selected_text(draft.framegen_debug_view.to_string())
                        .show_ui(ui, |ui| {
                            for view in [
                                FrameGenDebugView::Final,
                                FrameGenDebugView::RawFruc,
                                FrameGenDebugView::CorrectionOverlay,
                            ] {
                                ui.selectable_value(
                                    &mut draft.framegen_debug_view,
                                    view,
                                    view.to_string(),
                                );
                            }
                        });
                    if draft.framegen_debug_view == FrameGenDebugView::CorrectionOverlay {
                        ui.label(
                            RichText::new(
                                "red = still-pixel fix, green = camera-motion fix, \
                                 blue = screen-edge fix, yellow = fade fix, magenta = manual region",
                            )
                            .size(12.0 * text_scale)
                            .color(COLOR_TEXT_MUTED),
                        );
                    }
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Manual regions (troubleshooting only: always shown from the real \
                             frame, so they update at the source rate)",
                        )
                        .color(COLOR_TEXT_SECONDARY),
                    );
                    ui.horizontal_wrapped(|ui| {
                        for (bit, name) in [
                            (crate::render::ZONE_BOTTOM_LEFT, "Bottom-left"),
                            (crate::render::ZONE_BOTTOM_RIGHT, "Bottom-right"),
                            (crate::render::ZONE_TOP_LEFT, "Top-left"),
                            (crate::render::ZONE_TOP_RIGHT, "Top-right"),
                        ] {
                            let mut on = draft.framegen_manual_zones & bit != 0;
                            if ui.checkbox(&mut on, name).changed() {
                                draft.framegen_manual_zones ^= bit;
                            }
                        }
                    });
                    if draft.framegen_manual_zones != 0 {
                        ui.add(
                            egui::Slider::new(&mut draft.framegen_manual_zone_width, 5.0..=40.0)
                                .step_by(1.0)
                                .suffix(" %")
                                .text("region width"),
                        );
                        ui.add(
                            egui::Slider::new(&mut draft.framegen_manual_zone_height, 5.0..=40.0)
                                .step_by(1.0)
                                .suffix(" %")
                                .text("region height"),
                        );
                    }
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Diagnostic clip: saves the next 10 s of captured frames (raw, \
                             ~1.7 GB) to the recordings folder for offline replay \
                             (--fg-replay). Recording does not change what is shown.",
                        )
                        .size(12.0 * text_scale)
                        .color(COLOR_TEXT_MUTED),
                    );
                    ui.add_enabled_ui(frame.recorder_available, |ui| {
                        if styled_button(ui, "Record diagnostic clip (10 s)")
                            .clicked()
                        {
                            output.record_clip = true;
                        }
                    });
                    if !frame.recorder_status.is_empty() {
                        ui.label(
                            RichText::new(frame.recorder_status)
                                .size(12.0 * text_scale)
                                .color(COLOR_TEXT_SECONDARY),
                        );
                    }
                });
            ui.label(
                RichText::new("FRUC runtime folder (Optical Flow SDK or folder with NvOFFRUC.dll)")
                    .color(COLOR_TEXT_SECONDARY),
            );
            ui.add(
                egui::TextEdit::singleline(&mut draft.framegen_runtime_dir)
                    .hint_text("Auto: next to the app, ..\\..\\Optical_Flow_SDK_5.0.7")
                    .desired_width(f32::INFINITY),
            );
        });
        ui.label(
            RichText::new(
                "Interpolates one frame between each pair of new game frames using the GPU's \
                 optical-flow engine. Adds about half a source frame of latency (~17 ms at 30 FPS) \
                 plus processing time; the exact figure is shown below while active.",
            )
            .size(12.0 * text_scale)
            .color(COLOR_TEXT_MUTED),
        );
    });
    card_section(ui, "STATUS", text_scale, |ui| {
        let status_color = if diag.active {
            COLOR_TEXT_PRIMARY
        } else if diag.state.starts_with("Unavailable") {
            COLOR_WARNING
        } else {
            COLOR_TEXT_SECONDARY
        };
        ui.label(RichText::new(&diag.state).color(status_color));
        if !diag.backend.is_empty() {
            ui.label(
                RichText::new(&diag.backend)
                    .size(12.0 * text_scale)
                    .color(COLOR_TEXT_SECONDARY),
            );
        }
        if diag.state.starts_with("Unavailable")
            && styled_button(ui, "Retry frame generation").clicked()
        {
            output.retry_framegen = true;
        }
        if diag.active {
            for line in framegen_lines(diag) {
                ui.label(RichText::new(line).size(12.0 * text_scale).monospace());
            }
        }
    });
}

fn framegen_lines(d: &FgDiagnostics) -> Vec<String> {
    vec![
        format!(
            "Captured {:5.1}  Unique {:5.1}  Generated {:5.1}  Displayed {:5.1} FPS",
            d.captured_fps, d.unique_fps, d.generated_fps, d.displayed_fps
        ),
        format!(
            "FRUC {:5.2} ms (p95 {:5.2})   Frametime {:5.2} \u{00B1} {:4.2} ms (max {:5.2})",
            d.fruc_time.mean_ms,
            d.fruc_time.p95_ms,
            d.frametime.mean_ms,
            d.frametime.stddev_ms,
            d.frametime.max_ms
        ),
        format!(
            "Added latency \u{2248}{:4.1} ms   Duplicates {}   Dropped {}   Gaps {}",
            d.added_latency_ms, d.duplicates, d.dropped_no_slot, d.source_gaps
        ),
        format!(
            "Late {}   Transition skips {}   FRUC repeats {}   Failures {}   Hitches {}",
            d.late_generated, d.transition_skips, d.fruc_repeats, d.failures, d.hitches
        ),
        format!(
            "Present late p99 {:5.2} ms   Missed deadlines {}   Near-dup kept {}",
            d.present_lateness.p99_ms, d.missed_deadlines, d.near_duplicates_kept
        ),
        format!(
            "Frame difference {:5.1}   Camera motion {:6.1} px (residual {:.2})",
            d.last_diff_mean,
            d.motion_px[0].hypot(d.motion_px[1]),
            d.motion_residual
        ),
        match &d.correction {
            Some(c) if c.fix_failed => "Corrections: unavailable on this GPU (raw FRUC)".into(),
            Some(c) => format!(
                "Corrected area {:4.1}% now, {:4.1}% avg   GPU fix {}   capture {}",
                c.last.map(|s| s.corrected_pct).unwrap_or(0.0),
                c.mean_corrected_pct,
                c.fix_gpu
                    .as_ref()
                    .map(|s| format!("{:.2}/{:.2} ms", s.mean_ms, s.p99_ms))
                    .unwrap_or_else(|| "n/a".into()),
                c.prep_gpu
                    .as_ref()
                    .map(|s| format!("{:.2}/{:.2} ms", s.mean_ms, s.p99_ms))
                    .unwrap_or_else(|| "n/a".into()),
            ),
            None => "Corrections: off".into(),
        },
        format!(
            "Present lateness p99 {:.1} ms   missed {}   hitch causes: {}",
            d.present_lateness.p99_ms,
            d.missed_deadlines,
            if d.hitch_causes.is_empty() {
                "none".to_string()
            } else {
                d.hitch_causes
                    .iter()
                    .map(|(c, n)| format!("{c} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
    ]
}

fn draw_framegen_overlay(ctx: &egui::Context, diag: &FgDiagnostics) {
    let mut lines = vec![format!("FRAME GEN  {}", diag.state)];
    if diag.active {
        lines.extend(framegen_lines(diag));
    }
    egui::Area::new("framegen_overlay".into())
        .anchor(Align2::RIGHT_TOP, [-12.0, 12.0])
        .interactable(false)
        .movable(false)
        .show(ctx, |ui| {
            Frame::new()
                .fill(COLOR_PILL_BG)
                .stroke(Stroke::new(1.0_f32, Color32::from_white_alpha(30)))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(12, 6))
                .show(ui, |ui| {
                    for line in lines {
                        ui.add(
                            egui::Label::new(
                                RichText::new(line)
                                    .font(FontId::monospace(12.0))
                                    .color(COLOR_TEXT_PRIMARY),
                            )
                            .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    }
                });
        });
}
