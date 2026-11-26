#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod audio;
mod capabilities;
mod capture;
mod color;
mod device_refresh;
mod devices;
#[cfg(all(target_os = "windows", feature = "gpu-decode"))]
mod dx12_interop;
#[cfg(target_os = "windows")]
mod dxgi_compat;
mod frame_rate;
mod framegen;
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
mod updater;
#[cfg(any(target_os = "windows", test))]
mod usb_link;
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

const WINDOW_TITLE: &str = "TackleCast v3.VI";
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
    /// The frame generation worker has results or status for the UI thread.
    FrameGenEvent,
    /// The background updater worker has new status or download progress.
    UpdateEvent,
}

#[derive(Clone, Copy, Debug)]
enum TestPatternMode {
    Alternate,
    Nv12,
    Yuvj422p,
    /// Moving box, exact pacing (frame generation tests).
    Motion,
}

#[derive(Debug)]
struct CliArgs {
    no_updates: bool,
    test_mode: bool,
    test_pattern: TestPatternMode,
    preview_page: Option<ui::PreviewPage>,
    preview_theme: Option<settings::Theme>,
    /// `--test-fps=30|29.97|60000/1001...`: overrides the test pattern rate.
    test_fps: Option<frame_rate::FrameRate>,
    /// `--framegen`: enable frame generation for this run (not saved).
    force_framegen: bool,
    /// `--framegen-bench=SECONDS`: run, write a report, exit.
    bench_seconds: Option<f64>,
    bench_report: Option<std::path::PathBuf>,
    /// `--framegen-toggle-test`: during the bench, turn frame generation
    /// off and on again to check release and recovery.
    toggle_test: bool,
    /// `--test-clip=<clip folder>`: play a recorded clip as the capture
    /// source (looping, recorded timing).
    test_clip: Option<std::path::PathBuf>,
    /// `--framegen-backend=rife|fruc`: engine for this run (not saved).
    backend_override: Option<settings::FrameGenBackend>,
    /// `--framegen-rife-model=rife426fmh|rife46fmh|rife46fm`.
    rife_model_override: Option<settings::RifeModel>,
    /// `--framegen-artifacts=off|dynamic|advanced`: for this run (not saved).
    artifacts_override: Option<settings::FrameGenArtifactMode>,
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

        let value = |prefix: &str| {
            args.iter()
                .find_map(|arg| arg.strip_prefix(prefix).map(str::to_string))
        };
        let test_pattern = if has_flag("--test-motion") {
            TestPatternMode::Motion
        } else if has_flag("--test-nv12") {
            TestPatternMode::Nv12
        } else if has_flag("--test-yuvj422p") || has_flag("--test-mjpeg") {
            TestPatternMode::Yuvj422p
        } else {
            TestPatternMode::Alternate
        };

        let test_clip = value("--test-clip=").map(std::path::PathBuf::from);
        let test_mode = test_clip.is_some()
            || has_flag("--test")
            || has_flag("--test-alt")
            || has_flag("--test-nv12")
            || has_flag("--test-yuvj422p")
            || has_flag("--test-mjpeg")
            || has_flag("--test-motion");
        let test_fps = value("--test-fps=").and_then(|v| match v.as_str() {
            "29.97" => Some(frame_rate::FrameRate::new(30_000, 1001)),
            "59.94" => Some(frame_rate::FrameRate::new(60_000, 1001)),
            other => other.parse().ok(),
        });
        let bench_seconds = value("--framegen-bench=").and_then(|v| v.parse::<f64>().ok());

        let mut preview_page = None;
        let mut preview_theme = None;
        if test_mode {
            for arg in &args {
                if let Some(value) = arg.strip_prefix("--test-ui=") {
                    preview_page = match value {
                        "video" => Some(ui::PreviewPage::Video),
                        "picture" => Some(ui::PreviewPage::Picture),
                        "audio" => Some(ui::PreviewPage::Audio),
                        "framegen" => Some(ui::PreviewPage::FrameGen),
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
            no_updates: has_flag("--no-updates"),
            test_mode,
            test_pattern,
            preview_page,
            preview_theme,
            test_fps,
            force_framegen: has_flag("--framegen") || bench_seconds.is_some(),
            bench_seconds,
            bench_report: value("--bench-report=").map(std::path::PathBuf::from),
            toggle_test: has_flag("--framegen-toggle-test"),
            test_clip,
            rife_model_override: value("--framegen-rife-model=")
                .and_then(|v| settings::RifeModel::from_stem(&v)),
            backend_override: value("--framegen-backend=").and_then(|v| match v.as_str() {
                "rife" => Some(settings::FrameGenBackend::Rife),
                "fruc" => Some(settings::FrameGenBackend::NvidiaFruc),
                _ => None,
            }),
            artifacts_override: value("--framegen-artifacts=").and_then(|v| match v.as_str() {
                "off" => Some(settings::FrameGenArtifactMode::Off),
                "dynamic" => Some(settings::FrameGenArtifactMode::Dynamic),
                "advanced" => Some(settings::FrameGenArtifactMode::Advanced),
                _ => None,
            }),
        }
    }
}

fn main() {
    if let Some(exit_code) = updater::maybe_run_apply_update_from_args() {
        std::process::exit(exit_code);
    }
    #[cfg(target_os = "windows")]
    if capabilities::handle_probe_args() {
        return;
    }
    if let Err(error) = logger::init_logging() {
        eprintln!("failed to initialize logging: {error}");
    }
    let _update_lock = match updater::prepare_startup() {
        Ok(lock) => lock,
        Err(error) => {
            error!("Cannot start TackleCast during update recovery: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = ffmpeg_next::init() {
        eprintln!("failed to initialize ffmpeg: {error}");
    }

    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if raw_args.iter().any(|a| a.starts_with("--fruc-selftest")) {
        #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
        if let Some(args) = framegen::selftest::SelftestArgs::from_cli(&raw_args) {
            info!("====== TackleCast FRUC self-test ======");
            std::process::exit(framegen::selftest::run(args));
        }
        eprintln!("--fruc-selftest requires the Windows build with the gpu-decode feature");
        std::process::exit(2);
    }

    if raw_args.iter().any(|a| a.starts_with("--fruc-reference=")) {
        #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
        if let Some(args) = framegen::replay::ReferenceArgs::from_cli(&raw_args) {
            info!("====== TackleCast FRUC reference run ======");
            std::process::exit(framegen::replay::run_reference(args));
        }
        eprintln!("--fruc-reference requires the Windows build with the gpu-decode feature");
        std::process::exit(2);
    }
    if raw_args.iter().any(|a| a.starts_with("--fg-replay=")) {
        #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
        if let Some(args) = framegen::replay::ReplayArgs::from_cli(&raw_args) {
            info!("====== TackleCast frame-generation replay ======");
            std::process::exit(framegen::replay::run(args));
        }
        eprintln!("--fg-replay requires the Windows build with the gpu-decode feature");
        std::process::exit(2);
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
    devices::refresh_audio_choices(&mut settings, &audio_inputs, &audio_outputs);
    let selected = devices::select_capture_devices(
        &settings.video_device,
        settings.audio_input,
        &video_devices,
        &audio_inputs,
    );
    if !cli.test_mode && !selected.video_device.is_empty() {
        capabilities::prime_device_capabilities(&selected.video_device);
    }

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
        &cli,
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
    test_clip: Option<std::path::PathBuf>,
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
    device_refresh: Option<device_refresh::DeviceRefresh>,
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
    framegen: framegen::FrameGenController,
    /// Diagnostic clip recorder (raw captured frames for `--fg-replay`).
    recorder: framegen::recorder::ClipRecorder,
    /// Manual GitHub Releases self-updater controller.
    updater: Option<updater::UpdateController>,
    /// Frame generation was forced on from the command line (not saved).
    force_framegen: bool,
    /// False for automated runs (bench) so the user's settings are untouched.
    persist_settings: bool,
    test_fps: Option<frame_rate::FrameRate>,
    bench: Option<Bench>,
}

/// `--framegen-bench`: automated run that writes a pacing report and exits.
struct Bench {
    started: Instant,
    duration: Duration,
    report: std::path::PathBuf,
    toggle_test: bool,
    toggled_off: bool,
    toggled_on: bool,
    events: Vec<String>,
}

impl App {
    fn new(
        settings: Settings,
        cli: &CliArgs,
        video_devices: Vec<String>,
        audio_inputs: Vec<AudioDevice>,
        audio_outputs: Vec<AudioDevice>,
        event_proxy: EventLoopProxy<AppEvent>,
    ) -> Self {
        let mut effective = settings.clone();
        if let Some(backend) = cli.backend_override {
            effective.framegen_backend = backend;
        }
        if let Some(model) = cli.rife_model_override {
            effective.framegen_rife_model = model;
        }
        if let Some(mode) = cli.artifacts_override {
            effective.framegen_artifact_mode = mode;
        }
        let mut fg_config = framegen::FgConfig::from_settings(&effective);
        if cli.force_framegen {
            fg_config.enabled = true;
        }
        let waker_proxy = event_proxy.clone();
        let factory = framegen::platform_backend_factory(Box::new(move || {
            let _ = waker_proxy.send_event(AppEvent::FrameGenEvent);
        }));
        let updater = (!cli.no_updates).then(|| {
            let update_proxy = event_proxy.clone();
            updater::UpdateController::new(move || {
                let _ = update_proxy.send_event(AppEvent::UpdateEvent);
            })
        });
        let bench = cli.bench_seconds.map(|seconds| Bench {
            started: Instant::now(),
            duration: Duration::from_secs_f64(seconds.clamp(5.0, 3600.0)),
            report: cli.bench_report.clone().unwrap_or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("framegen_bench_report.json")))
                    .unwrap_or_else(|| "framegen_bench_report.json".into())
            }),
            toggle_test: cli.toggle_test,
            toggled_off: false,
            toggled_on: false,
            events: Vec::new(),
        });
        Self {
            framegen: framegen::FrameGenController::new(fg_config, factory),
            recorder: Default::default(),
            updater,
            force_framegen: cli.force_framegen,
            persist_settings: cli.bench_seconds.is_none() && !cli.test_mode,
            test_fps: cli.test_fps,
            bench,
            settings,
            preview_mode: false,
            preview_page: None,
            test_mode: cli.test_mode,
            test_pattern: cli.test_pattern,
            test_clip: cli.test_clip.clone(),
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
            device_refresh: None,
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
            use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
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
                    Some(HINSTANCE(h_instance.0)),
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
                            Some(WPARAM(ICON_SMALL as usize)),
                            Some(LPARAM(h_icon.0 as isize)),
                        );
                        SendMessageW(
                            hwnd,
                            WM_SETICON,
                            Some(WPARAM(ICON_BIG as usize)),
                            Some(LPARAM(h_icon.0 as isize)),
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

        if self.bench.is_none() {
            self.start_audio();
        } else {
            info!("bench run: audio passthrough not started");
        }
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
                            if let Some(settings) =
                                ui.toggle_menu(&self.settings, || window.request_redraw())
                            {
                                self.apply_settings(settings);
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
            // The returned bool describes input ownership, not drawing. This
            // app has no competing pointer/typing handler; lifecycle events
            // below must still be processed when egui consumes input.
            let _consumed = ui.on_window_event(&window, &event, || window.request_redraw());
        }

        match event {
            WindowEvent::CloseRequested => {
                if let Some(capture) = &mut self.capture {
                    capture.stop();
                }
                self.audio.stop();
                if let Some(renderer) = &mut self.renderer {
                    self.framegen.shutdown(renderer);
                }
                self.save_settings();
                event_loop.exit();
            }
            WindowEvent::CursorMoved { .. } => {
                self.last_cursor_moved = Instant::now();
                self.set_cursor_visible(true);
            }
            WindowEvent::Resized(size) => {
                let was_minimized = self.is_minimized;
                self.is_minimized = size.width == 0 || size.height == 0;
                if was_minimized != self.is_minimized {
                    if let Some(renderer) = &mut self.renderer {
                        self.framegen.break_continuity(renderer);
                    }
                }
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
                if !self.is_minimized {
                    window.request_redraw();
                }
                self.update_sleep_suppression();
            }
            WindowEvent::Occluded(occluded) => {
                if occluded != self.is_minimized {
                    if let Some(renderer) = &mut self.renderer {
                        self.framegen.break_continuity(renderer);
                    }
                }
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
                let fg_diagnostics = overlay.framegen.clone();
                let recorder_status = self.recorder.status().to_string();
                let recorder_available = !self.recorder.is_busy() && self.capture.is_some();
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
                            devices_busy: self.device_refresh.is_some(),
                            framegen: &fg_diagnostics,
                            framegen_forced: self.force_framegen,
                            recorder_status: &recorder_status,
                            recorder_available,
                            update_status: self
                                .updater
                                .as_ref()
                                .map(updater::UpdateController::snapshot),
                        },
                    );
                    if prepared_ui.output.retry_framegen {
                        self.framegen.retry(renderer);
                    }
                    if prepared_ui.output.record_clip && !self.recorder.is_busy() {
                        let height = renderer
                            .current_video_size()
                            .map(|(_, h)| h)
                            .unwrap_or(1080);
                        let color = renderer.color_info();
                        let info = framegen::recorder::ClipInfo {
                            color_space: color.resolve_space(self.settings.color_space, height),
                            color_range: color.resolve_range(self.settings.color_range),
                            nominal_fps: nominal_fps(&self.settings.fps_mode),
                            framegen_settings: framegen_settings_json(&self.settings),
                        };
                        let base = std::env::current_exe()
                            .ok()
                            .and_then(|p| p.parent().map(|d| d.join("recordings")))
                            .unwrap_or_else(|| "recordings".into());
                        self.recorder
                            .start(&base, Duration::from_secs(10), info, Instant::now());
                    }
                    let toggle_fullscreen = prepared_ui.output.toggle_fullscreen;
                    let mut exit_requested = prepared_ui.output.exit_requested;
                    let apply_settings = prepared_ui.output.apply_settings.clone();
                    let reinitialize_devices = prepared_ui.output.reinitialize_devices.clone();
                    let update_action = prepared_ui.output.update_action.clone();
                    if let Some(live) = &prepared_ui.output.live_settings {
                        self.audio.set_volume(live.volume);
                        self.audio.set_muted(live.audio_muted);
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
                    self.framegen.on_presented(Instant::now());
                    self.render_frame_counter += 1;
                    if toggle_fullscreen {
                        self.toggle_fullscreen();
                    }
                    if let Some(settings) = apply_settings {
                        self.apply_settings(settings);
                    }
                    if let Some(settings) = reinitialize_devices {
                        self.reinitialize_devices(settings);
                    }
                    if let Some(action) = update_action {
                        if self.handle_update_action(action) {
                            exit_requested = true;
                        }
                    }
                    if exit_requested {
                        if let Some(capture) = &mut self.capture {
                            capture.stop();
                        }
                        self.audio.stop();
                        if let Some(renderer) = &mut self.renderer {
                            self.framegen.shutdown(renderer);
                        }
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
                let mut action = framegen::FrameAction::RedrawNow;
                if let (Some(capture), Some(renderer)) = (&mut self.capture, &mut self.renderer) {
                    if let Some(frame) = capture.latest_frame() {
                        let arrival = Instant::now();
                        renderer.upload_frame(frame);
                        self.recorder.push(frame, arrival);
                        uploaded = true;
                        action = self.framegen.on_frame_uploaded(renderer, arrival);
                    }
                }
                if uploaded {
                    self.latest_error = None;
                    self.render_frames_uploaded += 1;
                    self.last_frame_at = Some(Instant::now());
                    self.update_sleep_suppression();
                }

                // With frame generation presenting, frames are shown on its
                // schedule instead of on arrival.
                if action == framegen::FrameAction::RedrawNow || !uploaded {
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            AppEvent::FrameGenEvent => self.service_framegen(),
            AppEvent::UpdateEvent => {
                if self
                    .updater
                    .as_mut()
                    .is_some_and(updater::UpdateController::poll)
                {
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
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
        self.service_framegen();
        self.recorder.tick(Instant::now());
        let mut wake_at = Instant::now() + HOUSEKEEPING_TICK;
        if let Some(deadline) = self.framegen.next_deadline() {
            wake_at = wake_at.min(deadline);
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(wake_at));
        self.update_bench(event_loop);

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
                    message.push_str(
                        ". Automatic recovery stopped; use Reinitialize devices to retry.",
                    );
                }
            }
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        self.poll_device_refresh();
        if self.device_refresh.is_none()
            && self.capture.is_none()
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
            && self.device_refresh.is_none()
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
    /// Handles an action emitted by the settings footer or inline update banner.
    ///
    /// Returns `true` when the staged updater was launched and the application
    /// should exit cleanly so file replacement can proceed.
    fn handle_update_action(&mut self, action: updater::UpdateAction) -> bool {
        let Some(updater) = self.updater.as_mut() else {
            return false;
        };
        let should_exit = match action {
            updater::UpdateAction::Check => {
                updater.check_for_updates();
                false
            }
            updater::UpdateAction::Download => {
                updater.start_download();
                false
            }
            updater::UpdateAction::Cancel => {
                updater.cancel_download();
                false
            }
            updater::UpdateAction::RestartToApply => match updater.launch_staged_updater() {
                Ok(()) => {
                    info!("launched staged updater; exiting to apply update");
                    true
                }
                Err(err) => {
                    error!("failed launching staged updater: {err}");
                    false
                }
            },
            updater::UpdateAction::OpenReleasePage(url) => {
                if let Err(err) = updater::open_release_page(&url) {
                    warn!("failed opening release page: {err}");
                }
                false
            }
            updater::UpdateAction::Dismiss => {
                updater.dismiss();
                false
            }
        };
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        should_exit
    }

    fn remember_changed_audio_choices(&self, settings: &mut Settings) {
        if settings.audio_input != self.settings.audio_input {
            settings.audio_input_name = None;
        }
        if settings.audio_output != self.settings.audio_output {
            settings.audio_output_name = None;
        }
        devices::remember_audio_choices(settings, &self.audio_inputs, &self.audio_outputs);
    }

    fn reinitialize_devices(&mut self, mut settings: Settings) {
        if self.device_refresh.is_some() {
            return;
        }
        if self.test_mode {
            self.apply_settings(settings);
            return;
        }
        settings.sanitize();
        self.remember_changed_audio_choices(&mut settings);
        let old_settings = self.settings.clone();
        self.settings = settings;
        self.configure_framegen(&old_settings);
        self.save_settings();
        self.capture_recovery.reset();
        self.audio.wait_for_selection("Reinitializing devices…");
        self.audio_restart_pending = None;
        self.queue_capture_restart(None);
        self.latest_error = None;
        let generation = capabilities::begin_device_refresh();
        self.device_refresh = Some(device_refresh::DeviceRefresh::new(
            self.settings.clone(),
            generation,
        ));
        info!("device reinitialization requested; waiting for capture retirement");
    }

    fn poll_device_refresh(&mut self) {
        let Some(task) = self.device_refresh.as_mut() else {
            return;
        };
        let generation = task.generation;
        let Some(result) = task.poll(
            self.capture.is_none(),
            device_refresh::DiscoveredDevices::scan,
        ) else {
            return;
        };
        self.device_refresh = None;
        self.capture_restart_pending = false;
        self.audio_restart_pending = None;
        match result {
            Ok(fresh) => {
                capabilities::finish_device_refresh(
                    generation,
                    (!fresh.video.is_empty()).then_some((fresh.video, fresh.capabilities)),
                );
                self.video_devices = fresh.videos;
                self.audio_inputs = fresh.inputs;
                self.audio_outputs = fresh.outputs;
                self.settings = fresh.settings;
                if let Some(ui) = &mut self.ui {
                    ui.refresh_device_settings(&self.settings);
                }
                self.save_settings();
                info!("device reinitialization discovered {} video, {} audio inputs, {} audio outputs", self.video_devices.len(), self.audio_inputs.len(), self.audio_outputs.len());
                self.start_audio();
                self.start_capture();
            }
            Err(message) => {
                capabilities::finish_device_refresh(generation, None);
                self.audio.wait_for_selection(&message);
                self.latest_error = Some(message);
            }
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn save_settings(&self) {
        if !self.preview_mode && self.persist_settings {
            if let Err(error) = self.settings.save() {
                error!("failed to save settings: {error}");
            }
        }
    }

    /// Drain frame generation results and show any image that is due.
    fn service_framegen(&mut self) {
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        let now = Instant::now();
        let mut redraw = self.framegen.process_events(renderer, now);
        if !self.is_minimized {
            redraw |= self.framegen.poll_display(renderer, now);
        }
        if redraw {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    /// Drive `--framegen-bench`: optional off/on toggle, then report and exit.
    fn update_bench(&mut self, event_loop: &ActiveEventLoop) {
        let Some(bench) = self.bench.as_mut() else {
            return;
        };
        let elapsed = bench.started.elapsed();
        let fraction = elapsed.as_secs_f64() / bench.duration.as_secs_f64();
        let mut toggle: Option<bool> = None;
        if bench.toggle_test && !bench.toggled_off && fraction >= 0.40 {
            bench.toggled_off = true;
            toggle = Some(false);
        } else if bench.toggle_test && bench.toggled_off && !bench.toggled_on && fraction >= 0.55 {
            bench.toggled_on = true;
            toggle = Some(true);
        }
        if let (Some(enabled), Some(renderer)) = (toggle, self.renderer.as_mut()) {
            let before = format!("{:?}", self.framegen.state());
            let mut config = self.framegen.config().clone();
            config.enabled = enabled;
            self.framegen.configure(config, renderer);
            let message = format!(
                "{:.1}s: frame generation {} ({} -> {:?})",
                elapsed.as_secs_f64(),
                if enabled { "re-enabled" } else { "disabled" },
                before,
                self.framegen.state()
            );
            info!("bench: {message}");
            bench.events.push(message);
        }
        if elapsed < bench.duration {
            return;
        }
        let diagnostics = self.framegen.diagnostics(Instant::now());
        let intervals = self.framegen.present_intervals_ms();
        let mut histogram = std::collections::BTreeMap::<String, usize>::new();
        for value in &intervals {
            let bucket = (value * 2.0).round() / 2.0;
            *histogram.entry(format!("{bucket:05.1} ms")).or_default() += 1;
        }
        let report = serde_json::json!({
            "tool": format!("TackleCast-FrameGen {} --framegen-bench", env!("CARGO_PKG_VERSION")),
            "duration_s": bench.duration.as_secs_f64(),
            "test_pattern": format!("{:?}", self.test_pattern),
            "test_clip": self.test_clip.as_ref().map(|p| p.display().to_string()),
            "framegen_backend": self.framegen.config().backend.to_string(),
            "framegen_rife_model": self.framegen.config().rife_model.stem(),
            "source_fps": self.test_fps.map(|f| f.as_f64()).unwrap_or_else(|| self.settings.get_fps().as_f64()),
            "capture_resolution": self.latest_stats.as_ref().map(|s| format!("{}x{}", s.width, s.height)),
            "capture_measured_fps": self.latest_stats.as_ref().map(|s| s.fps),
            "framegen": diagnostics,
            "present_intervals_last_n_ms": intervals,
            "present_interval_histogram": histogram,
            "toggle_events": bench.events,
            "note": "Present intervals are measured when TackleCast hands a new image to the swap chain;                      the compositor and display add their own (unmeasured) timing.",
        });
        let path = bench.report.clone();
        match std::fs::write(
            &path,
            serde_json::to_string_pretty(&report).unwrap_or_default(),
        ) {
            Ok(()) => info!("bench report written to {}", path.display()),
            Err(e) => error!("could not write bench report {}: {e}", path.display()),
        }
        if let Some(capture) = &mut self.capture {
            capture.stop();
        }
        self.audio.stop();
        if let Some(renderer) = &mut self.renderer {
            self.framegen.shutdown(renderer);
        }
        self.bench = None;
        event_loop.exit();
    }

    fn queue_capture_restart(&mut self, restart_audio: Option<AudioRestart>) {
        self.capture_restart_pending = true;
        if let Some(renderer) = &mut self.renderer {
            self.framegen.break_continuity(renderer);
        }
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

    fn configure_framegen(&mut self, old_settings: &Settings) {
        if let Some(renderer) = &mut self.renderer {
            let mut fg_config = framegen::FgConfig::from_settings(&self.settings);
            if self.force_framegen
                && self.settings.framegen_enabled == old_settings.framegen_enabled
            {
                fg_config.enabled = true;
            }
            self.framegen.configure(fg_config, renderer);
        }
    }

    fn apply_settings(&mut self, mut new_settings: Settings) {
        new_settings.sanitize();
        self.remember_changed_audio_choices(&mut new_settings);
        let old_settings = self.settings.clone();
        self.settings = new_settings;

        self.save_settings();
        self.configure_framegen(&old_settings);

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
        if (video_changed || audio_device_changed) && self.device_refresh.is_none() {
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

    fn overlay_info(&mut self) -> OverlayInfo {
        let status_message = if self.device_refresh.is_some() {
            Some("Reinitializing devices…".into())
        } else if self.capture_restart_pending {
            Some("Restarting capture...".into())
        } else if let Some(error_message) = self.latest_error.as_ref() {
            Some(error_message.clone())
        } else if self.latest_stats.is_none() {
            Some("Connecting...".to_string())
        } else {
            None
        };

        OverlayInfo {
            framegen: self.framegen.diagnostics(Instant::now()),
            width: self.latest_stats.as_ref().map(|stats| stats.width),
            height: self.latest_stats.as_ref().map(|stats| stats.height),
            fps: self.latest_stats.as_ref().map(|stats| stats.fps),
            show_overlay: self.settings.overlay_mode() != crate::overlay::Mode::Hidden
                && !self.is_minimized,
            mode: self.settings.overlay_mode(),
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

        let selection = devices::select_capture_devices(
            &self.settings.video_device,
            self.settings.audio_input,
            &self.video_devices,
            &self.audio_inputs,
        );
        let video_device = selection.video_device.clone();

        if video_device.is_empty() {
            let message = selection.message().unwrap().to_string();
            warn!("{message}");
            self.latest_error = Some(message);
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
            TestPatternMode::Nv12 | TestPatternMode::Motion => (false, Some(PixelFormat::Nv12)),
            TestPatternMode::Yuvj422p => (false, Some(PixelFormat::Yuvj422p)),
        };
        let motion = matches!(self.test_pattern, TestPatternMode::Motion);
        let fps = self.test_fps.unwrap_or(fps);

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
                    motion,
                    clip: self.test_clip.clone(),
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

        let selection = devices::select_capture_devices(
            &self.settings.video_device,
            self.settings.audio_input,
            &self.video_devices,
            &self.audio_inputs,
        );
        if let Some(message) = selection.message() {
            self.audio.wait_for_selection(message);
            return;
        }

        self.audio.start(&selection.video_device, &self.settings);
    }
}

/// Nominal capture rate from the settings key ("30", "29.97", "30000/1001").
fn nominal_fps(key: &str) -> f64 {
    if let Some((n, d)) = key.split_once('/') {
        if let (Ok(n), Ok(d)) = (n.trim().parse::<f64>(), d.trim().parse::<f64>()) {
            if d > 0.0 {
                return n / d;
            }
        }
    }
    key.trim().parse().unwrap_or(0.0)
}

/// The frame-generation part of the settings, as JSON (for clip metadata).
fn framegen_settings_json(settings: &settings::Settings) -> String {
    let Ok(serde_json::Value::Object(all)) = serde_json::to_value(settings) else {
        return String::new();
    };
    let fg: serde_json::Map<_, _> = all
        .into_iter()
        .filter(|(k, _)| k.starts_with("framegen"))
        .collect();
    serde_json::Value::Object(fg).to_string()
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[cfg(target_os = "windows")]
    pub(crate) fn check_soft_reinitialize_without_capture_frames(proxy: EventLoopProxy<AppEvent>) {
        let settings = Settings {
            volume: 0.42,
            brightness: 0.2,
            ..Default::default()
        };
        let mut app = App::new(
            settings.clone(),
            &CliArgs::parse_from(std::iter::empty::<&str>()),
            vec![],
            vec![],
            vec![],
            proxy,
        );
        app.preview_mode = true; // The fixture must never write the user's saved settings.
        app.reinitialize_devices(settings.clone());
        assert!(app.device_refresh.is_some() && app.capture_restart_pending);
        assert!(app.capture.is_none() && app.latest_stats.is_none());
        let generation = app.device_refresh.as_ref().unwrap().generation;
        app.reinitialize_devices(settings.clone());
        assert_eq!(
            app.device_refresh.as_ref().unwrap().generation,
            generation,
            "coalesce repeated clicks"
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        app.device_refresh = Some(device_refresh::DeviceRefresh::from_receiver(
            generation, receiver,
        ));
        app.poll_device_refresh();
        assert!(
            app.device_refresh.is_some() && app.capture.is_none(),
            "no pipeline may open before discovery completes"
        );
        sender
            .send(device_refresh::DiscoveredDevices {
                settings: settings.clone(),
                videos: vec![],
                inputs: vec![],
                outputs: vec![],
                video: "soft-refresh-card".into(),
                capabilities: capabilities::DeviceCapabilities {
                    modes: vec![capabilities::CaptureMode {
                        width: 3840,
                        height: 2160,
                        format: settings::VideoFormat::Mjpeg,
                        rate: 60.into(),
                    }],
                    ..Default::default()
                },
            })
            .unwrap();
        app.poll_device_refresh();
        assert!(app.device_refresh.is_none() && !app.capture_restart_pending);
        assert!(
            app.capture.is_none() && app.latest_stats.is_none(),
            "empty discovery must not open a substitute device"
        );
        assert_eq!(app.settings, settings);
        assert_eq!(
            capabilities::get_device_capabilities("soft-refresh-card").modes[0].width,
            3840
        );
        app.reinitialize_devices(settings);
        let generation = app.device_refresh.as_ref().unwrap().generation;
        let (sender, receiver) = std::sync::mpsc::channel();
        drop(sender);
        app.device_refresh = Some(device_refresh::DeviceRefresh::from_receiver(
            generation, receiver,
        ));
        app.poll_device_refresh();
        assert!(app.device_refresh.is_none() && app.capture.is_none());
        assert!(app
            .latest_error
            .as_ref()
            .unwrap()
            .contains("discovery failed"));
    }

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
    fn updates_can_be_disabled_at_launch() {
        assert!(CliArgs::parse_from(["--no-updates"]).no_updates);
        assert!(!CliArgs::parse_from(["--test"]).no_updates);
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
