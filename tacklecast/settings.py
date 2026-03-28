import json
import os
from dataclasses import dataclass, asdict

# Resolution options — just width/height, FPS is separate
RESOLUTIONS = {
    "720p": (1280, 720),
    "1080p": (1920, 1080),
    "1440p": (2560, 1440),
    "4K": (3840, 2160),
}

DEFAULT_FPS = 60
MIN_FPS = 30
MAX_FPS = 240


def get_capture_config(resolution, fps):
    """Determine pixel format and thread count from resolution + FPS.

    NV12 (raw) for 60fps and below — no decode overhead.
    MJPEG for higher FPS — required by most capture cards at high frame rates.
    """
    w, h = RESOLUTIONS.get(resolution, (1920, 1080))
    if fps <= 60:
        return w, h, fps, "nv12", 1
    else:
        return w, h, fps, "mjpeg", 4


def _app_dir():
    """Get the application directory — works in dev and PyInstaller bundle."""
    import sys
    if getattr(sys, 'frozen', False):
        return os.path.dirname(sys.executable)
    return os.path.dirname(os.path.dirname(__file__))

SETTINGS_PATH = os.path.join(_app_dir(), "tacklecast_settings.json")


@dataclass
class Settings:
    video_device: str = ""
    audio_input: int = -1
    audio_output: int = -1
    resolution: str = "1080p"
    fps: int = 60
    experimental_fps: bool = False
    volume: float = 1.0

    def save(self):
        with open(SETTINGS_PATH, "w") as f:
            json.dump(asdict(self), f, indent=2)

    @classmethod
    def load(cls) -> "Settings":
        try:
            with open(SETTINGS_PATH, "r") as f:
                data = json.load(f)
            return cls(**{k: v for k, v in data.items() if k in cls.__dataclass_fields__})
        except (FileNotFoundError, json.JSONDecodeError, TypeError):
            return cls()
