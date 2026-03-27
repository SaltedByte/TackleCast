import os
import sys
import time

# Ensure mpv DLL is findable — works in both dev mode and PyInstaller bundle
def _find_mpv():
    candidates = [
        # PyInstaller bundle: DLL is next to the exe
        getattr(sys, '_MEIPASS', ''),
        os.path.dirname(sys.executable),
        # Dev mode: mpv_bin/ next to the tacklecast package
        os.path.join(os.path.dirname(os.path.dirname(__file__)), "mpv_bin"),
    ]
    for d in candidates:
        if d and os.path.isfile(os.path.join(d, "libmpv-2.dll")):
            os.environ["PATH"] = d + ";" + os.environ.get("PATH", "")
            return
_find_mpv()

import mpv


class MpvCapture:
    """Wraps mpv for DirectShow capture card playback embedded in a Qt widget."""

    def __init__(self):
        self._player = None
        self._wid = None
        self._on_error = None
        self._last_frame_num = 0
        self._last_poll_time = 0.0
        self._measured_fps = 0.0

    def start(self, wid, device_name, width, height, fps, pixel_format="mjpeg",
              on_fps_update=None, on_error=None):
        """Start playback embedded in the given window handle."""
        self.stop()
        self._wid = wid
        self._on_error = on_error
        self._last_frame_num = 0
        self._last_poll_time = 0.0
        self._measured_fps = 0.0

        try:
            self._player = mpv.MPV(
                wid=str(int(wid)),
                profile="low-latency",
                untimed=True,
                aid="no",
                demuxer_lavf_format="dshow",
                demuxer_lavf_o=(
                    f"video_size={width}x{height},"
                    f"framerate={fps},"
                    f"rtbufsize=150M"
                    + (f",vcodec=mjpeg" if pixel_format == "mjpeg" else f",pixel_format={pixel_format}")
                ),
                vo="gpu",
                hwdec="auto-safe",
                video_latency_hacks="yes",
                cache="no",
                demuxer_max_bytes="500KiB",
                demuxer_max_back_bytes="0",
                vd_lavc_threads=4,
                osd_level=0,
                keep_open="yes",
                msg_level="all=error",
            )

            @self._player.event_callback("end-file")
            def _on_end(event):
                try:
                    reason = getattr(event, 'reason', None)
                    if self._on_error and reason and str(reason) == "error":
                        self._on_error("Device disconnected or capture error")
                except Exception:
                    pass

            url = f'av://dshow:video={device_name}'
            self._player.play(url)

        except Exception as e:
            if self._on_error:
                self._on_error(f"Failed to start mpv: {e}")

    def stop(self):
        if self._player:
            try:
                self._player.terminate()
            except Exception:
                pass
            self._player = None

    def poll_stats(self):
        """Poll mpv for current resolution and measured FPS.

        Returns (fps, width, height) or None.
        """
        if not self._player:
            return None
        try:
            w = self._player.width
            h = self._player.height
            if not w or not h:
                return None

            now = time.monotonic()
            try:
                frame_num = self._player.estimated_frame_number
            except Exception:
                frame_num = None

            if frame_num is not None and self._last_poll_time > 0:
                dt = now - self._last_poll_time
                if dt >= 0.3:
                    df = frame_num - self._last_frame_num
                    self._measured_fps = df / dt if dt > 0 else 0.0
                    self._last_frame_num = frame_num
                    self._last_poll_time = now
            elif frame_num is not None:
                self._last_frame_num = frame_num
                self._last_poll_time = now

            return (self._measured_fps, w, h)
        except Exception:
            return None

    @property
    def is_running(self):
        return self._player is not None
