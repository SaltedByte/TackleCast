#![windows_subsystem = "windows"]

mod audio;
mod capture;
mod devices;
#[cfg(feature = "gpu-decode")]
mod dx12_interop;
#[cfg(feature = "gpu-decode")]
mod gpu_decode;
#[cfg(feature = "gpu-decode")]
mod gpu_monitor;
mod logger;
mod render;
mod settings;
mod triple_buffer;
mod ui;

use std::sync::Arc;
use std::time::Instant;

use audio::AudioPassthrough;
use capture::{CaptureConfig, CaptureSource, CaptureStats, CaptureThread, PixelFormat};
use devices::AudioDevice;
use render::Renderer;
use settings::{get_capture_config, Settings};
use tracing::{error, info, warn};
use ui::{OverlayInfo, UiFrame, UiState};
use windows::core::HSTRING;
use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowAttributes};

const WINDOW_TITLE: &str = "TackleCast";
const INITIAL_WIDTH: u32 = 1280;
const INITIAL_HEIGHT: u32 = 720;
const APP_ID: &str = "tacklecast.tacklecast.v1";

/// Events sent from background threads to the winit event loop.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// The capture thread has published a new frame to the triple buffer.
    FrameReady,
}

#[derive(Clone, Copy, Debug)]
enum TestPatternMode {
    Alternate,
    Nv12,
    Yuvj422p,
}

#[derive(Debug)]
struct CliArgs {
    test_mode: bool,
    test_pattern: TestPatternMode,
}

impl CliArgs {
    fn parse() -> Self {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let has_flag = |name: &str| args.iter().any(|arg| arg.eq_ignore_ascii_case(name));

        let test_pattern = if has_flag("--test-nv12") {
            TestPatternMode::Nv12
        } else if has_flag("--test-yuvj422p") || has_flag("--test-mjpeg") {
            TestPatternMode::Yuvj422p
        } else {
            TestPatternMode::Alternate
        };

        let test_mode = has_flag("--test")
            || has_flag("--test-alt")
            || has_flag("--test-nv12")
            || has_flag("--test-yuvj422p")
            || has_flag("--test-mjpeg");

        Self {
            test_mode,
            test_pattern,
        }
    }
}

fn main() {
    if let Err(error) = logger::init_logging() {
        eprintln!("failed to initialize logging: {error}");
    }
    if let Err(error) = ffmpeg_next::init() {
        eprintln!("failed to initialize ffmpeg: {error}");
    }

    info!("====== TackleCast starting ======");
    info!("platform: {}", std::env::consts::OS);
    info!("version: {}", env!("CARGO_PKG_VERSION"));
    info!(
        "build: {}",
        if cfg!(debug_assertions) { "debug" } else { "release" }
    );
    let _ = unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID)) };

    let settings = Settings::load();
    info!("loaded settings from {}", settings::settings_path().display());
    let video_devices = devices::enumerate_video_devices();
    let audio_inputs = devices::enumerate_audio_inputs();
    let audio_outputs = devices::enumerate_audio_outputs();
    info!("video devices: {:?}", video_devices);
    info!("audio inputs: {:?}", audio_inputs);
    info!("audio outputs: {:?}", audio_outputs);

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .expect("failed to create event loop");
    let event_proxy = event_loop.create_proxy();
    let cli = CliArgs::parse();
    let mut app = App::new(
        settings,
        cli.test_mode,
        cli.test_pattern,
        video_devices,
        audio_inputs,
        audio_outputs,
        event_proxy,
    );
    event_loop.run_app(&mut app).expect("event loop error");
}

struct App {
    settings: Settings,
    test_mode: bool,
    test_pattern: TestPatternMode,
    video_devices: Vec<String>,
    audio_inputs: Vec<AudioDevice>,
    audio_outputs: Vec<AudioDevice>,
    event_proxy: EventLoopProxy<AppEvent>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    ui: Option<UiState>,
    capture: Option<CaptureThread>,
    audio: AudioPassthrough,
    latest_stats: Option<CaptureStats>,
    latest_error: Option<String>,
    is_fullscreen: bool,
    is_minimized: bool,
    is_sleep_suppressed: bool,
    is_cursor_visible: bool,
    render_frame_counter: u64,
    render_frames_uploaded: u64,
    last_render_summary: Instant,
    last_cursor_moved: Instant,
    #[cfg(feature = "gpu-decode")]
    gpu_monitor: Option<gpu_monitor::GpuMonitor>,
}

impl App {
    fn new(
        settings: Settings,
        test_mode: bool,
        test_pattern: TestPatternMode,
        video_devices: Vec<String>,
        audio_inputs: Vec<AudioDevice>,
        audio_outputs: Vec<AudioDevice>,
        event_proxy: EventLoopProxy<AppEvent>,
    ) -> Self {
        Self {
            settings,
            test_mode,
            test_pattern,
            video_devices,
            audio_inputs,
            audio_outputs,
            event_proxy,
            window: None,
            renderer: None,
            ui: None,
            capture: None,
            audio: AudioPassthrough::new(),
            latest_stats: None,
            latest_error: None,
            is_fullscreen: false,
            is_minimized: false,
            is_sleep_suppressed: false,
            is_cursor_visible: true,
            render_frame_counter: 0,
            render_frames_uploaded: 0,
            last_render_summary: Instant::now(),
            last_cursor_moved: Instant::now(),
            #[cfg(feature = "gpu-decode")]
            gpu_monitor: gpu_monitor::GpuMonitor::try_new(),
        }
    }

    fn create_window(event_loop: &ActiveEventLoop) -> Window {
        let window = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title(WINDOW_TITLE)
                    .with_resizable(true)
                    .with_inner_size(PhysicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT))
            )
            .expect("failed to create window");

        // Apply window icon from exe resources
        unsafe {
            use windows::core::PCWSTR;
            use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows::Win32::UI::WindowsAndMessaging::{
                LoadImageW, SendMessageW, HICON, IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED,
                ICON_BIG, ICON_SMALL, WM_SETICON,
            };
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

            // 1. Fetch the HWND from winit's HasWindowHandle structure
            let handle_abstract = window.window_handle().expect("Failed to read window handle");
            if let RawWindowHandle::Win32(win32_handle) = handle_abstract.as_raw() {
                let hwnd = HWND(win32_handle.hwnd.get() as *mut std::ffi::c_void);

                // 2. Fetch current module instance handle
                let h_instance = GetModuleHandleW(None).unwrap_or_default();

                // 3. Map numeric ID 1 assigned by winresource into PCWSTR
                let icon_name = PCWSTR(std::ptr::without_provenance(1));

                // 4. Load the embedded icon layout
                if let Ok(handle) = LoadImageW(
                    h_instance,
                    icon_name,
                    IMAGE_ICON,
                    0, // Allow OS to handle ideal bounds sampling
                    0,
                    LR_DEFAULTSIZE | LR_SHARED,
                ) {
                    let h_icon = HICON(handle.0);

                    if !h_icon.is_invalid() {
                        // 5. Send messages to assign to taskbar and titlebar context
                        SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_SMALL as usize), LPARAM(h_icon.0 as isize));
                        SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_BIG as usize), LPARAM(h_icon.0 as isize));
                    }
                }
            }
        }

        window
    }

    /// Sets ES_DISPLAY_REQUIRED to prevent the display from going to sleep.
    fn set_display_sleep_suppression(&mut self, should_suppress: bool) {
        if should_suppress != self.is_sleep_suppressed {
            info!(should_suppress, "updating display sleep suppression");
            self.is_sleep_suppressed = should_suppress;
            let flags = if should_suppress { ES_CONTINUOUS | ES_DISPLAY_REQUIRED } else { ES_CONTINUOUS };
            unsafe {
                SetThreadExecutionState(flags);
            }
        }
    }

    fn set_cursor_visible(&mut self, visible: bool) {
        if visible != self.is_cursor_visible {
            if let Some(window) = &self.window {
                info!(self.is_cursor_visible, "updating cursor visibility");
                self.is_cursor_visible = visible;
                window.set_cursor_visible(visible);
            }
        }
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let window = Arc::new(Self::create_window(event_loop));
        let renderer = match pollster::block_on(Renderer::new(window.clone())) {
            Ok(renderer) => renderer,
            Err(error) => {
                error!("renderer initialization failed: {error}");
                event_loop.exit();
                return;
            }
        };

        let ui = UiState::new(&window, renderer.max_texture_side());

        self.renderer = Some(renderer);
        self.ui = Some(ui);
        self.window = Some(window);

        self.start_capture();
        self.start_audio();

        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = &self.window else {
            return;
        };

        if window.id() != window_id {
            return;
        }

        if let WindowEvent::KeyboardInput { event, .. } = &event {
            if event.state == ElementState::Pressed && !event.repeat {
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        if let Some(ui) = &mut self.ui {
                            if ui.is_menu_open() {
                                if let Some(settings) = ui.close_menu_and_apply() {
                                    self.apply_settings(settings);
                                }
                            } else {
                                ui.open_menu(&self.settings);
                            }
                        }
                        if self.ui.as_ref().is_some_and(|ui| ui.is_menu_open()) {
                            self.set_cursor_visible(true);
                        }

                        return;
                    }
                    Key::Named(NamedKey::F11) => {
                        self.toggle_fullscreen();
                        return;
                    }
                    _ => {}
                }
            }
        }

        if let Some(ui) = &mut self.ui {
            if ui.is_menu_open() {
                ui.on_window_event(window, &event);
            }
        }

        match event {
            WindowEvent::CloseRequested => {
                if let Some(capture) = &mut self.capture {
                    capture.stop();
                }
                self.audio.stop();
                if let Err(error) = self.settings.save() {
                    error!("failed to save settings on exit: {error}");
                }
                event_loop.exit();
            }
            WindowEvent::CursorMoved { .. } => {
                self.last_cursor_moved = Instant::now();
                self.set_cursor_visible(true);
            }
            WindowEvent::Focused(is_focused) => self.set_display_sleep_suppression(is_focused),
            WindowEvent::Resized(size) => {
                self.is_minimized = size.width == 0 || size.height == 0;
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
                if !self.is_minimized {
                    window.request_redraw();
                }
                self.set_display_sleep_suppression(!self.is_minimized);
            }
            WindowEvent::Occluded(occluded) => {
                self.is_minimized = occluded;
                if !self.is_minimized {
                    window.request_redraw();
                }
                self.set_display_sleep_suppression(!self.is_minimized);
            }
            WindowEvent::RedrawRequested => {
                if self.is_minimized {
                    return;
                }
                let overlay = self.overlay_info();
                if let (Some(renderer), Some(ui)) = (&mut self.renderer, &mut self.ui) {
                    let prepared_ui = ui.prepare(
                        window,
                        UiFrame {
                            overlay: &overlay,
                            settings: &self.settings,
                            video_devices: &self.video_devices,
                            audio_inputs: &self.audio_inputs,
                            audio_outputs: &self.audio_outputs,
                            is_fullscreen: self.is_fullscreen,
                        },
                    );
                    let toggle_fullscreen = prepared_ui.output.toggle_fullscreen;
                    let exit_requested = prepared_ui.output.exit_requested;
                    let apply_settings = prepared_ui.output.apply_settings.clone();
                    if let Err(error) = renderer.render(Some(prepared_ui)) {
                        error!("render error: {error}");
                    }
                    self.render_frame_counter += 1;
                    if toggle_fullscreen {
                        self.toggle_fullscreen();
                    }
                    if let Some(settings) = apply_settings {
                        self.apply_settings(settings);
                    }
                    if exit_requested {
                        if let Some(capture) = &mut self.capture {
                            capture.stop();
                        }
                        self.audio.stop();
                        if let Err(error) = self.settings.save() {
                            error!("failed to save settings on exit: {error}");
                        }
                        event_loop.exit();
                    }
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::FrameReady => {
                // Skip frame processing entirely when the window isn't visible.
                // Without a render pass to drain them, queued write_texture calls
                // accumulate in GPU memory indefinitely (display off, minimized, etc).
                if self.is_minimized {
                    // Still drain the triple buffer so it doesn't hold stale frames
                    if let Some(capture) = &mut self.capture {
                        let _ = capture.latest_frame();
                    }
                    return;
                }

                let mut uploaded = false;
                if let Some(capture) = &mut self.capture {
                    if let Some(frame) = capture.latest_frame() {
                        if let Some(renderer) = &mut self.renderer {
                            renderer.upload_frame(&frame);
                            uploaded = true;
                        }
                    }
                }
                if uploaded {
                    self.latest_error = None;
                    self.render_frames_uploaded += 1;
                }

                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Poll non-frame channels (stats, errors, negotiation).
        // These are low-frequency and don't need event-driven wakeup.
        if let Some(capture) = &mut self.capture {
            if let Some(stats) = capture.latest_stats() {
                self.latest_stats = Some(stats);
            }

            if let Some(error_message) = capture.latest_error() {
                self.latest_error = Some(error_message.clone());
                warn!("capture error: {error_message}");
            }

            // If the capture thread fell back to a different resolution/fps,
            // update settings to reflect what's actually running.
            if let Some(negotiated) = capture.latest_negotiated() {
                info!(
                    "updating settings to match negotiated capture: {}x{} @ {}fps",
                    negotiated.width, negotiated.height, negotiated.fps
                );
                self.settings.apply_negotiated(
                    negotiated.width,
                    negotiated.height,
                    negotiated.fps,
                );
                if let Err(error) = self.settings.save() {
                    error!("failed to save negotiated settings: {error}");
                }
            }
        }

        // Periodic render summary (every 30s)
        let render_elapsed = self.last_render_summary.elapsed();
        if render_elapsed >= std::time::Duration::from_secs(30) {
            let render_fps = self.render_frame_counter as f64 / render_elapsed.as_secs_f64();
            let upload_fps = self.render_frames_uploaded as f64 / render_elapsed.as_secs_f64();

            #[cfg(feature = "gpu-decode")]
            let gpu_info = self.gpu_monitor.as_ref().map(|m| format!(", {}", m.snapshot()));
            #[cfg(not(feature = "gpu-decode"))]
            let gpu_info: Option<String> = None;

            info!(
                "render summary: {:.1} rendered fps, {:.1} uploaded fps, decode fps={:.1}{}",
                render_fps,
                upload_fps,
                self.latest_stats.map(|s| s.fps).unwrap_or(0.0),
                gpu_info.as_deref().unwrap_or(""),
            );
            self.render_frame_counter = 0;
            self.render_frames_uploaded = 0;
            self.last_render_summary = Instant::now();
        }

        // Hide cursor after 3s of inactivity if the ui menu is closed
        let cursor_idle_elapsed = self.last_cursor_moved.elapsed();
        if self.is_cursor_visible && cursor_idle_elapsed >= std::time::Duration::from_secs(3) && self.ui.as_ref().is_none_or(|ui| !ui.is_menu_open()) {
            self.set_cursor_visible(false);
        }
    }
}

impl App {
    fn apply_settings(&mut self, new_settings: Settings) {
        let old_settings = self.settings.clone();
        self.settings = new_settings;

        if let Err(error) = self.settings.save() {
            error!("failed to save settings: {error}");
        }

        let video_changed = old_settings.video_device != self.settings.video_device
            || old_settings.resolution != self.settings.resolution
            || old_settings.fps_mode != self.settings.fps_mode
            || old_settings.custom_fps != self.settings.custom_fps;

        let audio_device_changed = old_settings.video_device != self.settings.video_device
            || old_settings.audio_input != self.settings.audio_input
            || old_settings.audio_output != self.settings.audio_output;

        let scaling_changed = old_settings.scaling_filter != self.settings.scaling_filter;

        if video_changed {
            if let Some(capture) = &mut self.capture {
                capture.stop();
            }
            self.start_capture();
        }

        if audio_device_changed {
            self.audio.stop();
            self.start_audio();
        } else if (old_settings.volume - self.settings.volume).abs() > f64::EPSILON {
            self.audio.set_volume(self.settings.volume);
        }

        if scaling_changed {
            if let Some(renderer) = &mut self.renderer {
                renderer.set_scale_filter(self.settings.scaling_filter);
            }
        }
    }

    fn overlay_info(&self) -> OverlayInfo {
        let status_message = if let Some(error_message) = self.latest_error.as_ref() {
            Some(error_message.clone())
        } else if self.latest_stats.is_none() {
            Some("Connecting...".to_string())
        } else {
            None
        };

        OverlayInfo {
            width: self.latest_stats.map(|stats| stats.width),
            height: self.latest_stats.map(|stats| stats.height),
            fps: self.latest_stats.map(|stats| stats.fps),
            show_overlay: self.settings.show_overlay && !self.is_minimized,
            filter: self.settings.scaling_filter,
            status_message,
            status_is_alert: self.latest_error.is_some() || self.latest_stats.is_none(),
        }
    }

    fn toggle_fullscreen(&mut self) {
        self.is_fullscreen = !self.is_fullscreen;
        if let Some(window) = &self.window {
            if self.is_fullscreen {
                window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            } else {
                window.set_fullscreen(None);
            }
            self.set_display_sleep_suppression(self.is_fullscreen);
        }
    }

    fn start_capture(&mut self) {
        self.latest_stats = None;
        self.latest_error = None;
        let capture_config = get_capture_config(&self.settings.resolution, self.settings.get_fps());
        if self.test_mode {
            self.start_test_capture(
                capture_config.width,
                capture_config.height,
                capture_config.fps,
                "CLI test mode enabled",
            );
            return;
        }

        let video_device = if self.settings.video_device.is_empty() {
            self.video_devices.first().cloned().unwrap_or_default()
        } else {
            self.settings.video_device.clone()
        };

        if video_device.is_empty() {
            warn!("no video device configured or discovered");
            self.start_test_capture(
                capture_config.width,
                capture_config.height,
                capture_config.fps,
                "No video device discovered, using test pattern fallback",
            );
            return;
        }

        // Try to initialize shared DX12 buffers for zero-copy GPU decode
        #[cfg(feature = "gpu-decode")]
        let shared_gpu_handles = if let Some(renderer) = &mut self.renderer {
            renderer.try_init_shared_buffers(capture_config.width, capture_config.height)
        } else {
            None
        };

        info!(
            "starting DirectShow capture: device='{}' {}x{} @ {}fps ({}, threads={})",
            video_device,
            capture_config.width,
            capture_config.height,
            capture_config.fps,
            capture_config.pixel_format,
            capture_config.decode_threads
        );
        self.capture = Some(CaptureThread::start(CaptureConfig {
            width: capture_config.width,
            height: capture_config.height,
            fps: capture_config.fps,
            source: CaptureSource::DirectShow {
                device_name: video_device,
                pixel_format: capture_config.pixel_format.to_string(),
                decode_threads: capture_config.decode_threads,
                #[cfg(feature = "gpu-decode")]
                shared_gpu_handles,
            },
        }, self.event_proxy.clone()));
    }

    fn start_test_capture(&mut self, width: u32, height: u32, fps: u32, reason: &str) {
        let (alternate_formats, force_format) = match self.test_pattern {
            TestPatternMode::Alternate => (true, None),
            TestPatternMode::Nv12 => (false, Some(PixelFormat::Nv12)),
            TestPatternMode::Yuvj422p => (false, Some(PixelFormat::Yuvj422p)),
        };

        info!("starting test-pattern capture: {}x{} @ {}fps ({reason})", width, height, fps);
        self.capture = Some(CaptureThread::start(CaptureConfig {
            width,
            height,
            fps,
            source: CaptureSource::TestPattern {
                alternate_formats,
                force_format,
            },
        }, self.event_proxy.clone()));
    }

    fn start_audio(&mut self) {
        if self.test_mode {
            return;
        }

        if self.settings.video_device.is_empty() && self.video_devices.is_empty() {
            info!("skipping audio passthrough because no video capture device is available");
            return;
        }

        let video_device = if self.settings.video_device.is_empty() {
            self.video_devices.first().cloned().unwrap_or_default()
        } else {
            self.settings.video_device.clone()
        };

        self.audio.start(
            &video_device,
            self.settings.audio_input,
            self.settings.audio_output,
            self.settings.volume,
        );
    }
}
