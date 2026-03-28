import sys

from PyQt6.QtWidgets import (
    QApplication, QMainWindow, QWidget, QHBoxLayout,
    QComboBox, QSlider, QLabel,
)
from PyQt6.QtCore import Qt, QTimer, QPoint
from PyQt6.QtGui import QFont, QColor, QIcon

from tacklecast.capture import MpvCapture
from tacklecast.audio import AudioPassthrough
from tacklecast.devices import enumerate_video_devices, enumerate_audio_inputs, enumerate_audio_outputs
from tacklecast.overlay import OverlayWidget
from tacklecast.settings import Settings, RESOLUTION_PRESETS
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
"""


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


class ControlBar(QWidget):
    """Frameless top-level floating control bar."""

    def __init__(self, parent=None):
        super().__init__(parent,
                         Qt.WindowType.FramelessWindowHint |
                         Qt.WindowType.Tool)
        self.setAttribute(Qt.WidgetAttribute.WA_ShowWithoutActivating)
        self.setFixedHeight(44)
        self.setAutoFillBackground(True)

        pal = self.palette()
        pal.setColor(self.backgroundRole(), QColor(10, 10, 25, 230))
        self.setPalette(pal)

        layout = QHBoxLayout(self)
        layout.setContentsMargins(12, 6, 12, 6)
        layout.setSpacing(8)

        lbl = QLabel("Video:")
        lbl.setStyleSheet("color: #8899aa;")
        layout.addWidget(lbl)
        self.video_combo = QComboBox()
        self.video_combo.setMaximumWidth(220)
        layout.addWidget(self.video_combo)

        lbl = QLabel("Audio In:")
        lbl.setStyleSheet("color: #8899aa;")
        layout.addWidget(lbl)
        self.audio_in_combo = QComboBox()
        self.audio_in_combo.setMaximumWidth(200)
        layout.addWidget(self.audio_in_combo)

        lbl = QLabel("Out:")
        lbl.setStyleSheet("color: #8899aa;")
        layout.addWidget(lbl)
        self.audio_out_combo = QComboBox()
        self.audio_out_combo.setMaximumWidth(200)
        layout.addWidget(self.audio_out_combo)

        lbl = QLabel("Res:")
        lbl.setStyleSheet("color: #8899aa;")
        layout.addWidget(lbl)
        self.resolution_combo = QComboBox()
        self.resolution_combo.setMaximumWidth(200)
        layout.addWidget(self.resolution_combo)

        lbl = QLabel("Vol:")
        lbl.setStyleSheet("color: #8899aa;")
        layout.addWidget(lbl)
        self.volume_slider = QSlider(Qt.Orientation.Horizontal)
        self.volume_slider.setRange(0, 100)
        self.volume_slider.setValue(100)
        self.volume_slider.setFixedWidth(100)
        layout.addWidget(self.volume_slider)

        self.volume_label = QLabel("100%")
        self.volume_label.setStyleSheet("color: #8899aa;")
        self.volume_label.setFixedWidth(36)
        layout.addWidget(self.volume_label)

        layout.addStretch()


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

        # Video container
        self.video_container = VideoContainer(self)
        self.setCentralWidget(self.video_container)

        # Floating overlay (top-level, stays on top of mpv)
        self.overlay = OverlayWidget()
        self.overlay.show()

        # Floating control bar (top-level)
        self.control_bar = ControlBar()
        self.control_bar.hide()

        # Auto-hide timer
        self._hide_timer = QTimer()
        self._hide_timer.setSingleShot(True)
        self._hide_timer.timeout.connect(self._hide_controls)
        self._controls_pinned = False

        # Mouse tracking timer — poll mouse position since mpv eats events
        self._mouse_timer = QTimer()
        self._mouse_timer.timeout.connect(self._check_mouse)
        self._mouse_timer.start(100)

        # Populate controls
        self._populate_devices()
        self._populate_resolutions()
        self._populating = False

        # Connect signals
        self.control_bar.video_combo.currentIndexChanged.connect(self._on_video_device_changed)
        self.control_bar.audio_in_combo.currentIndexChanged.connect(self._on_audio_changed)
        self.control_bar.audio_out_combo.currentIndexChanged.connect(self._on_audio_changed)
        self.control_bar.resolution_combo.currentIndexChanged.connect(self._on_resolution_changed)
        self.control_bar.volume_slider.valueChanged.connect(self._on_volume_changed)

        # Start after window is shown
        QTimer.singleShot(200, self._initial_start)

    def _initial_start(self):
        self._position_floating_widgets()
        self._start_capture()
        self._start_audio()

    def _populate_devices(self):
        video_devs = enumerate_video_devices()
        for name, device_id in video_devs:
            self.control_bar.video_combo.addItem(name, device_id)

        selected = False
        if self.settings.video_device:
            idx = self.control_bar.video_combo.findData(self.settings.video_device)
            if idx >= 0:
                self.control_bar.video_combo.setCurrentIndex(idx)
                selected = True
        if not selected:
            for i in range(self.control_bar.video_combo.count()):
                name = self.control_bar.video_combo.itemText(i).lower()
                if any(kw in name for kw in ["shadowcast", "capture", "elgato", "avermedia", "cam link"]):
                    self.control_bar.video_combo.setCurrentIndex(i)
                    break

        audio_ins = enumerate_audio_inputs()
        self.control_bar.audio_in_combo.addItem("Default", -1)
        for idx, name in audio_ins:
            self.control_bar.audio_in_combo.addItem(name, idx)
        if self.settings.audio_input >= 0:
            saved_idx = self.control_bar.audio_in_combo.findData(self.settings.audio_input)
            if saved_idx >= 0:
                self.control_bar.audio_in_combo.setCurrentIndex(saved_idx)

        audio_outs = enumerate_audio_outputs()
        self.control_bar.audio_out_combo.addItem("Default", -1)
        for idx, name in audio_outs:
            self.control_bar.audio_out_combo.addItem(name, idx)
        if self.settings.audio_output >= 0:
            saved_idx = self.control_bar.audio_out_combo.findData(self.settings.audio_output)
            if saved_idx >= 0:
                self.control_bar.audio_out_combo.setCurrentIndex(saved_idx)

        self.control_bar.volume_slider.setValue(int(self.settings.volume * 100))

    def _populate_resolutions(self):
        for name in RESOLUTION_PRESETS:
            self.control_bar.resolution_combo.addItem(name, name)
        saved_idx = self.control_bar.resolution_combo.findData(self.settings.resolution)
        if saved_idx >= 0:
            self.control_bar.resolution_combo.setCurrentIndex(saved_idx)

    def _start_capture(self):
        log = get_logger()
        device_name = self.control_bar.video_combo.currentData()
        if not device_name:
            log.warning("No video device found")
            self.overlay.set_status("No video device found")
            return

        res_key = self.control_bar.resolution_combo.currentData() or "1080p @60"
        w, h, fps, pixel_format = RESOLUTION_PRESETS[res_key]
        wid = self.video_container.get_wid()
        log.info(f"Starting capture: device={device_name}, preset={res_key} ({w}x{h}@{fps}, {pixel_format})")

        self.overlay.set_status("Connecting...")
        self.capture.start(
            wid=wid,
            device_name=device_name,
            width=w, height=h, fps=fps,
            pixel_format=pixel_format,
            on_fps_update=None,
            on_error=self._on_capture_error,
        )

    def _start_audio(self):
        log = get_logger()
        self.audio.stop()
        in_dev = self.control_bar.audio_in_combo.currentData()
        out_dev = self.control_bar.audio_out_combo.currentData()
        if in_dev is None:
            return
        in_dev = in_dev if in_dev >= 0 else None
        out_dev = out_dev if out_dev is not None and out_dev >= 0 else None
        volume = self.control_bar.volume_slider.value() / 100.0
        log.info(f"Starting audio: input={in_dev}, output={out_dev}, volume={volume:.2f}")
        self.audio.start(in_dev, out_dev, volume)

    def _on_video_device_changed(self):
        if not self._populating:
            self._start_capture()
            self._save_settings()

    def _on_audio_changed(self):
        if not self._populating:
            self._start_audio()
            self._save_settings()

    def _on_resolution_changed(self):
        if not self._populating:
            self._start_capture()
            self._save_settings()

    def _on_volume_changed(self, value):
        self.control_bar.volume_label.setText(f"{value}%")
        self.audio.set_volume(value / 100.0)
        if not self._populating:
            self._save_settings()

    def _on_fps_updated(self, fps, width, height, latency_ms):
        QTimer.singleShot(0, lambda: self.overlay.update_stats(fps, width, height, latency_ms))

    def _on_capture_error(self, msg):
        get_logger().error(f"Capture error: {msg}")
        QTimer.singleShot(0, lambda: self.overlay.set_status(msg))

    def _save_settings(self):
        self.settings.video_device = self.control_bar.video_combo.currentData() or ""
        self.settings.audio_input = self.control_bar.audio_in_combo.currentData() or -1
        self.settings.audio_output = self.control_bar.audio_out_combo.currentData() or -1
        self.settings.resolution = self.control_bar.resolution_combo.currentData() or "1080p @60"
        self.settings.volume = self.control_bar.volume_slider.value() / 100.0
        self.settings.save()

    # --- Floating widget positioning ---

    def _position_floating_widgets(self):
        """Position overlay and control bar relative to main window."""
        if not self.isVisible():
            return
        # Overlay: top-left of window
        top_left = self.video_container.mapToGlobal(QPoint(4, 4))
        self.overlay.move(top_left)
        self.overlay.setFixedWidth(min(500, self.width()))

        # Control bar: bottom of window
        bar_h = self.control_bar.height()
        bottom_left = self.video_container.mapToGlobal(QPoint(0, self.video_container.height() - bar_h))
        self.control_bar.move(bottom_left)
        self.control_bar.setFixedWidth(self.video_container.width())

    def _check_mouse(self):
        """Poll mouse position, keep overlay visible, and update stats."""
        from PyQt6.QtGui import QCursor

        # Always keep overlay positioned and raised above mpv
        if self.isVisible():
            top_left = self.video_container.mapToGlobal(QPoint(4, 4))
            if self.overlay.pos() != top_left:
                self.overlay.move(top_left)
            self.overlay.raise_()

        # Poll mpv for resolution/FPS
        stats = self.capture.poll_stats()
        if stats:
            fps, w, h = stats
            self.overlay.set_status("")  # Clear "Connecting..."
            self.overlay.update_stats(fps, w, h)

        pos = QCursor.pos()
        geom = self.geometry()

        if not geom.contains(pos):
            return

        local_y = pos.y() - geom.y()
        window_h = geom.height()

        if local_y > window_h * 0.75:
            self._show_controls()
        elif self.control_bar.isVisible() and not self._controls_pinned:
            if not self.control_bar.geometry().contains(pos):
                self._hide_timer.start(3000)

    def _show_controls(self):
        self._position_floating_widgets()
        self.control_bar.show()
        self._hide_timer.start(3000)

    def _hide_controls(self):
        if not self._controls_pinned:
            self.control_bar.hide()

    def keyPressEvent(self, event):
        if event.key() == Qt.Key.Key_F11:
            if self.isFullScreen():
                self.showNormal()
            else:
                self.showFullScreen()
            QTimer.singleShot(100, self._position_floating_widgets)
        elif event.key() == Qt.Key.Key_Escape:
            if self.isFullScreen():
                self.showNormal()
                QTimer.singleShot(100, self._position_floating_widgets)
        elif event.key() == Qt.Key.Key_Tab:
            if self.control_bar.isVisible():
                self._controls_pinned = False
                self._hide_controls()
            else:
                self._controls_pinned = True
                self._show_controls()
                self._hide_timer.stop()
        else:
            self._show_controls()
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
        self._save_settings()
        self.overlay.close()
        self.control_bar.close()
        super().closeEvent(event)


def main():
    import os
    import platform
    import ctypes

    # Initialize logging before anything else
    log = setup_logger()
    log.info("====== TackleCast starting ======")
    log.info(f"Platform: {platform.platform()}")
    log.info(f"Python: {sys.version}")
    log.info(f"Frozen: {getattr(sys, 'frozen', False)}")

    # Set AppUserModelID so Windows shows our icon in the taskbar (not Python's)
    ctypes.windll.shell32.SetCurrentProcessExplicitAppUserModelID("tacklecast.tacklecast.v1")

    app = QApplication(sys.argv)
    app.setApplicationName("TackleCast")
    font = QFont("Segoe UI", 9)
    app.setFont(font)
    app.setStyleSheet(DARK_STYLE)

    # Set app icon — check PyInstaller bundle path first, then dev path
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
