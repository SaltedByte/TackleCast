import json
import os
from dataclasses import dataclass, asdict

# Based on ShadowCast 3 hardware capabilities.
# mpv handles GPU-accelerated decode, so MJPG@120 is viable now.
# (width, height, fps, pixel_format)
RESOLUTION_PRESETS = {
    "720p @60": (1280, 720, 60, "nv12"),
    "720p @120": (1280, 720, 120, "nv12"),
    "1080p @60": (1920, 1080, 60, "nv12"),
    "1080p @120": (1920, 1080, 120, "mjpeg"),
    "1440p @60": (2560, 1440, 60, "nv12"),
    "1440p @120": (2560, 1440, 120, "mjpeg"),
    "4K @30": (3840, 2160, 30, "nv12"),
    "4K @60": (3840, 2160, 60, "mjpeg"),
}

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
    resolution: str = "1440p @120"
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
