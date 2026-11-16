#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod audio;
mod capabilities;
mod capture;
mod color;
mod devices;
#[cfg(all(target_os = "windows", feature = "gpu-decode"))]
mod dx12_interop;
#[cfg(target_os = "windows")]
mod dxgi_compat;
mod frame_rate;
#[cfg(all(target_os = "windows", feature = "gpu-decode"))]
mod gpu_decode;
#[cfg(all(target_os = "windows", feature = "gpu-decode"))]
mod gpu_monitor;
mod logger;
mod overlay;
mod recovery;
mod render;
mod settings;
mod theme;
mod triple_buffer;
mod ui;
#[cfg(target_os = "windows")]
mod vsr_interop;

use std::sync::Arc;
use std::time::{Duration, Instant};

use audio::AudioPassthrough;
use capture::{CaptureConfig, CaptureSource, CaptureStats, CaptureThread, PixelFormat};
use devices::AudioDevice;
use render::Renderer;
use settings::{get_capture_config, Settings};
use tracing::{error, info, warn};
use ui::{OverlayInfo, UiFrame, UiState};
#[cfg(target_os = "windows")]
use windows::core::HSTRING;
#[cfg(target_os = "windows")]
use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED};
#[cfg(target_os = "windows")]
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowAttributes};

const WINDOW_TITLE: &str = "TackleCast VI";
const INITIAL_WIDTH: u32 = 1280;
const INITIAL_HEIGHT: u32 = 720;
#[cfg(target_os = "windows")]
const APP_ID: &str = "tacklecast.tacklecast.v1";

/// How often the event loop wakes when no frames are arriving, to poll the
/// stats/error channels and run the cursor-idle and render-summary timers.
/// Frames drive redraws through `AppEvent::FrameReady` and don't wait for this.
const HOUSEKEEPING_TICK: Duration = Duration::from_millis(100);

/// A capture counts as live while frames have arrived this recently. Used to
/// decide whether to hold the display awake.
const CAPTURE_LIVE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum AudioRestart {
    Previous,
    Selected,
}

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
    preview_page: Option<ui::PreviewPage>,
    preview_theme: Option<settings::Theme>,
}

impl CliArgs {
    fn parse() -> Self {
        Self::parse_from(std::env::args().skip(1))
    }

    fn preview_active(&self) -> bool {
        self.preview_page.is_some() || self.preview_theme.is_some()
    }

    fn parse_from(args: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let args: Vec<String> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_ascii_lowercase())
            .collect();
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

        let mut preview_page = None;
        let mut preview_theme = None;
        if test_mode {
            for arg in &args {
                if let Some(value) = arg.strip_prefix("--test-ui=") {
                    preview_page = match value {
                        "video" => Some(ui::PreviewPage::Video),
                        "picture" => Some(ui::PreviewPage::Picture),
                        "audio" => Some(ui::PreviewPage::Audio),
                        "display" => Some(ui::PreviewPage::Display),
                        "overlay" => Some(ui::PreviewPage::Overlay),
                        "none" => Some(ui::PreviewPage::None),
                        _ => None,
                    };
                }
                if let Some(value) = arg.strip_prefix("--test-theme=") {
                    preview_theme = match value {
                        "classic" => Some(settings::Theme::Classic),
                        "vice" => Some(settings::Theme::Vice),
                        _ => None,
                    };
                }
            }
        }
        Self {
            test_mode,
            test_pattern,
            preview_page,
            preview_theme,
        }
    }
}

fn main() {
    #[cfg(target_os = "windows")]
    if capabilities::handle_probe_args() {
        return;
    }
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
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    #[cfg(target_os = "windows")]
    let _ = unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID)) };

    let cli = CliArgs::parse();
    let mut settings = Settings::load();
    if let Some(theme) = cli.preview_theme {
        settings.theme = theme;
    }
    info!(
        "loaded settings from {}",
        settings::settings_path().display()
    );
    let video_devices = if cli.preview_active() {
        Vec::new()
    } else {
        devices::enumerate_video_devices()
    };
    if cli.preview_active() {
        // Do not query saved hardware capabilities while drawing the Video page.
        settings.video_device.clear();
    }
    settings.video_device = devices::restore_video_device(&settings.video_device, &video_devices);
    let mode_query = std::env::args()
        .find(|arg| arg == "--list-capture-modes" || arg.starts_with("--list-capture-modes="));
    if let Some(query) = mode_query {
        let selected = query.split_once('=').map(|(_, value)| value);
        for device in &video_devices {
            if selected.is_some_and(|value| {
                device != value && devices::video_device_label(device) != value
            }) {
                continue;
            }
            let caps = capabilities::query_device_capabilities(device);
            println!(
                "Device: {} [{}]",
                devices::video_device_label(device),
                device
            );
            for mode in caps.modes {
                println!(
                    "  {}x{} | {} | {} FPS ({})",
                    mode.width,
                    mode.height,
                    mode.format,
                    mode.rate,
                    mode.rate.key()
                );
            }
            if let Some(message) = caps.message {
                println!("  {message}");
            }
        }
        return;
    }
    if !settings.video_device.is_empty() && !std::env::args().any(|arg| arg.starts_with("--test")) {
        capabilities::prime_device_capabilities(&settings.video_device);
    }
    let audio_inputs = if cli.preview_active() {
        Vec::new()
    } else {
        devices::enumerate_audio_inputs()
    };
    let audio_outputs = if cli.preview_active() {
        Vec::new()
    } else {
        devices::enumerate_audio_outputs()
    };

    info!("=== Audio & Video Diagnostics at Startup ===");
    info!("detected video capture devices ({}):", video_devices.len());
    for (i, dev) in video_devices.iter().enumerate() {
        info!("  [{i}] {dev}");
    }
    info!("detected audio input devices ({}):", audio_inputs.len());
    for dev in &audio_inputs {
        info!("  [{}] {}", dev.index, dev.name);
    }
    info!("detected audio output devices ({}):", audio_outputs.len());
    for dev in &audio_outputs {
        info!("  [{}] {}", dev.index, dev.name);
    }
    info!(
        "initial settings: video='{}', audio_in={}, audio_out={}, vol={:.0}%, muted={}",
        settings.video_device,
        settings.audio_input,
        settings.audio_output,
        settings.volume,
        settings.audio_muted
    );
    info!("============================================");

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .expect("failed to create event loop");
    let event_proxy = event_loop.create_proxy();
    let mut app = App::new(
        settings,
        cli.test_mode,
        cli.test_pattern,
        video_devices,
        audio_inputs,
        audio_outputs,
        event_proxy,
    );
    app.preview_mode = cli.preview_active();
    app.preview_page = cli.preview_page;
    event_loop.run_app(&mut app).expect("event loop error");
}

struct App {
    settings: Settings,
    preview_mode: bool,
    preview_page: Option<ui::PreviewPage>,
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
    capture_restart_pending: bool,
    audio_restart_pending: Option<AudioRestart>,
    capture_recovery: recovery::RecoveryBackoff,
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
    last_frame_at: Option<Instant>,
    next_ui_redraw: Option<Instant>,
    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
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
            preview_mode: false,
            preview_page: None,
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
            capture_restart_pending: false,
            audio_restart_pending: None,
            capture_recovery: recovery::RecoveryBackoff::default(),
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
            last_frame_at: None,
            next_ui_redraw: None,
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            gpu_monitor: gpu_monitor::GpuMonitor::try_new(),
        }
    }

    fn create_window(event_loop: &ActiveEventLoop) -> Window {
        let window = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title(WINDOW_TITLE)
                    .with_resizable(true)
                    .with_inner_size(PhysicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT)),
            )
            .expect("failed to create window");

        // Apply window icon from exe resources
        #[cfg(target_os = "windows")]
        unsafe {
            use windows::core::PCWSTR;
            use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows::Win32::UI::WindowsAndMessaging::{
                LoadImageW, SendMessageW, HICON, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTSIZE,
                LR_SHARED, WM_SETICON,
            };
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

            // 1. Fetch the HWND from winit's HasWindowHandle structure
            let handle_abstract = window
                .window_handle()
                .expect("Failed to read window handle");
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
                        SendMessageW(
                            hwnd,
                            WM_SETICON,
                            WPARAM(ICON_SMALL as usize),
                            LPARAM(h_icon.0 as isize),
                        );
                        SendMessageW(
                            hwnd,
                            WM_SETICON,
                            WPARAM(ICON_BIG as usize),
                            LPARAM(h_icon.0 as isize),
                        );
                    }
                }
            }
        }

        window
    }

    /// Recomputes whether the display should be held awake and pushes the
    /// result to Windows via ES_DISPLAY_REQUIRED.
    ///
    /// The predicate is "frames are still arriving and the window is visible".
    /// Deliberately not focus-based: watching a console on a second monitor
    /// while working in another window is the normal case for this app, and
    /// the screen shouldn't sleep underneath it. Frames stopping — capture card
    /// unplugged, console powered off — does let the display sleep again.
    ///
    /// Cheap to call often; it early-returns unless the answer changed.
    fn update_sleep_suppression(&mut self) {
        let capture_live = self
            .last_frame_at
            .is_some_and(|at| at.elapsed() < CAPTURE_LIVE_TIMEOUT);
        let should_suppress = capture_live && !self.is_minimized;

        if should_suppress == self.is_sleep_suppressed {
            return;
        }

        info!(should_suppress, "updating display sleep suppression");
        self.is_sleep_suppressed = should_suppress;
        #[cfg(target_os = "windows")]
        {
            let flags = if should_suppress {
                ES_CONTINUOUS | ES_DISPLAY_REQUIRED
            } else {
                ES_CONTINUOUS
            };
            unsafe {
                SetThreadExecutionState(flags);
            }
        }
    }

    fn set_cursor_visible(&mut self, visible: bool) {
        if visible == self.is_cursor_visible {
            return;
        }
        if let Some(window) = &self.window {
            info!(visible, "updating cursor visibility");
            self.is_cursor_visible = visible;
            window.set_cursor_visible(visible);
        }
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let window = Arc::new(Self::create_window(event_loop));
        let renderer = match pollster::block_on(Renderer::new(
            window.clone(),
            self.settings.scaling_filter,
            self.settings.sharpness,
            self.settings.color_space,
            self.settings.color_range,
            self.settings.brightness,
            self.settings.contrast,
            self.settings.saturation,
            self.settings.gamma,
        )) {
            Ok(renderer) => renderer,
            Err(error) => {
                error!("renderer initialization failed: {error}");
                event_loop.exit();
                return;
            }
        };

        let mut ui = UiState::new(&window, renderer.max_texture_side());
        if let Some(page) = self.preview_page.take() {
            ui.open_preview(&self.settings, page);
        }

        self.renderer = Some(renderer);
        self.ui = Some(ui);
        self.window = Some(window);

        self.start_audio();
        self.start_capture();

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
        let Some(window) = self.window.clone() else {
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

        // Forward every event, not just while the menu is open: egui also
        // tracks DPI, surface size, modifiers, and pointer position from these,
        // and goes stale (wrong scale factor after a monitor change, unknown
        // pointer position) if it only sees them some of the time.
        if let Some(ui) = &mut self.ui {
            if ui.on_window_event(&window, &event) {
                window.request_redraw();
            }
        }

        match event {
            WindowEvent::CloseRequested => {
                if let Some(capture) = &mut self.capture {
                    capture.stop();
                }
                self.audio.stop();
                self.save_settings();
                event_loop.exit();
            }
            WindowEvent::CursorMoved { .. } => {
                self.last_cursor_moved = Instant::now();
                self.set_cursor_visible(true);
            }
            WindowEvent::Resized(size) => {
                self.is_minimized = size.width == 0 || size.height == 0;
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
                if !self.is_minimized {
                    window.request_redraw();
                }
                self.update_sleep_suppression();
            }
            WindowEvent::Occluded(occluded) => {
                self.is_minimized = occluded;
                if !self.is_minimized {
                    window.request_redraw();
                }
                self.update_sleep_suppression();
            }
            WindowEvent::RedrawRequested => {
                if self.is_minimized {
                    return;
                }
                let overlay = self.overlay_info();
                let audio_status = self.audio.status().to_string();
                if let (Some(renderer), Some(ui)) = (&mut self.renderer, &mut self.ui) {
                    let prepared_ui = ui.prepare(
                        &window,
                        UiFrame {
                            overlay: &overlay,
                            settings: &self.settings,
                            video_devices: &self.video_devices,
                            audio_inputs: &self.audio_inputs,
                            audio_outputs: &self.audio_outputs,
                            is_fullscreen: self.is_fullscreen,
                            audio_status: &audio_status,
                        },
                    );
                    let toggle_fullscreen = prepared_ui.output.toggle_fullscreen;
                    let exit_requested = prepared_ui.output.exit_requested;
                    let apply_settings = prepared_ui.output.apply_settings.clone();
                    let restart_capture = prepared_ui.output.restart_capture;
                    let restart_audio = prepared_ui.output.restart_audio;
                    if let Some(live) = &prepared_ui.output.live_video {
                        renderer.set_picture_settings(
                            live.brightness,
                            live.contrast,
                            live.saturation,
                            live.gamma,
                        );
                        renderer.set_color_settings(live.color_space, live.color_range);
                        renderer.set_scale_filter(live.scaling_filter);
                        renderer.set_sharpness(live.sharpness);
                    }
                    self.next_ui_redraw =
                        Instant::now().checked_add(prepared_ui.output.repaint_after);
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
                    if restart_capture {
                        self.capture_recovery.reset();
                        self.audio.reset_recovery();
                        self.queue_capture_restart(Some(AudioRestart::Previous));
                    } else if restart_audio {
                        info!("audio restart requested from settings");
                        self.audio.reset_recovery();
                        if self.audio.uses_directshow() || self.capture_restart_pending {
                            self.queue_capture_restart(Some(AudioRestart::Previous));
                        } else if self.audio.has_route() {
                            self.audio.restart_previous();
                        } else {
                            self.start_audio();
                        }
                    }
                    if exit_requested {
                        if let Some(capture) = &mut self.capture {
                            capture.stop();
                        }
                        self.audio.stop();
                        self.save_settings();
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
                if self.capture_restart_pending
                    || self.capture.is_none()
                    || self.latest_error.is_some()
                {
                    return;
                }
                // Skip frame processing entirely when the window isn't visible.
                // Without a render pass to drain them, queued write_texture calls
                // accumulate in GPU memory indefinitely (display off, minimized, etc).
                if self.is_minimized {
                    // Still drain the triple buffer so it doesn't hold stale frames
                    if let Some(capture) = &mut self.capture {
                        if capture.latest_frame().is_some() {
                            self.last_frame_at = Some(Instant::now());
                        }
                    }
                    return;
                }

                let mut uploaded = false;
                if let (Some(capture), Some(renderer)) = (&mut self.capture, &mut self.renderer) {
                    if let Some(frame) = capture.latest_frame() {
                        renderer.upload_frame(frame);
                        uploaded = true;
                    }
                }
                if uploaded {
                    self.latest_error = None;
                    self.render_frames_uploaded += 1;
                    self.last_frame_at = Some(Instant::now());
                    self.update_sleep_suppression();
                }

                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Frames arrive as user events, so the loop no longer spins. It still
        // needs a slow tick, though: the channels below and the timers further
        // down are only serviced here, and without a wakeup they'd stall
        // whenever frames stop — leaving a capture error unreported and the
        // overlay frozen on stale numbers.
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + HOUSEKEEPING_TICK));

        // Poll non-frame channels (stats, errors, negotiation).
        // These are low-frequency and don't need event-driven wakeup.
        let restarting = self.capture_restart_pending;
        if let Some(capture) = self.capture.as_mut().filter(|_| !restarting) {
            if let Some(stats) = capture.latest_stats() {
                self.latest_stats = Some(stats);
            }

            if let Some(error_message) = capture.latest_error() {
                self.latest_error = Some(error_message.clone());
                self.latest_stats = None;
                self.last_frame_at = None;
                warn!("capture error: {error_message}");
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            // If the capture thread fell back to a different resolution/fps,
            // update settings to reflect what's actually running.
            if let Some(negotiated) = capture.latest_negotiated() {
                info!(
                    "updating settings to match negotiated capture: {}x{} @ {}fps",
                    negotiated.width, negotiated.height, negotiated.fps
                );
                self.settings
                    .apply_negotiated(negotiated.width, negotiated.height, negotiated.fps);
                self.save_settings();
            }
        }

        // Reap only completed workers. Driver teardown never runs on the UI
        // thread, and sessions cannot overlap or reuse each other's GPU buffers.
        if self
            .capture
            .as_ref()
            .is_some_and(CaptureThread::is_finished)
        {
            let mut capture = self.capture.take().expect("finished capture exists");
            if let Some(message) = capture.latest_error() {
                self.latest_error = Some(message);
            }
            capture.stop();
            if !self.capture_restart_pending {
                self.latest_error
                    .get_or_insert_with(|| "Capture stopped unexpectedly; reconnecting...".into());
                self.capture_recovery.failed(Instant::now());
                warn!(
                    "capture stopped after {} recovery attempts: {:?}",
                    self.capture_recovery.attempts(),
                    self.latest_error
                );
                self.latest_stats = None;
                self.last_frame_at = None;
                self.audio.stop();
                self.audio_restart_pending = Some(AudioRestart::Previous);
                if self.capture_recovery.exhausted() {
                    let message = self.latest_error.get_or_insert_default();
                    message
                        .push_str(". Automatic recovery stopped; use Reconnect capture to retry.");
                }
            }
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        if self.capture.is_none()
            && (self.capture_restart_pending || self.capture_recovery.take_ready(Instant::now()))
        {
            self.capture_restart_pending = false;
            if let Some(restart) = self.audio_restart_pending.take() {
                match restart {
                    AudioRestart::Selected => self.start_audio(),
                    AudioRestart::Previous => self.audio.restart_previous(),
                }
            }
            self.start_capture();
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        if self.capture.is_some()
            && self.latest_error.is_none()
            && self
                .last_frame_at
                .is_some_and(|at| at.elapsed() < CAPTURE_LIVE_TIMEOUT)
            && !self.capture_restart_pending
        {
            self.capture_recovery.healthy(Instant::now());
        }
        let audio_status = self.audio.status();
        if !self.capture_restart_pending
            && self.audio_restart_pending.is_none()
            && self.audio.poll_health()
        {
            self.queue_capture_restart(Some(AudioRestart::Previous));
        }
        if self.audio.status() != audio_status {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }

        // Periodic render summary (every 30s)
        let render_elapsed = self.last_render_summary.elapsed();
        if render_elapsed >= std::time::Duration::from_secs(30) {
            let render_fps = self.render_frame_counter as f64 / render_elapsed.as_secs_f64();
            let upload_fps = self.render_frames_uploaded as f64 / render_elapsed.as_secs_f64();

            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            let gpu_info = self
                .gpu_monitor
                .as_ref()
                .map(|m| format!(", {}", m.snapshot()));
            #[cfg(not(all(target_os = "windows", feature = "gpu-decode")))]
            let gpu_info: Option<String> = None;

            info!(
                "render summary: {:.1} rendered fps, {:.1} uploaded fps, decode fps={:.1}{}",
                render_fps,
                upload_fps,
                self.latest_stats.as_ref().map(|s| s.fps).unwrap_or(0.0),
                gpu_info.as_deref().unwrap_or(""),
            );
            self.render_frame_counter = 0;
            self.render_frames_uploaded = 0;
            self.last_render_summary = Instant::now();
        }

        if self
            .next_ui_redraw
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.next_ui_redraw = None;
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }

        // Hide cursor after 3s of inactivity if the ui menu is closed
        let cursor_idle_elapsed = self.last_cursor_moved.elapsed();
        if self.is_cursor_visible
            && cursor_idle_elapsed >= Duration::from_secs(3)
            && self.ui.as_ref().is_none_or(|ui| !ui.is_menu_open())
        {
            self.set_cursor_visible(false);
        }

        // Let the display sleep again once frames have stopped arriving.
        self.update_sleep_suppression();
    }
}

impl App {
    fn save_settings(&self) {
        if !self.preview_mode {
            if let Err(error) = self.settings.save() {
                error!("failed to save settings: {error}");
            }
        }
    }

    fn queue_capture_restart(&mut self, restart_audio: Option<AudioRestart>) {
        self.capture_restart_pending = true;
        // A user-selected route takes priority over a queued recovery of the old route.
        if self.audio_restart_pending != Some(AudioRestart::Selected) {
            self.audio_restart_pending = restart_audio.or(self.audio_restart_pending);
        }
        self.last_frame_at = None;
        self.latest_stats = None;
        if let Some(capture) = &mut self.capture {
            capture.stop();
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn apply_settings(&mut self, mut new_settings: Settings) {
        new_settings.sanitize();
        let old_settings = self.settings.clone();
        self.settings = new_settings;

        self.save_settings();

        let video_changed = old_settings.video_device != self.settings.video_device
            || old_settings.video_format != self.settings.video_format
            || old_settings.resolution != self.settings.resolution
            || old_settings.fps_mode != self.settings.fps_mode
            || old_settings.custom_fps != self.settings.custom_fps;

        let audio_device_changed = old_settings.video_device != self.settings.video_device
            || old_settings.audio_input != self.settings.audio_input
            || old_settings.audio_output != self.settings.audio_output;

        let mute_changed = old_settings.audio_muted != self.settings.audio_muted;

        if (old_settings.volume - self.settings.volume).abs() > f64::EPSILON {
            self.audio.set_volume(self.settings.volume);
        }
        if mute_changed {
            self.audio.set_muted(self.settings.audio_muted);
        }
        if video_changed || audio_device_changed {
            let independent_audio_change = audio_device_changed
                && !video_changed
                && !self.audio.uses_directshow()
                && self.settings.audio_input != devices::DIRECTSHOW_AUDIO_INDEX
                && self.capture.is_some()
                && !self.capture_restart_pending;
            if independent_audio_change {
                self.start_audio();
            } else {
                self.capture_recovery.reset();
                self.queue_capture_restart(audio_device_changed.then_some(AudioRestart::Selected));
            }
        }

        if let Some(renderer) = &mut self.renderer {
            renderer.set_scale_filter(self.settings.scaling_filter);
            renderer.set_sharpness(self.settings.sharpness);
            renderer.set_color_settings(self.settings.color_space, self.settings.color_range);
            renderer.set_picture_settings(
                self.settings.brightness,
                self.settings.contrast,
                self.settings.saturation,
                self.settings.gamma,
            );
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn overlay_info(&self) -> OverlayInfo {
        let status_message = if self.capture_restart_pending {
            Some("Restarting capture...".into())
        } else if let Some(error_message) = self.latest_error.as_ref() {
            Some(error_message.clone())
        } else if self.latest_stats.is_none() {
            Some("Connecting...".to_string())
        } else {
            None
        };

        OverlayInfo {
            width: self.latest_stats.as_ref().map(|stats| stats.width),
            height: self.latest_stats.as_ref().map(|stats| stats.height),
            fps: self.latest_stats.as_ref().map(|stats| stats.fps),
            show_overlay: self.settings.show_overlay && !self.is_minimized,
            filter: self
                .renderer
                .as_ref()
                .map(Renderer::scale_filter)
                .unwrap_or(self.settings.scaling_filter),
            filter_label: self
                .renderer
                .as_ref()
                .map(Renderer::active_scale_filter_label)
                .map(str::to_string)
                .unwrap_or_else(|| self.settings.scaling_filter.to_string()),
            color_space: self.settings.color_space,
            color_range: self.settings.color_range,
            color_info: self
                .renderer
                .as_ref()
                .map(Renderer::color_info)
                .unwrap_or_default(),
            active_format: self
                .latest_stats
                .as_ref()
                .map(|stats| stats.format_label.clone()),
            nominal_rate: self
                .latest_stats
                .as_ref()
                .and_then(|stats| stats.nominal_rate),
            layout: self
                .settings
                .overlay
                .clone()
                .unwrap_or_else(|| overlay::OverlayLayout::legacy(self.settings.detailed_overlay)),
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
        }
    }

    fn start_capture(&mut self) {
        self.latest_stats = None;
        self.latest_error = None;
        self.last_frame_at = None;
        let capture_config = get_capture_config(
            &self.settings.resolution,
            self.settings.get_fps(),
            self.settings.video_format,
        );
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
        #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
        let shared_gpu_handles = if let Some(renderer) = &mut self.renderer {
            renderer.try_init_shared_buffers(capture_config.width, capture_config.height)
        } else {
            None
        };

        info!(
            "starting capture: device='{}' {}x{} @ {}fps ({}, threads={})",
            video_device,
            capture_config.width,
            capture_config.height,
            capture_config.fps,
            capture_config.pixel_format,
            capture_config.decode_threads
        );
        self.capture = Some(CaptureThread::start(
            CaptureConfig {
                width: capture_config.width,
                height: capture_config.height,
                fps: capture_config.fps,
                source: CaptureSource::Device {
                    device_name: video_device,
                    pixel_format: capture_config.pixel_format.to_string(),
                    decode_threads: capture_config.decode_threads,
                    #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
                    shared_gpu_handles,
                },
                audio_sink: self.audio.directshow_sink(),
            },
            self.event_proxy.clone(),
        ));
    }

    fn start_test_capture(
        &mut self,
        width: u32,
        height: u32,
        fps: frame_rate::FrameRate,
        reason: &str,
    ) {
        let (alternate_formats, force_format) = match self.test_pattern {
            TestPatternMode::Alternate => (true, None),
            TestPatternMode::Nv12 => (false, Some(PixelFormat::Nv12)),
            TestPatternMode::Yuvj422p => (false, Some(PixelFormat::Yuvj422p)),
        };

        info!(
            "starting test-pattern capture: {}x{} @ {}fps ({reason})",
            width, height, fps
        );
        self.capture = Some(CaptureThread::start(
            CaptureConfig {
                width,
                height,
                fps,
                source: CaptureSource::TestPattern {
                    alternate_formats,
                    force_format,
                },
                audio_sink: None,
            },
            self.event_proxy.clone(),
        ));
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
            self.settings.audio_muted,
        );
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn preview_defaults_invalid_values_and_duplicate_flags_are_bounded() {
        let plain = CliArgs::parse_from(["--test"]);
        assert!(plain.test_mode);
        assert!(!plain.preview_active());
        assert_eq!(plain.preview_page, None);
        assert_eq!(plain.preview_theme, None);
        let invalid = CliArgs::parse_from(["--test", "--test-ui=bogus", "--test-theme=bogus"]);
        assert!(!invalid.preview_active());
        let selected = CliArgs::parse_from([
            "--TEST-NV12",
            "--test-ui=video",
            "--TEST-UI=OVERLAY",
            "--TEST-THEME=VICE",
        ]);
        assert_eq!(selected.preview_page, Some(ui::PreviewPage::Overlay));
        assert_eq!(selected.preview_theme, Some(settings::Theme::Vice));
        assert!(matches!(selected.test_pattern, TestPatternMode::Nv12));
        let theme_only = CliArgs::parse_from(["--test-mjpeg", "--test-theme=classic"]);
        assert!(theme_only.preview_active());
        assert_eq!(theme_only.preview_page, None);
        assert!(matches!(theme_only.test_pattern, TestPatternMode::Yuvj422p));
        let not_a_test_flag = CliArgs::parse_from(["--test-ui=none"]);
        assert!(!not_a_test_flag.test_mode);
        assert!(!not_a_test_flag.preview_active());
    }

    #[test]
    fn preview_selection_requires_explicit_test_mode() {
        for flag in [
            "--test",
            "--test-alt",
            "--test-nv12",
            "--test-yuvj422p",
            "--test-mjpeg",
        ] {
            for (name, page) in [
                ("video", ui::PreviewPage::Video),
                ("picture", ui::PreviewPage::Picture),
                ("audio", ui::PreviewPage::Audio),
                ("display", ui::PreviewPage::Display),
                ("overlay", ui::PreviewPage::Overlay),
                ("none", ui::PreviewPage::None),
            ] {
                let selection = format!("--test-ui={name}");
                let cli = CliArgs::parse_from([flag, selection.as_str(), "--test-theme=classic"]);
                assert_eq!(cli.preview_page, Some(page));
                assert_eq!(cli.preview_theme, Some(settings::Theme::Classic));
                assert!(cli.preview_active());
            }
        }
        let cli = CliArgs::parse_from(["--test-ui=overlay", "--test-theme=vice"]);
        assert!(!cli.test_mode);
        assert_eq!(cli.preview_page, None);
        assert_eq!(cli.preview_theme, None);
        assert!(!cli.preview_active());
    }
}
