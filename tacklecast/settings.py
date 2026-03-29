import json
import os
import sys
from dataclasses import dataclass, asdict

# Resolution options — just width/height, FPS is separate
RESOLUTIONS = {
    "720p": (1280, 720),
    "1080p": (1920, 1080),
    "1440p": (2560, 1440),
    "4K": (3840, 2160),
}

# FPS mode options
FPS_MODE_60 = "60"
FPS_MODE_120 = "120"
FPS_MODE_CUSTOM = "custom"

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


def _data_dir():
    """Get the data directory for settings/logs — _internal/ for frozen, project root for dev."""
    if getattr(sys, 'frozen', False):
        return os.path.join(os.path.dirname(sys.executable), "_internal")
    return os.path.dirname(os.path.dirname(__file__))


SETTINGS_PATH = os.path.join(_data_dir(), "tacklecast_settings.json")


@dataclass
class Settings:
    video_device: str = ""
    audio_input: int = -1
    audio_output: int = -1
    resolution: str = "1080p"
    fps_mode: str = FPS_MODE_60
    custom_fps: int = 120
    volume: float = 1.0
    show_overlay: bool = True

    def save(self):
        os.makedirs(os.path.dirname(SETTINGS_PATH), exist_ok=True)
        with open(SETTINGS_PATH, "w") as f:
            json.dump(asdict(self), f, indent=2)

    @classmethod
    def load(cls) -> "Settings":
        try:
            with open(SETTINGS_PATH, "r") as f:
                data = json.load(f)
            # Migrate old settings format
            if "experimental_fps" in data:
                if data.get("experimental_fps"):
                    data["fps_mode"] = FPS_MODE_CUSTOM
                    data["custom_fps"] = data.get("fps", 120)
                data.pop("experimental_fps", None)
                data.pop("fps", None)
            return cls(**{k: v for k, v in data.items() if k in cls.__dataclass_fields__})
        except (FileNotFoundError, json.JSONDecodeError, TypeError):
            return cls()

    def get_fps(self):
        """Get the effective FPS value based on the current mode."""
        if self.fps_mode == FPS_MODE_60:
            return 60
        elif self.fps_mode == FPS_MODE_120:
            return 120
        else:
            return self.custom_fps
