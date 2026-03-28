import sounddevice as sd
import numpy as np


class AudioPassthrough:
    def __init__(self):
        self._stream = None
        self._volume = 1.0

    def _callback(self, indata, outdata, frames, time_info, status):
        outdata[:] = indata * self._volume

    def start(self, input_device=None, output_device=None, volume=1.0):
        self.stop()
        self._volume = volume

        try:
            # Get sample rate from input device
            if input_device is not None:
                dev_info = sd.query_devices(input_device)
                samplerate = int(dev_info["default_samplerate"])
                channels = min(dev_info["max_input_channels"], 2)
            else:
                samplerate = 48000
                channels = 2

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
            from tacklecast.logger import get_logger
            get_logger().error(f"Audio error: {e}")
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
