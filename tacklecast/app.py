import sys

from PyQt6.QtWidgets import (
    QApplication, QMainWindow, QWidget, QVBoxLayout, QHBoxLayout,
    QComboBox, QSlider, QLabel, QSpinBox, QPushButton, QCheckBox, QFrame,
)
from PyQt6.QtCore import Qt, QTimer, QPoint
from PyQt6.QtGui import QFont, QColor, QIcon, QPainter

from tacklecast.capture import MpvCapture
from tacklecast.audio import AudioPassthrough
from tacklecast.devices import enumerate_video_devices, enumerate_audio_inputs, enumerate_audio_outputs
from tacklecast.audio import find_audio_input_for_video
from tacklecast.overlay import OverlayWidget
from tacklecast.settings import (
    Settings, RESOLUTIONS, MIN_FPS, MAX_FPS,
    FPS_MODE_60, FPS_MODE_120, FPS_MODE_CUSTOM, get_capture_config,
)
from tacklecast.logger import setup_logger, get_logger


DARK_STYLE = """
QMainWindow {
    background-color: #0a0a14;
}
QComboBox {
    background-color: #16213e;
    color: #e0e0e0;
    border: 1px solid #0f3460;
    border-radius: 4px;
    padding: 4px 8px;
    min-width: 120px;
}
QComboBox::drop-down {
    border: none;
    width: 20px;
}
QComboBox::down-arrow {
    image: none;
    border-left: 4px solid transparent;
    border-right: 4px solid transparent;
    border-top: 5px solid #8899aa;
    margin-right: 6px;
}
QComboBox QAbstractItemView {
    background-color: #16213e;
    color: #e0e0e0;
    selection-background-color: #e94560;
    border: 1px solid #0f3460;
}
QSlider::groove:horizontal {
    background: #0f3460;
    height: 6px;
    border-radius: 3px;
}
QSlider::handle:horizontal {
    background: #e94560;
    width: 14px;
    height: 14px;
    margin: -4px 0;
    border-radius: 7px;
}
QSlider::sub-page:horizontal {
    background: #e94560;
    border-radius: 3px;
}
QSpinBox {
    background-color: #16213e;
    color: #e0e0e0;
    border: 1px solid #0f3460;
    border-radius: 4px;
    padding: 2px 4px;
    max-width: 60px;
}
QSpinBox::up-button, QSpinBox::down-button {
    width: 0px;
}
"""

MENU_STYLE = """
QWidget#PauseMenu {
    background-color: rgba(12, 12, 28, 240);
    border: 1px solid #1a2a50;
    border-radius: 12px;
}
"""

MENU_BUTTON = """
QPushButton {
    background-color: #16213e;
    color: #e0e0e0;
    border: 1px solid #0f3460;
    border-radius: 6px;
    padding: 8px 16px;
}
QPushButton:hover {
    background-color: #1a2a50;
    border-color: #e94560;
}
"""

EXIT_BUTTON = """
QPushButton {
    background-color: #3a1020;
    color: #e94560;
    border: 1px solid #e94560;
    border-radius: 6px;
    padding: 8px 16px;
    font-weight: bold;
}
QPushButton:hover {
    background-color: #e94560;
    color: #ffffff;
}
"""


class NoScrollComboBox(QComboBox):
    """QComboBox that ignores scroll wheel events."""

    def wheelEvent(self, event):
        event.ignore()


class NoScrollSpinBox(QSpinBox):
    """QSpinBox that ignores scroll wheel events."""

    def wheelEvent(self, event):
        event.ignore()


class VideoContainer(QWidget):
    """Black container that mpv renders into."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setAttribute(Qt.WidgetAttribute.WA_NativeWindow, True)
        self.setMouseTracking(True)
        pal = self.palette()
        pal.setColor(self.backgroundRole(), QColor(0, 0, 0))
        self.setPalette(pal)
        self.setAutoFillBackground(True)

    def get_wid(self):
        return int(self.winId())


class DimOverlay(QWidget):
    """Semi-transparent overlay that dims the video when the pause menu is open."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents, False)
        self.setAutoFillBackground(False)
        self.hide()

    def paintEvent(self, event):
        painter = QPainter(self)
        painter.fillRect(self.rect(), QColor(0, 0, 0, 120))
        painter.end()

    def mousePressEvent(self, event):
        parent = self.parent()
        if hasattr(parent, '_close_menu'):
            parent._close_menu()


class PauseMenu(QWidget):
    """Centered settings panel styled as a pause menu."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setObjectName("PauseMenu")
        self.setStyleSheet(MENU_STYLE)
        self.hide()

        layout = QVBoxLayout(self)
        layout.setContentsMargins(28, 24, 28, 24)
        layout.setSpacing(8)

        # Title
        self.title = QLabel("Settings")
        self.title.setAlignment(Qt.AlignmentFlag.AlignCenter)
        layout.addWidget(self.title)

        layout.addSpacing(10)

        # --- Video section ---
        self.video_section_lbl = self._section_label("VIDEO")
        layout.addWidget(self.video_section_lbl)

        self.video_device_lbl = self._field_label("Video Device")
        layout.addWidget(self.video_device_lbl)
        self.video_combo = NoScrollComboBox()
        layout.addWidget(self.video_combo)

        row = QHBoxLayout()
        row.setSpacing(12)
        res_col = QVBoxLayout()
        res_col.setSpacing(4)
        self.res_lbl = self._field_label("Resolution")
        res_col.addWidget(self.res_lbl)
        self.resolution_combo = NoScrollComboBox()
        res_col.addWidget(self.resolution_combo)
        row.addLayout(res_col)

        fps_col = QVBoxLayout()
        fps_col.setSpacing(4)
        self.fps_lbl = self._field_label("Frame Rate")
        fps_col.addWidget(self.fps_lbl)
        self.fps_combo = NoScrollComboBox()
        self.fps_combo.addItem("60 FPS", FPS_MODE_60)
        self.fps_combo.addItem("120 FPS", FPS_MODE_120)
        self.fps_combo.addItem("Custom", FPS_MODE_CUSTOM)
        fps_col.addWidget(self.fps_combo)
        row.addLayout(fps_col)
        layout.addLayout(row)

        # Custom FPS spinbox (hidden by default)
        custom_row = QHBoxLayout()
        custom_row.setSpacing(8)
        self.custom_fps_label = self._field_label("Custom FPS:")
        custom_row.addWidget(self.custom_fps_label)
        self.custom_fps_spin = NoScrollSpinBox()
        self.custom_fps_spin.setRange(MIN_FPS, MAX_FPS)
        self.custom_fps_spin.setValue(120)
        custom_row.addWidget(self.custom_fps_spin)
        custom_row.addStretch()
        self.custom_fps_container = QWidget()
        self.custom_fps_container.setLayout(custom_row)
        self.custom_fps_container.hide()
        layout.addWidget(self.custom_fps_container)

        # FPS warning label
        self.fps_warning = QLabel()
        self.fps_warning.setWordWrap(True)
        self.fps_warning.hide()
        layout.addWidget(self.fps_warning)

        layout.addSpacing(6)
        layout.addWidget(self._separator())
        layout.addSpacing(6)

        # --- Audio section ---
        self.audio_section_lbl = self._section_label("AUDIO")
        layout.addWidget(self.audio_section_lbl)

        self.audio_in_lbl = self._field_label("Audio Input")
        layout.addWidget(self.audio_in_lbl)
        self.audio_in_combo = NoScrollComboBox()
        layout.addWidget(self.audio_in_combo)

        self.audio_out_lbl = self._field_label("Audio Output")
        layout.addWidget(self.audio_out_lbl)
        self.audio_out_combo = NoScrollComboBox()
        layout.addWidget(self.audio_out_combo)

        vol_row = QHBoxLayout()
        vol_row.setSpacing(8)
        self.vol_lbl = self._field_label("Volume")
        vol_row.addWidget(self.vol_lbl)
        self.volume_slider = QSlider(Qt.Orientation.Horizontal)
        self.volume_slider.setRange(0, 100)
        self.volume_slider.setValue(100)
        vol_row.addWidget(self.volume_slider)
        self.volume_label = QLabel("100%")
        self.volume_label.setFixedWidth(40)
        vol_row.addWidget(self.volume_label)
        layout.addLayout(vol_row)

        layout.addSpacing(6)
        layout.addWidget(self._separator())
        layout.addSpacing(6)

        # --- Display section ---
        self.display_section_lbl = self._section_label("DISPLAY")
        layout.addWidget(self.display_section_lbl)

        btn_row = QHBoxLayout()
        btn_row.setSpacing(12)

        self.fullscreen_btn = QPushButton("Enter Fullscreen")
        self.fullscreen_btn.setStyleSheet(MENU_BUTTON)
        btn_row.addWidget(self.fullscreen_btn)

        self.overlay_cb = QCheckBox("Show FPS Overlay")
        self.overlay_cb.setChecked(True)
        btn_row.addWidget(self.overlay_cb)

        layout.addLayout(btn_row)

        layout.addSpacing(14)
        layout.addWidget(self._separator())
        layout.addSpacing(10)

        # --- Exit button ---
        self.exit_btn = QPushButton("Exit TackleCast")
        self.exit_btn.setStyleSheet(EXIT_BUTTON)
        layout.addWidget(self.exit_btn)

        layout.addSpacing(6)

        # Hint at bottom
        self.hint_label = QLabel("Press Escape to close")
        self.hint_label.setAlignment(Qt.AlignmentFlag.AlignCenter)
        layout.addWidget(self.hint_label)

        # Keep references to labels that need scaling
        self._all_labels = []

    def apply_scale(self, window_width):
        """Scale the menu width and font sizes based on window width."""
        # Scale menu width: 460 at 1280px, up to 600 at 2560px+, down to 380 at 720px
        scale = max(0.8, min(1.4, window_width / 1280))
        menu_width = int(460 * scale)
        self.setFixedWidth(menu_width)

        # Font sizes scale with the menu
        title_size = int(18 * scale)
        section_size = int(12 * scale)
        field_size = int(11 * scale)
        warning_size = int(11 * scale)
        button_size = int(11 * scale)
        hint_size = int(9 * scale)
        combo_size = int(10 * scale)

        self.title.setStyleSheet(f"color: #e0e0e0; font-size: {title_size}px; font-weight: bold;")

        section_style = f"color: #e94560; font-size: {section_size}px; font-weight: bold; letter-spacing: 1px;"
        for lbl in [self.video_section_lbl, self.audio_section_lbl, self.display_section_lbl]:
            lbl.setStyleSheet(section_style)

        field_style = f"color: #8899aa; font-size: {field_size}px;"
        for lbl in [self.video_device_lbl, self.res_lbl, self.fps_lbl,
                    self.audio_in_lbl, self.audio_out_lbl, self.vol_lbl,
                    self.custom_fps_label]:
            lbl.setStyleSheet(field_style)

        self.volume_label.setStyleSheet(f"color: #8899aa; font-size: {field_size}px;")

        warning_style = f"color: #e94560; font-size: {warning_size}px; font-style: italic; padding: 4px 0px;"
        self.fps_warning.setStyleSheet(warning_style)

        self.overlay_cb.setStyleSheet(f"color: #e0e0e0; font-size: {field_size}px; spacing: 6px;")

        self.hint_label.setStyleSheet(f"color: #445566; font-size: {hint_size}px;")

        # Scale buttons
        btn_style = MENU_BUTTON.replace("padding: 8px 16px;", f"padding: {int(8*scale)}px {int(16*scale)}px;")
        self.fullscreen_btn.setStyleSheet(btn_style)
        exit_style = EXIT_BUTTON.replace("padding: 8px 16px;", f"padding: {int(8*scale)}px {int(16*scale)}px;")
        self.exit_btn.setStyleSheet(exit_style)

        # Scale combo box font via per-widget stylesheet
        combo_style = f"font-size: {combo_size}px;"
        for combo in [self.video_combo, self.resolution_combo, self.fps_combo,
                      self.audio_in_combo, self.audio_out_combo]:
            combo.setStyleSheet(combo_style)
        self.custom_fps_spin.setStyleSheet(f"font-size: {combo_size}px; max-width: {int(60*scale)}px;")

    def _section_label(self, text):
        lbl = QLabel(text)
        return lbl

    def _field_label(self, text):
        lbl = QLabel(text)
        return lbl

    def _separator(self):
        line = QFrame()
        line.setFrameShape(QFrame.Shape.HLine)
        line.setStyleSheet("color: #1a2a50;")
        return line


class MainWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("TackleCast")
        self.resize(1280, 720)
        self.setMouseTracking(True)

        self.settings = Settings.load()
        self.capture = MpvCapture()
        self.audio = AudioPassthrough()
        self._populating = True
        self._menu_open = False

        # Video container
        self.video_container = VideoContainer(self)
        self.setCentralWidget(self.video_container)

        # Dim overlay (child of main window, covers the video)
        self.dim_overlay = DimOverlay(self)
        self.dim_overlay.hide()

        # Pause menu (child of main window, centered)
        self.menu = PauseMenu(self)
        self.menu.hide()

        # Floating overlay (top-level, stays on top of mpv)
        self.overlay = OverlayWidget()
        self.overlay.set_show_stats(self.settings.show_overlay)
        self.overlay.show()

        # Polling timer — keeps overlay positioned and polls mpv stats
        self._mouse_timer = QTimer()
        self._mouse_timer.timeout.connect(self._check_mouse)
        self._mouse_timer.start(100)

        # Populate controls
        self._populate_devices()
        self._populate_resolutions()
        self._populate_fps()
        self._populating = False

        # Connect UI-only signals (no settings/capture changes while menu is open)
        self.menu.fps_combo.currentIndexChanged.connect(self._on_fps_mode_ui_changed)
        self.menu.volume_slider.valueChanged.connect(self._on_volume_ui_changed)
        self.menu.fullscreen_btn.clicked.connect(self._toggle_fullscreen)
        self.menu.exit_btn.clicked.connect(self.close)

        # Restore overlay checkbox
        self.menu.overlay_cb.setChecked(self.settings.show_overlay)

        # Start after window is shown
        QTimer.singleShot(200, self._initial_start)

    def _initial_start(self):
        self._position_floating_widgets()
        self._start_capture()
        self._start_audio()

    def _populate_devices(self):
        video_devs = enumerate_video_devices()
        for name, device_id in video_devs:
            self.menu.video_combo.addItem(name, device_id)

        selected = False
        if self.settings.video_device:
            idx = self.menu.video_combo.findData(self.settings.video_device)
            if idx >= 0:
                self.menu.video_combo.setCurrentIndex(idx)
                selected = True
        if not selected:
            for i in range(self.menu.video_combo.count()):
                name = self.menu.video_combo.itemText(i).lower()
                if any(kw in name for kw in ["shadowcast", "capture", "elgato", "avermedia", "cam link"]):
                    self.menu.video_combo.setCurrentIndex(i)
                    break

        audio_ins = enumerate_audio_inputs()
        self.menu.audio_in_combo.addItem("Default", -1)
        for idx, name in audio_ins:
            self.menu.audio_in_combo.addItem(name, idx)
        if self.settings.audio_input >= 0:
            saved_idx = self.menu.audio_in_combo.findData(self.settings.audio_input)
            if saved_idx >= 0:
                self.menu.audio_in_combo.setCurrentIndex(saved_idx)

        audio_outs = enumerate_audio_outputs()
        self.menu.audio_out_combo.addItem("Default", -1)
        for idx, name in audio_outs:
            self.menu.audio_out_combo.addItem(name, idx)
        if self.settings.audio_output >= 0:
            saved_idx = self.menu.audio_out_combo.findData(self.settings.audio_output)
            if saved_idx >= 0:
                self.menu.audio_out_combo.setCurrentIndex(saved_idx)

        self.menu.volume_slider.setValue(int(self.settings.volume * 100))

    def _populate_resolutions(self):
        for name in RESOLUTIONS:
            self.menu.resolution_combo.addItem(name, name)
        saved_idx = self.menu.resolution_combo.findData(self.settings.resolution)
        if saved_idx >= 0:
            self.menu.resolution_combo.setCurrentIndex(saved_idx)

    def _populate_fps(self):
        mode = self.settings.fps_mode
        idx = self.menu.fps_combo.findData(mode)
        if idx >= 0:
            self.menu.fps_combo.setCurrentIndex(idx)
        self.menu.custom_fps_spin.setValue(self.settings.custom_fps)
        self._update_fps_ui(mode)

    def _update_fps_ui(self, mode):
        """Update FPS warning text and custom spinbox visibility (UI only)."""
        if mode == FPS_MODE_120:
            self.menu.fps_warning.setText(
                "A fast CPU is required for 120 FPS. Performance may vary by hardware."
            )
            self.menu.fps_warning.show()
            self.menu.custom_fps_container.hide()
        elif mode == FPS_MODE_CUSTOM:
            self.menu.fps_warning.setText(
                "Custom FPS is experimental and is not guaranteed to work with all devices."
            )
            self.menu.fps_warning.show()
            self.menu.custom_fps_container.show()
        else:
            self.menu.fps_warning.hide()
            self.menu.custom_fps_container.hide()

        self.menu.adjustSize()
        self._position_menu()

    def _on_fps_mode_ui_changed(self):
        """React to FPS dropdown change — just update the UI, don't apply yet."""
        mode = self.menu.fps_combo.currentData()
        self._update_fps_ui(mode)

    def _on_volume_ui_changed(self, value):
        """Update volume label text as user drags the slider (UI only)."""
        self.menu.volume_label.setText(f"{value}%")

    # --- Snapshot / apply / revert ---

    def _snapshot_menu_state(self):
        """Capture the current menu widget state so we can revert on cancel."""
        self._snap = {
            "video_idx": self.menu.video_combo.currentIndex(),
            "audio_in_idx": self.menu.audio_in_combo.currentIndex(),
            "audio_out_idx": self.menu.audio_out_combo.currentIndex(),
            "resolution_idx": self.menu.resolution_combo.currentIndex(),
            "fps_idx": self.menu.fps_combo.currentIndex(),
            "custom_fps": self.menu.custom_fps_spin.value(),
            "volume": self.menu.volume_slider.value(),
            "overlay": self.menu.overlay_cb.isChecked(),
        }

    def _apply_menu_settings(self):
        """Read the current menu state, save settings, and restart capture/audio if needed."""
        new_video = self.menu.video_combo.currentData() or ""
        new_audio_in = self.menu.audio_in_combo.currentData() or -1
        new_audio_out = self.menu.audio_out_combo.currentData() or -1
        new_resolution = self.menu.resolution_combo.currentData() or "1080p"
        new_fps_mode = self.menu.fps_combo.currentData() or FPS_MODE_60
        new_custom_fps = self.menu.custom_fps_spin.value()
        new_volume = self.menu.volume_slider.value() / 100.0
        new_show_overlay = self.menu.overlay_cb.isChecked()

        # Determine what changed
        video_changed = (
            new_video != self.settings.video_device
            or new_resolution != self.settings.resolution
            or new_fps_mode != self.settings.fps_mode
            or (new_fps_mode == FPS_MODE_CUSTOM and new_custom_fps != self.settings.custom_fps)
        )
        audio_changed = (
            new_audio_in != self.settings.audio_input
            or new_audio_out != self.settings.audio_output
            or new_video != self.settings.video_device
        )
        volume_changed = abs(new_volume - self.settings.volume) > 0.001
        overlay_changed = new_show_overlay != self.settings.show_overlay

        # Update settings
        self.settings.video_device = new_video
        self.settings.audio_input = new_audio_in
        self.settings.audio_output = new_audio_out
        self.settings.resolution = new_resolution
        self.settings.fps_mode = new_fps_mode
        self.settings.custom_fps = new_custom_fps
        self.settings.volume = new_volume
        self.settings.show_overlay = new_show_overlay
        self.settings.save()

        # Apply changes
        if video_changed:
            self._start_capture()
        if audio_changed:
            self._start_audio()
        if volume_changed:
            self.audio.set_volume(new_volume)
        if overlay_changed:
            self.overlay.set_show_stats(new_show_overlay)

    def _start_capture(self):
        log = get_logger()
        device_name = self.menu.video_combo.currentData()
        if not device_name:
            log.warning("No video device found")
            self.overlay.set_status("No video device found")
            return

        res_key = self.menu.resolution_combo.currentData() or "1080p"
        fps = self.settings.get_fps()
        w, h, fps, pixel_format, threads = get_capture_config(res_key, fps)
        wid = self.video_container.get_wid()
        log.info(f"Starting capture: device={device_name}, {res_key} @ {fps}fps ({w}x{h}, {pixel_format}, threads={threads})")

        self.overlay.set_status("Connecting...")
        self.capture.start(
            wid=wid,
            device_name=device_name,
            width=w, height=h, fps=fps,
            pixel_format=pixel_format,
            decode_threads=threads,
            on_fps_update=None,
            on_error=self._on_capture_error,
        )

    def _start_audio(self):
        log = get_logger()
        self.audio.stop()
        in_dev = self.menu.audio_in_combo.currentData()
        out_dev = self.menu.audio_out_combo.currentData()
        if in_dev is None:
            return
        in_dev = in_dev if in_dev >= 0 else None
        out_dev = out_dev if out_dev is not None and out_dev >= 0 else None

        # Auto-detect capture card audio when input is "Default"
        if in_dev is None:
            video_name = self.menu.video_combo.currentData() or ""
            matched = find_audio_input_for_video(video_name)
            if matched is not None:
                log.info(f"Audio auto-detect: matched input device {matched} for video '{video_name}'")
                in_dev = matched
            else:
                log.warning(f"Audio auto-detect: no match found for video '{video_name}', using system default")

        volume = self.settings.volume
        log.info(f"Starting audio: input={in_dev}, output={out_dev}, volume={volume:.2f}")
        self.audio.start(in_dev, out_dev, volume)

    def _on_capture_error(self, msg):
        get_logger().error(f"Capture error: {msg}")
        QTimer.singleShot(0, lambda: self.overlay.set_status(msg))

    # --- Menu and fullscreen ---

    def _toggle_fullscreen(self):
        if self.isFullScreen():
            self.showNormal()
        else:
            self.showFullScreen()
        QTimer.singleShot(100, self._sync_fullscreen_button)
        QTimer.singleShot(100, self._position_floating_widgets)

    def _sync_fullscreen_button(self):
        if self.isFullScreen():
            self.menu.fullscreen_btn.setText("Exit Fullscreen")
        else:
            self.menu.fullscreen_btn.setText("Enter Fullscreen")

    def _position_menu(self):
        """Center the pause menu within the window."""
        if not self.isVisible():
            return
        self.menu.adjustSize()
        menu_w = self.menu.width()
        menu_h = self.menu.height()
        x = (self.width() - menu_w) // 2
        y = (self.height() - menu_h) // 2
        self.menu.move(x, y)

    def _open_menu(self):
        """Open the pause menu and snapshot current state."""
        self._snapshot_menu_state()
        self._sync_fullscreen_button()
        self.menu.apply_scale(self.width())
        self.dim_overlay.setGeometry(self.rect())
        self.dim_overlay.show()
        self.dim_overlay.raise_()
        self._position_menu()
        self.menu.show()
        self.menu.raise_()
        self._menu_open = True

    def _close_menu(self):
        """Close the menu and apply any changed settings."""
        self.menu.hide()
        self.dim_overlay.hide()
        self._menu_open = False
        self._apply_menu_settings()

    def _toggle_menu(self):
        """Toggle the pause menu on/off."""
        if self._menu_open:
            self._close_menu()
        else:
            self._open_menu()

    def _position_floating_widgets(self):
        """Position overlay relative to main window."""
        if not self.isVisible():
            return
        top_left = self.video_container.mapToGlobal(QPoint(4, 4))
        self.overlay.move(top_left)
        self.overlay.setFixedWidth(min(500, self.width()))

        if self._menu_open:
            self.dim_overlay.setGeometry(self.rect())
            self.menu.apply_scale(self.width())
            self._position_menu()

    def _check_mouse(self):
        """Keep overlay positioned and poll mpv stats."""
        if self.isVisible():
            top_left = self.video_container.mapToGlobal(QPoint(4, 4))
            if self.overlay.pos() != top_left:
                self.overlay.move(top_left)
            self.overlay.raise_()

        stats = self.capture.poll_stats()
        if stats:
            fps, w, h = stats
            self.overlay.set_status("")
            self.overlay.update_stats(fps, w, h)

    def keyPressEvent(self, event):
        if event.key() == Qt.Key.Key_F11:
            self._toggle_fullscreen()
        elif event.key() == Qt.Key.Key_Escape:
            self._toggle_menu()
        super().keyPressEvent(event)

    def moveEvent(self, event):
        super().moveEvent(event)
        self._position_floating_widgets()

    def resizeEvent(self, event):
        super().resizeEvent(event)
        self._position_floating_widgets()

    def closeEvent(self, event):
        self._mouse_timer.stop()
        self.capture.stop()
        self.audio.stop()
        # Save current state on exit even if menu wasn't opened
        self.settings.save()
        self.overlay.close()
        super().closeEvent(event)


def main():
    import os
    import platform
    import ctypes

    log = setup_logger()
    log.info("====== TackleCast starting ======")
    log.info(f"Platform: {platform.platform()}")
    log.info(f"Python: {sys.version}")
    log.info(f"Frozen: {getattr(sys, 'frozen', False)}")

    ctypes.windll.shell32.SetCurrentProcessExplicitAppUserModelID("tacklecast.tacklecast.v1")

    app = QApplication(sys.argv)
    app.setApplicationName("TackleCast")
    font = QFont("Segoe UI", 9)
    app.setFont(font)
    app.setStyleSheet(DARK_STYLE)

    if getattr(sys, '_MEIPASS', None):
        base = sys._MEIPASS
    elif getattr(sys, 'frozen', False):
        base = os.path.dirname(sys.executable)
    else:
        base = os.path.dirname(os.path.dirname(__file__))
    icon_path = os.path.join(base, "assets", "icon.ico")
    if os.path.exists(icon_path):
        app.setWindowIcon(QIcon(icon_path))

    window = MainWindow()
    window.show()
    sys.exit(app.exec())
