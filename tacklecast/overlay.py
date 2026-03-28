from PyQt6.QtWidgets import QWidget
from PyQt6.QtCore import Qt
from PyQt6.QtGui import QPainter, QColor, QFont


class OverlayWidget(QWidget):
    """Transparent child widget that floats above the video surface."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)
        self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
        self.setFixedHeight(40)
        self.setFixedWidth(420)

        self._fps = 0.0
        self._width = 0
        self._height = 0
        self._status = ""
        self._font = QFont("Segoe UI", 10)
        self._font.setBold(True)

    def update_stats(self, fps, width, height, latency_ms=0.0):
        self._fps = fps
        self._width = width
        self._height = height
        self.update()

    def set_status(self, text):
        self._status = text
        self.update()

    def paintEvent(self, event):
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        painter.setFont(self._font)

        if self._status:
            text = f"  {self._status}  "
        elif self._width > 0:
            text = f"  {self._width}x{self._height} | {self._fps:.1f} FPS  "
        else:
            text = "  Connecting...  "

        metrics = painter.fontMetrics()
        text_width = metrics.horizontalAdvance(text)
        pill_w = text_width + 16
        pill_h = 28

        painter.setBrush(QColor(0, 0, 0, 180))
        painter.setPen(Qt.PenStyle.NoPen)
        painter.drawRoundedRect(8, 6, pill_w, pill_h, 8, 8)

        color = QColor(233, 69, 96) if self._status else QColor(224, 224, 224)
        painter.setPen(color)
        y = 6 + (pill_h + metrics.ascent() - metrics.descent()) // 2
        painter.drawText(16, y, text)

        painter.end()
