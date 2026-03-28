import threading
import time

import av
import numpy as np

from tacklecast.logger import get_logger

# Chroma subsampling dimensions for common YUV formats
# Returns (uv_w_divisor, uv_h_divisor, full_range)
_YUV_FORMATS = {
    "yuv420p":  (2, 2, False),
    "yuvj420p": (2, 2, True),
    "yuv422p":  (2, 1, False),
    "yuvj422p": (2, 1, True),
    "yuv444p":  (1, 1, False),
    "yuvj444p": (1, 1, True),
}


class CaptureThread:
    """Captures video from a DirectShow device using PyAV and delivers
    decoded YUV frames via a shared reference for GL rendering."""

    def __init__(self):
        self._thread = None
        self._stop_event = threading.Event()
        self._frame_lock = threading.Lock()
        self._latest_frame = None  # (y, u, v, uv_w, uv_h, full_range)
        self._frame_w = 0
        self._frame_h = 0
        self._new_frame = False
        self._fps = 0.0
        self._width = 0
        self._height = 0
        self._frame_count = 0
        self._fps_time = 0.0
        self._lock = threading.Lock()
        self._on_error = None

    def get_frame(self):
        """Get the latest frame if available.
        Returns (frame_data, w, h, is_new) or (None, 0, 0, False).
        frame_data is (y_bytes, u_bytes, v_bytes, uv_w, uv_h, full_range)."""
        with self._frame_lock:
            if self._latest_frame is not None:
                new = self._new_frame
                self._new_frame = False
                return self._latest_frame, self._frame_w, self._frame_h, new
            return None, 0, 0, False

    def get_stats(self):
        """Returns (fps, width, height)."""
        with self._lock:
            return self._fps, self._width, self._height

    def start(self, device_name, width, height, fps, pixel_format="nv12",
              decode_threads=1, on_error=None):
        """Start capturing from the given DirectShow device."""
        self.stop()
        self._stop_event.clear()
        self._on_error = on_error

        with self._frame_lock:
            self._latest_frame = None
            self._new_frame = False

        self._thread = threading.Thread(
            target=self._capture_loop,
            args=(device_name, width, height, fps, pixel_format),
            daemon=True,
        )
        self._thread.start()

    def stop(self):
        """Stop the capture thread."""
        self._stop_event.set()
        if self._thread and self._thread.is_alive():
            self._thread.join(timeout=3.0)
        self._thread = None

    @property
    def is_running(self):
        return self._thread is not None and self._thread.is_alive()

    def _capture_loop(self, device_name, width, height, fps, pixel_format):
        log = get_logger()
        log.info("--- Capture start ---")
        log.info(f"Requested: device={device_name}, {width}x{height}@{fps}fps, format={pixel_format}")

        options = {
            "video_size": f"{width}x{height}",
            "framerate": str(fps),
            "rtbufsize": "1M",
        }
        if pixel_format == "mjpeg":
            options["vcodec"] = "mjpeg"
        else:
            options["pixel_format"] = pixel_format

        log.info(f"PyAV options: {options}")

        container = None
        try:
            container = av.open(
                f"video={device_name}",
                format="dshow",
                options=options,
            )
            stream = container.streams.video[0]

            # Enable multi-threaded MJPEG decode
            if pixel_format == "mjpeg":
                stream.codec_context.thread_count = 4
                stream.codec_context.thread_type = "FRAME"
                log.info(f"MJPEG multi-threaded decode: threads={stream.codec_context.thread_count}")

            log.info(f"Stream opened: codec={stream.codec_context.name}, "
                     f"{stream.width}x{stream.height}")

            diag_logged = False
            self._fps_time = time.monotonic()
            self._frame_count = 0
            last_periodic = time.monotonic()
            error_count = 0

            for packet in container.demux(stream):
                if self._stop_event.is_set():
                    break

                # Decode packet — skip bad packets gracefully
                try:
                    frames = stream.codec_context.decode(packet)
                except (av.error.InvalidDataError, ValueError):
                    error_count += 1
                    if error_count <= 5:
                        log.warning(f"Skipping bad packet (count={error_count})")
                    continue

                for frame in frames:
                    if self._stop_event.is_set():
                        break

                    w = frame.width
                    h = frame.height
                    fmt = frame.format.name

                    # For NV12, reformat to planar yuv420p
                    if fmt == "nv12":
                        frame = frame.reformat(format="yuv420p")
                        fmt = "yuv420p"

                    # Look up chroma layout
                    fmt_info = _YUV_FORMATS.get(fmt)
                    if fmt_info is None:
                        # Unknown format — fall back to yuv420p conversion
                        frame = frame.reformat(format="yuv420p")
                        fmt_info = (2, 2, False)

                    uv_w_div, uv_h_div, full_range = fmt_info
                    uv_w = w // uv_w_div
                    uv_h = h // uv_h_div

                    # Extract raw plane bytes — no swscale conversion
                    y_stride = frame.planes[0].line_size
                    uv_stride = frame.planes[1].line_size

                    if y_stride == w:
                        y_data = bytes(frame.planes[0])
                    else:
                        y_data = np.frombuffer(frame.planes[0], dtype=np.uint8).reshape(h, y_stride)[:, :w].tobytes()

                    if uv_stride == uv_w:
                        u_data = bytes(frame.planes[1])
                        v_data = bytes(frame.planes[2])
                    else:
                        u_data = np.frombuffer(frame.planes[1], dtype=np.uint8).reshape(uv_h, uv_stride)[:, :uv_w].tobytes()
                        v_data = np.frombuffer(frame.planes[2], dtype=np.uint8).reshape(uv_h, uv_stride)[:, :uv_w].tobytes()

                    # Store latest frame for main thread
                    with self._frame_lock:
                        self._latest_frame = (y_data, u_data, v_data, uv_w, uv_h, full_range)
                        self._frame_w = w
                        self._frame_h = h
                        self._new_frame = True

                    # Update FPS measurement
                    self._frame_count += 1
                    now = time.monotonic()
                    dt = now - self._fps_time
                    if dt >= 1.0:
                        with self._lock:
                            self._fps = self._frame_count / dt
                            self._width = w
                            self._height = h
                        self._frame_count = 0
                        self._fps_time = now

                    # Log diagnostics on first frame
                    if not diag_logged:
                        diag_logged = True
                        log.info(f"=== First frame received ===")
                        log.info(f"  codec: {stream.codec_context.name}")
                        log.info(f"  pixel format: {fmt} ({'full' if full_range else 'limited'} range)")
                        log.info(f"  resolution: {w}x{h}")
                        log.info(f"  chroma: {uv_w}x{uv_h} ({'4:2:2' if uv_h_div == 1 else '4:2:0'})")
                        log.info(f"  Y: {len(y_data)} bytes (stride={y_stride}, {'no pad' if y_stride == w else 'PADDED'})")
                        log.info(f"  U: {len(u_data)} bytes (stride={uv_stride}, {'no pad' if uv_stride == uv_w else 'PADDED'})")
                        log.info(f"=== end first frame ===")

                    # Periodic stats
                    if now - last_periodic >= 15.0:
                        last_periodic = now
                        with self._lock:
                            fps_val = self._fps
                        log.info(f"[periodic] fps={fps_val:.1f}, resolution={w}x{h}")

        except av.error.FileNotFoundError:
            msg = f"Device not found: {device_name}"
            log.error(msg)
            if self._on_error:
                self._on_error(msg)
        except av.error.OSError as e:
            msg = f"Capture error: {e}"
            log.error(msg)
            if self._on_error:
                self._on_error(msg)
        except Exception as e:
            msg = f"Unexpected capture error: {e}"
            log.error(msg)
            if self._on_error:
                self._on_error(msg)
        finally:
            if container:
                try:
                    container.close()
                except Exception:
                    pass
            log.info("Capture thread stopped")
