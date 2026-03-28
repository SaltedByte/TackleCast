import os
import sys
import time

from tacklecast.logger import get_logger

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

# mpv properties we want to capture for diagnostics
_DIAG_PROPERTIES = [
    "hwdec-current",
    "video-codec",
    "video-params",
    "video-out-params",
    "vo",
    "estimated-vf-fps",
    "frame-drop-count",
    "decoder-frame-drop-count",
    "demuxer-cache-duration",
    "width",
    "height",
    "container-fps",
    "avsync",
]

# Lighter set for periodic logging (every 15s)
_PERIODIC_PROPERTIES = [
    "estimated-vf-fps",
    "frame-drop-count",
    "decoder-frame-drop-count",
    "demuxer-cache-duration",
    "avsync",
]


class MpvCapture:
    """Wraps mpv for DirectShow capture card playback embedded in a Qt widget."""

    def __init__(self):
        self._player = None
        self._wid = None
        self._on_error = None
        self._last_frame_num = 0
        self._last_poll_time = 0.0
        self._measured_fps = 0.0
        self._diag_logged = False
        self._last_periodic_log = 0.0

    def start(self, wid, device_name, width, height, fps, pixel_format="mjpeg",
              decode_threads=1, on_fps_update=None, on_error=None):
        """Start playback embedded in the given window handle."""
        log = get_logger()
        log.info("--- Capture start ---")
        log.info(f"Requested: device={device_name}, {width}x{height}@{fps}fps, format={pixel_format}, threads={decode_threads}")

        self.stop()
        self._wid = wid
        self._on_error = on_error
        self._last_frame_num = 0
        self._last_poll_time = 0.0
        self._measured_fps = 0.0
        self._diag_logged = False
        self._last_periodic_log = 0.0

        # Scale buffers based on codec — MJPEG frames are much larger than raw NV12
        # and need room in the demuxer, while the DirectShow buffer should be small
        # to prevent upstream latency buildup.
        if pixel_format == "mjpeg":
            # ~2-3 frames of headroom in demuxer, tight DirectShow buffer
            rtbufsize = "1M"
            demuxer_max = "2MiB"
        else:
            # Raw NV12 — minimal buffers, no decoding needed
            rtbufsize = "1M"
            demuxer_max = "100KiB"

        log.info(f"Buffer config: rtbufsize={rtbufsize}, demuxer_max_bytes={demuxer_max}")
        log.info(f"Render config: vo=gpu-next, video_sync=desync, untimed=true")

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
                    f"rtbufsize={rtbufsize}"
                    + (f",vcodec=mjpeg" if pixel_format == "mjpeg" else f",pixel_format={pixel_format}")
                ),
                vo="gpu-next",
                hwdec="auto-safe",
                video_latency_hacks="yes",
                cache="no",
                demuxer_max_bytes=demuxer_max,
                demuxer_max_back_bytes="0",
                demuxer_thread="no",
                vd_lavc_threads=decode_threads,
                video_sync="desync",
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
            log.error(f"Failed to start mpv: {e}")
            if self._on_error:
                self._on_error(f"Failed to start mpv: {e}")

    def stop(self):
        if self._player:
            try:
                self._player.terminate()
            except Exception:
                pass
            self._player = None

    def log_diagnostics(self):
        """Log all mpv diagnostic properties to the log file."""
        if not self._player:
            return
        log = get_logger()
        log.info("=== mpv diagnostic snapshot ===")
        for prop in _DIAG_PROPERTIES:
            try:
                val = self._player._get_property(prop)
                log.info(f"  {prop} = {val}")
            except Exception:
                log.info(f"  {prop} = <unavailable>")

        # Add human-readable hwdec explanation
        try:
            hwdec = self._player._get_property("hwdec-current")
            codec = self._player._get_property("video-codec")
            if hwdec == "no" or not hwdec:
                if codec and "jpeg" in str(codec).lower():
                    log.info("  NOTE: MJPEG is not supported by GPU hardware decoders (NVDEC/etc). "
                             "CPU decode is expected. GPU is still used for rendering via vo=gpu.")
                else:
                    log.info("  NOTE: Raw video (NV12) requires no decoding. "
                             "GPU is used for rendering via vo=gpu.")
        except Exception:
            pass
        log.info("=== end diagnostics ===")

    def _log_periodic_stats(self):
        """Log lightweight stats periodically to catch latency buildup."""
        if not self._player:
            return
        log = get_logger()
        stats = {}
        for prop in _PERIODIC_PROPERTIES:
            try:
                stats[prop] = self._player._get_property(prop)
            except Exception:
                stats[prop] = "<unavailable>"
        parts = ", ".join(f"{k}={v}" for k, v in stats.items())
        log.info(f"[periodic] {parts}")

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

            # Log diagnostics once on the first frame we get back
            if not self._diag_logged:
                self._diag_logged = True
                self.log_diagnostics()
                self._last_periodic_log = time.monotonic()

            now = time.monotonic()

            # Periodic stats every 15 seconds
            if now - self._last_periodic_log >= 15.0:
                self._last_periodic_log = now
                self._log_periodic_stats()

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
