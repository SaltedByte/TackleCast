use crate::capabilities;
use crate::color::ColorInfo;
use crate::devices::AudioDevice;
use crate::frame_rate::FrameRate;
use crate::settings::{ColorRange, ColorSpace, ScaleFilter, Settings, VideoFormat};
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
    Display,
}

pub struct UiState {
    egui_ctx: egui::Context,
    egui_winit: State,
    menu_open: bool,
    draft_settings: Settings,
    tab: MenuTab,
    edid: EdidUi,
}

/// Menu-only state for the Elgato sections. Not saved: the card itself stores
/// its EDID and settings.
#[derive(Default)]
struct EdidUi {
    device: String,
    choice: Option<EdidChoice>,
    confirming: bool,
    /// Why the last picked file was rejected.
    file_error: Option<String>,
    /// Line-in gain while the slider is being dragged.
    gain_draft: Option<i64>,
    /// An Input EDID mode waiting for confirmation: changing it makes the
    /// console renegotiate HDMI, which can blank the picture.
    pending_edid_mode: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EdidChoice {
    Profile(String),
    File(std::path::PathBuf),
}

#[derive(Clone)]
pub struct OverlayInfo {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f32>,
    pub filter: ScaleFilter,
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
    pub live_video: Option<Settings>,
    pub slider_active: bool,
    pub toggle_fullscreen: bool,
    pub exit_requested: bool,
    pub repaint_after: Duration,
    /// Make this change on an Elgato card: (video device ID, change).
    pub elgato_job: Option<(String, crate::elgato_edid::Job)>,
    /// Open the file picker for an EDID file.
    pub pick_edid_file: bool,
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
            edid: EdidUi::default(),
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
        self.edid = EdidUi::default();
        crate::elgato_edid::refresh(&settings.video_device);
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
                    &mut self.edid,
                    &frame,
                    &mut ui_output,
                );
            }
            let mut overlay = frame.overlay.clone();
            if self.menu_open {
                overlay.color_space = self.draft_settings.color_space;
                overlay.color_range = self.draft_settings.color_range;
                overlay.filter = self.draft_settings.scaling_filter;
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

fn draw_overlay(ctx: &egui::Context, overlay: &OverlayInfo) {
    let text = overlay_text(overlay);
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
    edid: &mut EdidUi,
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
                                        if let Some(view) =
                                            crate::elgato_edid::view(&draft.video_device)
                                        {
                                            elgato_sections(
                                                ui,
                                                ctx,
                                                view,
                                                &draft.video_device,
                                                edid,
                                                output,
                                                text_scale,
                                            );
                                        }
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
                                            labeled_audio_combo(
                                                ui,
                                                "Input Device",
                                                &mut draft.audio_input,
                                                frame.audio_inputs,
                                                "Auto (follow video)",
                                            );
                                            labeled_audio_combo(
                                                ui,
                                                "Output Device",
                                                &mut draft.audio_output,
                                                frame.audio_outputs,
                                                "Default system output",
                                            );
                                            ui.label("Auto uses matching video-device audio; otherwise audio stays disconnected.");
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
                                        });
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

fn muted_text(ui: &mut egui::Ui, text: &str, text_scale: f32) {
    ui.label(
        RichText::new(text)
            .size(12.0 * text_scale)
            .color(COLOR_TEXT_MUTED),
    );
}

fn edid_profile_label(profile: &crate::elgato_edid::Profile) -> String {
    if profile.kind == "default" {
        format!("{} (default, PS5)", profile.name)
    } else {
        profile.name.clone()
    }
}

fn edid_choice_label(choice: &EdidChoice, status: &crate::elgato_edid::Status) -> String {
    match choice {
        EdidChoice::Profile(name) => status
            .profiles
            .iter()
            .find(|p| &p.name == name)
            .map(edid_profile_label)
            .unwrap_or_else(|| name.clone()),
        EdidChoice::File(path) => {
            let name = status
                .saved_files
                .iter()
                .find(|f| &f.path == path)
                .map(|f| f.name.clone())
                .unwrap_or_else(|| {
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                });
            format!("File: {name}")
        }
    }
}

/// A combo box for one card setting. Returns the newly picked value.
fn card_setting_combo(
    ui: &mut egui::Ui,
    label: &str,
    choice: crate::elgato_edid::Choice,
    options: &[(i32, &'static str)],
) -> Option<i32> {
    use crate::elgato_edid::label_for;
    let mut picked = None;
    ui.label(RichText::new(label).color(COLOR_TEXT_SECONDARY));
    ComboBox::from_id_salt(("elgato_setting", label))
        .width(ui.available_width())
        .selected_text(label_for(options, choice.value))
        .show_ui(ui, |ui| {
            for (value, name) in options {
                let text = if choice.default == Some(*value) {
                    format!("{name} (default)")
                } else {
                    (*name).to_string()
                };
                if ui.selectable_label(choice.value == *value, text).clicked()
                    && choice.value != *value
                {
                    picked = Some(*value);
                }
            }
        });
    picked
}

/// The Elgato 4K X cards: the card's own settings, applied as soon as they
/// change, and its EDID, which takes two clicks because writing it re-flashes
/// the card and takes it offline for about 20 s.
#[allow(clippy::too_many_arguments)]
fn elgato_sections(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    view: crate::elgato_edid::View,
    device: &str,
    edid: &mut EdidUi,
    output: &mut UiOutput,
    text_scale: f32,
) {
    use crate::elgato_edid::{self as elgato, Job, SettingChange, View};
    if edid.device != device {
        *edid = EdidUi {
            device: device.to_string(),
            ..EdidUi::default()
        };
    }
    if let Some(path) = elgato::take_picked_file() {
        edid.file_error = std::fs::read(&path)
            .map_err(|e| format!("Cannot read {}: {e}", path.display()))
            .and_then(|bytes| elgato::validate_edid(&bytes))
            .err();
        if edid.file_error.is_none() {
            edid.choice = Some(EdidChoice::File(path));
            edid.confirming = false;
        }
    }

    let status = match view {
        View::Ready(status) => status,
        other => {
            card_section(ui, "ELGATO 4K X", text_scale, |ui| match other {
                View::Loading => {
                    muted_text(ui, "Reading the card's settings…", text_scale);
                    ctx.request_repaint_after(Duration::from_millis(200));
                }
                View::Busy(busy) if busy.pauses_capture => {
                    warning_text(
                        ui,
                        &format!(
                            "Writing {} to the card ({}s). Video returns when it reconnects. Don't unplug the card.",
                            busy.label,
                            busy.elapsed.as_secs()
                        ),
                        text_scale,
                    );
                    ctx.request_repaint_after(Duration::from_millis(250));
                }
                View::Busy(busy) if busy.label == Job::DownloadLibrary.describe() => {
                    muted_text(
                        ui,
                        &format!(
                            "Downloading Elgato's device library from Elgato ({}s)…",
                            busy.elapsed.as_secs()
                        ),
                        text_scale,
                    );
                    ctx.request_repaint_after(Duration::from_millis(250));
                }
                View::Busy(busy) => {
                    muted_text(ui, &format!("Applying {}…", busy.label), text_scale);
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                View::Error(message) => match elgato::library_missing(&message) {
                    Some(reason) => {
                        warning_text(ui, reason, text_scale);
                        muted_text(
                            ui,
                            "TackleCast changes the 4K X through Elgato's own device library, which comes with Elgato Studio. It can download just that library from Elgato instead (about 5 MB, signed by Corsair). Nothing is installed, and it updates itself afterwards.",
                            text_scale,
                        );
                        if styled_button(ui, "Download Elgato's device library").clicked() {
                            output.elgato_job = Some((device.to_string(), Job::DownloadLibrary));
                        }
                    }
                    None => {
                        warning_text(ui, &message, text_scale);
                        if ui.small_button("Retry").clicked() {
                            elgato::refresh(device);
                            ctx.request_repaint();
                        }
                    }
                },
                View::Ready(_) => {}
            });
            return;
        }
    };
    let settings = &status.settings;
    let can_change = !status.studio_running;
    let mut job = None;

    card_section(ui, "ELGATO 4K X", text_scale, |ui| {
        if status.studio_running {
            warning_text(
                ui,
                "Close Elgato Studio to change these settings; it holds the card.",
                text_scale,
            );
        }
        match elgato::notice(device) {
            Some(Ok(message)) => muted_text(ui, &message, text_scale),
            Some(Err(message)) => warning_text(ui, &message, text_scale),
            None => {}
        }
        let mut input = Vec::new();
        if let Some(signal) = status.signal {
            if !signal.has_signal {
                warning_text(
                    ui,
                    "No HDMI signal from the source. If it went black after an EDID change, undock and redock the console (or replug its HDMI cable), then Refresh.",
                    text_scale,
                );
            } else if signal.width > 0 && signal.height > 0 {
                let rate = if (signal.frame_rate - signal.frame_rate.round()).abs() < 0.005 {
                    format!("{:.0}", signal.frame_rate)
                } else {
                    format!("{:.2}", signal.frame_rate)
                };
                input.push(format!("{}×{} at {rate} Hz", signal.width, signal.height));
            }
            if signal.hdcp {
                input.push("HDCP-protected".to_string());
            }
        }
        if let Some(hdr) = settings.video_hdr {
            input.push(if hdr { "HDR" } else { "SDR" }.to_string());
        }
        if let Some(space) = settings.input_color_space {
            input.push(elgato::label_for(&elgato::INPUT_COLOR_SPACES, space));
        }
        if !input.is_empty() {
            muted_text(ui, &format!("HDMI input: {}", input.join(", ")), text_scale);
        }
        ui.add_enabled_ui(can_change, |ui| {
            if let Some(choice) = settings.color_range {
                if let Some(v) =
                    card_setting_combo(ui, "HDMI color range", choice, &elgato::COLOR_RANGES)
                {
                    job = Some(Job::Setting(SettingChange::ColorRange(v)));
                }
            }
            if let Some(choice) = settings.edid_mode {
                if let Some(v) =
                    card_setting_combo(ui, "Input EDID mode", choice, &elgato::EDID_MODES)
                {
                    edid.pending_edid_mode = Some(v);
                }
                if let Some(v) = edid.pending_edid_mode.filter(|v| *v != choice.value) {
                    warning_text(
                        ui,
                        &format!(
                            "Switch to {}? The console renegotiates HDMI, and the picture on TackleCast and the pass-through monitor can go black. If it does, undock and redock the console (or replug its HDMI cable).",
                            elgato::label_for(&elgato::EDID_MODES, v)
                        ),
                        text_scale,
                    );
                    ui.horizontal(|ui| {
                        if styled_button(ui, "Change mode").clicked() {
                            job = Some(Job::Setting(SettingChange::EdidMode(v)));
                            edid.pending_edid_mode = None;
                        }
                        if styled_button(ui, "Cancel").clicked() {
                            edid.pending_edid_mode = None;
                        }
                    });
                } else {
                    edid.pending_edid_mode = None;
                }
            }
            if let Some(choice) = settings.audio_input {
                if let Some(v) =
                    card_setting_combo(ui, "Audio input", choice, &elgato::AUDIO_INPUTS)
                {
                    job = Some(Job::Setting(SettingChange::AudioInput(v)));
                }
            }
            if let Some(gain) = settings.line_in_gain {
                ui.label(RichText::new("Line-in gain").color(COLOR_TEXT_SECONDARY));
                let value = edid.gain_draft.get_or_insert(gain.value);
                let response = ui.add(Slider::new(value, gain.min..=gain.max));
                let released =
                    response.drag_stopped() || (response.changed() && !response.dragged());
                if released && *value != gain.value {
                    job = Some(Job::Setting(SettingChange::LineInGain(*value)));
                }
            }
            if let Some(toggle) = settings.hdr_tonemapping {
                let mut on = toggle.value;
                if ui
                    .checkbox(&mut on, "HDR tone mapping")
                    .on_hover_text("Convert HDR input to SDR on the card")
                    .changed()
                {
                    job = Some(Job::Setting(SettingChange::HdrTonemapping(on)));
                }
            }
        });
        if ui.small_button("Refresh").clicked() {
            edid.gain_draft = None;
            elgato::refresh(device);
            ctx.request_repaint();
        }
    });

    card_section(ui, "ELGATO EDID", text_scale, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Active:").color(COLOR_TEXT_SECONDARY));
            ui.label(RichText::new(edid_profile_label(&status.active)).strong());
        });
        if let Some(note) = &status.asset_note {
            muted_text(ui, note, text_scale);
        }
        if let Some(library) = &status.library {
            muted_text(ui, &format!("Elgato library: {library}"), text_scale);
        }
        if settings
            .edid_mode
            .is_some_and(|m| m.value != elgato::EDID_MODE_INTERNAL)
        {
            muted_text(
                ui,
                "Input EDID mode isn't Internal, so with a pass-through monitor connected the console may see the monitor's EDID instead of this one.",
                text_scale,
            );
        }

        let active = EdidChoice::Profile(status.active.name.clone());
        let valid = match &edid.choice {
            Some(EdidChoice::Profile(name)) => status.profiles.iter().any(|p| &p.name == name),
            Some(EdidChoice::File(_)) => true,
            None => false,
        };
        if !valid {
            edid.choice = Some(active.clone());
        }
        let before = edid.choice.clone();
        let choice = edid.choice.get_or_insert(active.clone());
        ui.label(RichText::new("EDID").color(COLOR_TEXT_SECONDARY));
        ComboBox::from_id_salt("elgato_edid_choice")
            .width(ui.available_width())
            .selected_text(edid_choice_label(choice, &status))
            .show_ui(ui, |ui| {
                for profile in &status.profiles {
                    let mut label = edid_profile_label(profile);
                    if !profile.available {
                        label.push_str(" (not downloaded)");
                    }
                    ui.add_enabled_ui(profile.available, |ui| {
                        ui.selectable_value(
                            choice,
                            EdidChoice::Profile(profile.name.clone()),
                            label,
                        );
                    });
                }
                if !status.saved_files.is_empty() {
                    ui.separator();
                }
                for file in &status.saved_files {
                    ui.selectable_value(
                        choice,
                        EdidChoice::File(file.path.clone()),
                        format!("File: {}", file.name),
                    );
                }
                if let EdidChoice::File(path) = choice {
                    if !status.saved_files.iter().any(|f| &f.path == path) {
                        // The picked file, already selected.
                        let label = edid_choice_label(choice, &status);
                        let _ = ui.selectable_label(true, label);
                    }
                }
            });
        if edid.choice != before {
            edid.confirming = false;
            edid.file_error = None;
        }
        if let Some(error) = &edid.file_error {
            warning_text(ui, error, text_scale);
        }

        let changed = edid.choice.as_ref() != Some(&active);
        let can_write = changed && can_change;
        if edid.confirming && can_write {
            warning_text(
                ui,
                "The card is re-flashed and disconnects for about 20 s, so video and audio stop. Don't unplug it until video returns. If the picture stays black afterwards, undock and redock the console.",
                text_scale,
            );
            ui.horizontal(|ui| {
                if styled_button(ui, "Write EDID").clicked() {
                    job = match edid.choice.clone() {
                        Some(EdidChoice::Profile(name)) => Some(Job::Profile(name)),
                        Some(EdidChoice::File(path)) => Some(Job::File(path)),
                        None => None,
                    };
                    edid.confirming = false;
                }
                if styled_button(ui, "Cancel").clicked() {
                    edid.confirming = false;
                }
            });
        } else {
            ui.horizontal(|ui| {
                let apply = ui
                    .add_enabled_ui(can_write, |ui| styled_button(ui, "Apply EDID…"))
                    .inner;
                if apply.clicked() {
                    edid.confirming = true;
                }
                if ui
                    .add_enabled_ui(can_change, |ui| styled_button(ui, "Load EDID file…"))
                    .inner
                    .clicked()
                {
                    output.pick_edid_file = true;
                }
            });
        }
    });

    if let Some(job) = job {
        if matches!(job, Job::Setting(SettingChange::LineInGain(_))) {
            edid.gain_draft = None;
        }
        output.elgato_job = Some((device.to_string(), job));
    }
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
    default_label: &str,
) {
    ui.label(RichText::new(label).color(COLOR_TEXT_SECONDARY));
    ComboBox::from_id_salt(label)
        .width(ui.available_width())
        .selected_text(audio_device_name(*selected, devices, default_label))
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, -1, default_label);
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

fn audio_device_name(index: i32, devices: &[AudioDevice], default_label: &str) -> String {
    if index < 0 {
        return default_label.to_string();
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
                    overlay.filter,
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
