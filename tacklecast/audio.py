import sounddevice as sd
import numpy as np


def find_audio_input_for_video(video_device_name):
    """Try to find an audio input device that matches the video capture card.

    Returns the device index, or None if no match found.
    """
    if not video_device_name:
        return None
    # Normalize for matching — capture cards typically share keywords
    video_lower = video_device_name.lower()
    keywords = []
    for word in video_lower.replace("-", " ").split():
        if len(word) >= 3 and word not in ("pro", "the", "and"):
            keywords.append(word)

    devices = sd.query_devices()
    best_match = None
    best_score = 0
    for i, dev in enumerate(devices):
        if dev["max_input_channels"] <= 0:
            continue
        name_lower = dev["name"].lower()
        score = sum(1 for kw in keywords if kw in name_lower)
        if score > best_score:
            best_score = score
            best_match = i

    if best_score >= 2:
        return best_match
    return None


class AudioPassthrough:
    def __init__(self):
        self._stream = None
        self._volume = 1.0

    def _callback(self, indata, outdata, frames, time_info, status):
        outdata[:] = indata * self._volume

    def start(self, input_device=None, output_device=None, volume=1.0):
        from tacklecast.logger import get_logger
        log = get_logger()
        self.stop()
        self._volume = volume

        try:
            # Query input device for its actual capabilities
            if input_device is not None:
                dev_info = sd.query_devices(input_device)
            else:
                dev_info = sd.query_devices(kind="input")
            log.info(f"Audio input device: {dev_info['name']} (idx={input_device}, "
                     f"channels={dev_info['max_input_channels']}, "
                     f"samplerate={dev_info['default_samplerate']})")

            samplerate = int(dev_info["default_samplerate"])
            in_channels = dev_info["max_input_channels"]
            if in_channels <= 0:
                log.error("Audio input device has 0 input channels")
                return
            in_channels = min(in_channels, 2)

            # Query output device for its capabilities
            if output_device is not None:
                out_info = sd.query_devices(output_device)
            else:
                out_info = sd.query_devices(kind="output")
            out_channels = min(out_info["max_output_channels"], 2)
            log.info(f"Audio output device: {out_info['name']} (idx={output_device}, "
                     f"channels={out_info['max_output_channels']})")

            # Use the minimum channel count that both devices support
            channels = min(in_channels, out_channels)
            if channels <= 0:
                log.error("No compatible channel count between input and output devices")
                return

            log.info(f"Audio stream: {channels}ch @ {samplerate}Hz")

            self._stream = sd.Stream(
                device=(input_device, output_device),
                samplerate=samplerate,
                blocksize=256,
                channels=channels,
                dtype=np.float32,
                latency="low",
                callback=self._callback,
            )
            self._stream.start()
        except Exception as e:
            log.error(f"Audio error: {e}")
            self._stream = None

    def stop(self):
        if self._stream is not None:
            try:
                self._stream.stop()
                self._stream.close()
            except Exception:
                pass
            self._stream = None

    def set_volume(self, volume):
        self._volume = max(0.0, min(1.0, volume))

    @property
    def is_running(self):
        return self._stream is not None and self._stream.active
