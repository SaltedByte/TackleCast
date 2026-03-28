import sys

from PyQt6.QtWidgets import (
    QApplication, QMainWindow, QWidget, QHBoxLayout,
    QComboBox, QSlider, QLabel, QCheckBox, QSpinBox,
)
from PyQt6.QtCore import Qt, QTimer, QPoint
from PyQt6.QtGui import QFont, QColor, QIcon
from PyQt6.QtOpenGLWidgets import QOpenGLWidget
from OpenGL.GL import (
    glViewport, glClearColor, glClear, glEnable, glDisable,
    glGenTextures, glBindTexture, glTexImage2D, glTexSubImage2D,
    glTexParameteri, glActiveTexture, glDeleteTextures,
    glGenBuffers, glBindBuffer, glBufferData, glEnableVertexAttribArray,
    glVertexAttribPointer, glDrawArrays, glGenVertexArrays, glBindVertexArray,
    glPixelStorei,
    GL_COLOR_BUFFER_BIT, GL_TEXTURE_2D, GL_TEXTURE0, GL_TEXTURE1, GL_TEXTURE2,
    GL_RED, GL_R8, GL_UNSIGNED_BYTE, GL_LINEAR,
    GL_TEXTURE_MIN_FILTER, GL_TEXTURE_MAG_FILTER,
    GL_TEXTURE_WRAP_S, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE,
    GL_FLOAT, GL_TRIANGLE_STRIP, GL_ARRAY_BUFFER, GL_STATIC_DRAW,
    GL_UNPACK_ROW_LENGTH, GL_UNPACK_ALIGNMENT,
)

from PyQt6.QtOpenGL import QOpenGLShaderProgram, QOpenGLShader
import numpy as np
import ctypes

from tacklecast.capture import CaptureThread
from tacklecast.audio import AudioPassthrough
from tacklecast.devices import enumerate_video_devices, enumerate_audio_inputs, enumerate_audio_outputs
from tacklecast.audio import find_audio_input_for_video
from tacklecast.overlay import OverlayWidget
from tacklecast.settings import Settings, RESOLUTIONS, DEFAULT_FPS, MIN_FPS, MAX_FPS, get_capture_config
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
QCheckBox {
    color: #8899aa;
    spacing: 4px;
}
QCheckBox::indicator {
    width: 14px;
    height: 14px;
    border: 1px solid #0f3460;
    border-radius: 3px;
    background: #16213e;
}
QCheckBox::indicator:checked {
    background: #e94560;
    border-color: #e94560;
}
QSpinBox {
    background-color: #16213e;
    color: #e0e0e0;
    border: 1px solid #0f3460;
    border-radius: 4px;
    padding: 2px 4px;
    max-width: 50px;
}
QSpinBox::up-button, QSpinBox::down-button {
    width: 0px;
}
"""


VERTEX_SHADER = """
#version 330 core
layout(location = 0) in vec2 position;
layout(location = 1) in vec2 texcoord;
out vec2 v_texcoord;

uniform vec4 u_viewport;  // x, y, w, h in normalized [-1,1] coords

void main() {
    gl_Position = vec4(position * u_viewport.zw + u_viewport.xy, 0.0, 1.0);
    v_texcoord = texcoord;
}
"""

FRAGMENT_SHADER = """
#version 330 core
in vec2 v_texcoord;
out vec4 fragColor;

uniform sampler2D tex_y;
uniform sampler2D tex_u;
uniform sampler2D tex_v;
uniform int u_full_range;

void main() {
    float y = texture(tex_y, v_texcoord).r;
    float u = texture(tex_u, v_texcoord).r - 0.5;
    float v = texture(tex_v, v_texcoord).r - 0.5;

    // BT.601 YUV to RGB (MJPEG uses BT.601)
    if (u_full_range != 0) {
        // Full range (yuvj*): Y 0-255
        fragColor = vec4(
            clamp(y               + 1.402  * v, 0.0, 1.0),
            clamp(y - 0.34414 * u - 0.71414 * v, 0.0, 1.0),
            clamp(y + 1.772   * u              , 0.0, 1.0),
            1.0);
    } else {
        // Limited range (yuv*): Y 16-235
        y = 1.1643 * (y - 0.0625);
        fragColor = vec4(
            clamp(y               + 1.596  * v, 0.0, 1.0),
            clamp(y - 0.39173 * u - 0.81290 * v, 0.0, 1.0),
            clamp(y + 2.0172  * u              , 0.0, 1.0),
            1.0);
    }
}
"""


class VideoWidget(QOpenGLWidget):
    """Renders YUV420p video frames using OpenGL textures and a GLSL shader."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setMouseTracking(True)
        self._yuv_data = None  # (y_bytes, u_bytes, v_bytes, y_stride, uv_stride, full_range)
        self._frame_w = 0
        self._frame_h = 0
        self._program = None
        self._textures = None  # (tex_y, tex_u, tex_v)
        self._tex_w = 0
        self._tex_h = 0
        self._vao = None
        self._vbo = None

    def update_frame(self, frame_data, width, height):
        """Update the displayed frame. frame_data is (y, u, v, uv_w, uv_h, full_range)."""
        self._yuv_data = frame_data
        self._frame_w = width
        self._frame_h = height
        self.update()

    def initializeGL(self):
        glClearColor(0.0, 0.0, 0.0, 1.0)

        # Compile shader program
        self._program = QOpenGLShaderProgram(self)
        self._program.addShaderFromSourceCode(QOpenGLShader.ShaderTypeBit.Vertex, VERTEX_SHADER)
        self._program.addShaderFromSourceCode(QOpenGLShader.ShaderTypeBit.Fragment, FRAGMENT_SHADER)
        self._program.link()

        # Create VAO and VBO for a fullscreen quad
        # positions (clip space) + texcoords
        quad = np.array([
            -1, -1,  0, 1,   # bottom-left  (flip V: texcoord y=1 at bottom)
             1, -1,  1, 1,   # bottom-right
            -1,  1,  0, 0,   # top-left
             1,  1,  1, 0,   # top-right
        ], dtype=np.float32)

        self._vao = glGenVertexArrays(1)
        glBindVertexArray(self._vao)

        self._vbo = glGenBuffers(1)
        glBindBuffer(GL_ARRAY_BUFFER, self._vbo)
        glBufferData(GL_ARRAY_BUFFER, quad.nbytes, quad, GL_STATIC_DRAW)

        # position: location 0, 2 floats, stride 16, offset 0
        glEnableVertexAttribArray(0)
        glVertexAttribPointer(0, 2, GL_FLOAT, False, 16, ctypes.c_void_p(0))
        # texcoord: location 1, 2 floats, stride 16, offset 8
        glEnableVertexAttribArray(1)
        glVertexAttribPointer(1, 2, GL_FLOAT, False, 16, ctypes.c_void_p(8))

        glBindVertexArray(0)

        # Create 3 textures for Y, U, V planes
        self._textures = glGenTextures(3)
        for tex in self._textures:
            glBindTexture(GL_TEXTURE_2D, tex)
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR)
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR)
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE)
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE)

    def resizeGL(self, w, h):
        glViewport(0, 0, w, h)

    def paintGL(self):
        glClear(GL_COLOR_BUFFER_BIT)

        if self._yuv_data is None or self._program is None:
            return

        y_data, u_data, v_data, uv_w, uv_h, full_range = self._yuv_data
        w, h = self._frame_w, self._frame_h
        resized = (w != self._tex_w or h != self._tex_h)

        glPixelStorei(GL_UNPACK_ALIGNMENT, 1)
        glPixelStorei(GL_UNPACK_ROW_LENGTH, 0)

        # Upload Y plane (full resolution)
        glActiveTexture(GL_TEXTURE0)
        glBindTexture(GL_TEXTURE_2D, self._textures[0])
        if resized:
            glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, w, h, 0,
                         GL_RED, GL_UNSIGNED_BYTE, y_data)
        else:
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, w, h,
                            GL_RED, GL_UNSIGNED_BYTE, y_data)

        # Upload U plane (chroma resolution from capture)
        glActiveTexture(GL_TEXTURE1)
        glBindTexture(GL_TEXTURE_2D, self._textures[1])
        if resized:
            glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, uv_w, uv_h, 0,
                         GL_RED, GL_UNSIGNED_BYTE, u_data)
        else:
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, uv_w, uv_h,
                            GL_RED, GL_UNSIGNED_BYTE, u_data)

        # Upload V plane (same chroma resolution)
        glActiveTexture(GL_TEXTURE2)
        glBindTexture(GL_TEXTURE_2D, self._textures[2])
        if resized:
            glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, uv_w, uv_h, 0,
                         GL_RED, GL_UNSIGNED_BYTE, v_data)
        else:
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, uv_w, uv_h,
                            GL_RED, GL_UNSIGNED_BYTE, v_data)

        self._tex_w = w
        self._tex_h = h

        # Calculate aspect-ratio-correct viewport in normalized coords
        win_w, win_h = self.width(), self.height()
        src_aspect = w / h
        win_aspect = win_w / win_h

        if win_aspect > src_aspect:
            scale_x = src_aspect / win_aspect
            scale_y = 1.0
        else:
            scale_x = 1.0
            scale_y = win_aspect / src_aspect

        self._program.bind()
        self._program.setUniformValue("tex_y", 0)
        self._program.setUniformValue("tex_u", 1)
        self._program.setUniformValue("tex_v", 2)
        self._program.setUniformValue("u_full_range", int(full_range))
        self._program.setUniformValue("u_viewport", 0.0, 0.0, scale_x, scale_y)

        glBindVertexArray(self._vao)
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4)
        glBindVertexArray(0)

        self._program.release()


class ControlBar(QWidget):
    """Child widget control bar that sits at the bottom of the video."""

    def __init__(self, parent=None):
        super().__init__(parent)
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
        self.resolution_combo.setMaximumWidth(120)
        layout.addWidget(self.resolution_combo)

        lbl = QLabel("FPS:")
        lbl.setStyleSheet("color: #8899aa;")
        layout.addWidget(lbl)
        self.fps_label = QLabel("60")
        self.fps_label.setStyleSheet("color: #e0e0e0;")
        self.fps_label.setFixedWidth(24)
        layout.addWidget(self.fps_label)

        self.fps_spin = QSpinBox()
        self.fps_spin.setRange(MIN_FPS, MAX_FPS)
        self.fps_spin.setValue(DEFAULT_FPS)
        self.fps_spin.setVisible(False)
        layout.addWidget(self.fps_spin)

        self.experimental_cb = QCheckBox("Experimental")
        layout.addWidget(self.experimental_cb)

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
        self.capture = CaptureThread()
        self.audio = AudioPassthrough()
        self._populating = True

        # Video widget
        self.video_container = VideoWidget(self)
        self.setCentralWidget(self.video_container)

        # Overlay and control bar are children of the main window,
        # positioned manually on top of the video surface
        self.overlay = OverlayWidget(self)
        self.overlay.show()
        self.overlay.raise_()

        self.control_bar = ControlBar(self)
        self.control_bar.hide()

        # Frame polling timer — drives rendering and overlay updates
        self._render_timer = QTimer()
        self._render_timer.timeout.connect(self._poll_frame)
        self._render_timer.start(8)  # ~120fps capable polling

        # Populate controls
        self._populate_devices()
        self._populate_resolutions()
        self._populating = False

        # Connect signals
        self.control_bar.video_combo.currentIndexChanged.connect(self._on_video_device_changed)
        self.control_bar.audio_in_combo.currentIndexChanged.connect(self._on_audio_changed)
        self.control_bar.audio_out_combo.currentIndexChanged.connect(self._on_audio_changed)
        self.control_bar.resolution_combo.currentIndexChanged.connect(self._on_resolution_changed)
        self.control_bar.experimental_cb.toggled.connect(self._on_experimental_toggled)
        self.control_bar.fps_spin.valueChanged.connect(self._on_fps_changed)
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
        for name in RESOLUTIONS:
            self.control_bar.resolution_combo.addItem(name, name)
        saved_idx = self.control_bar.resolution_combo.findData(self.settings.resolution)
        if saved_idx >= 0:
            self.control_bar.resolution_combo.setCurrentIndex(saved_idx)

        # Restore FPS and experimental state
        self.control_bar.fps_spin.setValue(self.settings.fps)
        self.control_bar.experimental_cb.setChecked(self.settings.experimental_fps)
        self._on_experimental_toggled(self.settings.experimental_fps)

    def _get_fps(self):
        """Get the current FPS — 60 unless experimental is enabled."""
        if self.control_bar.experimental_cb.isChecked():
            return self.control_bar.fps_spin.value()
        return DEFAULT_FPS

    def _start_capture(self):
        log = get_logger()
        device_name = self.control_bar.video_combo.currentData()
        if not device_name:
            log.warning("No video device found")
            self.overlay.set_status("No video device found")
            return

        res_key = self.control_bar.resolution_combo.currentData() or "1080p"
        fps = self._get_fps()
        w, h, fps, pixel_format, threads = get_capture_config(res_key, fps)
        log.info(f"Starting capture: device={device_name}, {res_key} @ {fps}fps ({w}x{h}, {pixel_format})")

        self.overlay.set_status("Connecting...")
        self.capture.start(
            device_name=device_name,
            width=w, height=h, fps=fps,
            pixel_format=pixel_format,
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

        # Auto-detect capture card audio when input is "Default"
        if in_dev is None:
            video_name = self.control_bar.video_combo.currentData() or ""
            matched = find_audio_input_for_video(video_name)
            if matched is not None:
                log.info(f"Audio auto-detect: matched input device {matched} for video '{video_name}'")
                in_dev = matched
            else:
                log.warning(f"Audio auto-detect: no match found for video '{video_name}', using system default")

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

    def _on_experimental_toggled(self, checked):
        self.control_bar.fps_spin.setVisible(checked)
        self.control_bar.fps_label.setVisible(not checked)
        if not self._populating:
            if not checked:
                self.control_bar.fps_spin.setValue(DEFAULT_FPS)
            self._start_capture()
            self._save_settings()

    def _on_fps_changed(self):
        if not self._populating and self.control_bar.experimental_cb.isChecked():
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
        self.settings.resolution = self.control_bar.resolution_combo.currentData() or "1080p"
        self.settings.fps = self.control_bar.fps_spin.value()
        self.settings.experimental_fps = self.control_bar.experimental_cb.isChecked()
        self.settings.volume = self.control_bar.volume_slider.value() / 100.0
        self.settings.save()

    # --- Floating widget positioning ---

    def _position_floating_widgets(self):
        """Position overlay and control bar relative to the video container."""
        if not self.isVisible():
            return
        # Overlay: top-left of video area
        vid_pos = self.video_container.pos()
        self.overlay.move(vid_pos.x() + 4, vid_pos.y() + 4)
        self.overlay.setFixedWidth(min(500, self.video_container.width()))
        self.overlay.raise_()

        # Control bar: bottom of video area
        bar_h = self.control_bar.height()
        self.control_bar.move(vid_pos.x(), vid_pos.y() + self.video_container.height() - bar_h)
        self.control_bar.setFixedWidth(self.video_container.width())
        self.control_bar.raise_()

    def _poll_frame(self):
        """Poll for new frames and update the video widget and overlay."""

        # Keep overlay positioned
        if self.isVisible():
            vid_pos = self.video_container.pos()
            target = QPoint(vid_pos.x() + 4, vid_pos.y() + 4)
            if self.overlay.pos() != target:
                self.overlay.move(target)
            self.overlay.raise_()

        # Get latest frame from capture thread
        arr, w, h, is_new = self.capture.get_frame()
        if is_new and arr is not None:
            self.video_container.update_frame(arr, w, h)
            self.overlay.set_status("")  # Clear "Connecting..."

        # Update overlay stats
        fps, sw, sh = self.capture.get_stats()
        if sw > 0:
            self.overlay.update_stats(fps, sw, sh)

    def _toggle_controls(self):
        """Toggle the control bar on/off."""
        if self.control_bar.isVisible():
            self.control_bar.hide()
        else:
            self._position_floating_widgets()
            self.control_bar.show()

    def keyPressEvent(self, event):
        if event.key() == Qt.Key.Key_F11:
            if self.isFullScreen():
                self.showNormal()
            else:
                self.showFullScreen()
            QTimer.singleShot(100, self._position_floating_widgets)
        elif event.key() == Qt.Key.Key_Escape:
            self._toggle_controls()
        super().keyPressEvent(event)

    def moveEvent(self, event):
        super().moveEvent(event)
        self._position_floating_widgets()

    def resizeEvent(self, event):
        super().resizeEvent(event)
        self._position_floating_widgets()

    def closeEvent(self, event):
        self._render_timer.stop()
        self.capture.stop()
        self.audio.stop()
        self._save_settings()
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
