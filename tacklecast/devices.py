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


def enumerate_audio_inputs():
    """Returns list of (index, name) for audio input devices."""
    devices = sd.query_devices()
    inputs = []
    for i, dev in enumerate(devices):
        if dev["max_input_channels"] > 0:
            inputs.append((i, dev["name"]))
    return inputs


def enumerate_audio_outputs():
    """Returns list of (index, name) for audio output devices."""
    devices = sd.query_devices()
    outputs = []
    for i, dev in enumerate(devices):
        if dev["max_output_channels"] > 0:
            outputs.append((i, dev["name"]))
    return outputs
