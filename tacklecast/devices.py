import re
import subprocess

import imageio_ffmpeg
import sounddevice as sd

_ffmpeg = imageio_ffmpeg.get_ffmpeg_exe()


def enumerate_video_devices():
    """Get DirectShow video device names via ffmpeg. Returns list of (name, name)."""
    devices = []
    try:
        result = subprocess.run(
            [_ffmpeg, "-f", "dshow", "-list_devices", "true", "-i", "dummy"],
            capture_output=True, text=True, timeout=10,
            creationflags=0x08000000,
        )
        # Parse ffmpeg output: lines like '[dshow @ ...] "Device Name" (video)'
        for line in result.stderr.split('\n'):
            match = re.search(r'"(.+?)"\s+\(video\)', line)
            if match:
                name = match.group(1)
                devices.append((name, name))
    except Exception:
        pass
    return devices


def _wasapi_index():
    """Find the WASAPI host API index, preferred on modern Windows."""
    try:
        for i, api in enumerate(sd.query_hostapis()):
            if "wasapi" in api["name"].lower():
                return i
    except Exception:
        pass
    return None


def enumerate_audio_inputs():
    """Returns list of (index, name) for WASAPI audio input devices."""
    wasapi = _wasapi_index()
    devices = sd.query_devices()
    inputs = []
    for i, dev in enumerate(devices):
        if dev["max_input_channels"] > 0:
            if wasapi is not None and dev["hostapi"] != wasapi:
                continue
            inputs.append((i, dev["name"]))
    return inputs


def enumerate_audio_outputs():
    """Returns list of (index, name) for WASAPI audio output devices."""
    wasapi = _wasapi_index()
    devices = sd.query_devices()
    outputs = []
    for i, dev in enumerate(devices):
        if dev["max_output_channels"] > 0:
            if wasapi is not None and dev["hostapi"] != wasapi:
                continue
            outputs.append((i, dev["name"]))
    return outputs
