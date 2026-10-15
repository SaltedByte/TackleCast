# TackleCast

<p align="center">
  <img src="assets/icon.png" alt="TackleCast" width="128">
</p>

**A lightweight, GPU-accelerated capture card viewer for Windows and Linux.** No recording, no bloat, just your game on your screen.

Built for capture cards like the Genki ShadowCast, Elgato, AVerMedia, and other UVC-compliant devices. Written in Rust with a zero-copy GPU pipeline and low-latency audio passthrough.

## Features

- **GPU-accelerated rendering** via wgpu with a custom YUV-to-RGB shader
- **NVIDIA GPU MJPEG decode** via nvJPEG/CUDA (automatic fallback to software decode)
- **Zero-copy GPU pipeline** - decoded frames never leave the GPU (CUDA to DX12 to wgpu)
- **Low-latency audio passthrough** via WASAPI (Windows) and ALSA, including PipeWire/PulseAudio compatibility plugins (Linux)
- **Robust audio resampling & channel conversion** - handles mono, stereo, and variable sample rates (e.g. PS4/PS5/Switch audio over USB capture cards)
- **Audio mute toggle & UI status** - mute control with real-time audio connection status
- **Cross-platform video capture** - DirectShow on Windows, V4L2 on Linux
- **Device-supported modes** - resolutions, pixel formats, and frame rates reported by the selected card, including fractional rates
- **Visible format negotiation** - Auto prefers NV12, then MJPEG at the selected resolution and FPS, and shows the format actually opened
- **Color controls** - metadata-aware color space and range, with explicitly labeled defaults and manual overrides
- **Live picture controls** - brightness, contrast, saturation, gamma, and reset
- **Live FPS counter** with separate nominal and delivered frame rates
- **Auto-detect capture cards** via DirectShow and V4L2
- **Four-tab settings** - Video, Picture, Audio, and Display, retaining the coral/navy theme
- **Fullscreen support** (F11 or toggle in settings)
- **Zero recording overhead** - purely a viewer
- **Settings persistence** - remembers your device selections
- **Diagnostic logging** - rotating log files for troubleshooting

## Quick Start

1. Download the latest release from [Releases](../../releases)
   - **Windows**: Download `TackleCast-Windows-x86_64.zip`, extract anywhere, and double-click `TackleCast.exe`.
   - **Linux**: Download `TackleCast-Linux-x86_64.tar.gz`, extract (`tar -xzvf TackleCast-Linux-x86_64.tar.gz`), and run `./run.sh`.

FFmpeg is bundled on both platforms. Windows packages also include the CUDA/nvJPEG runtime; GPU decoding needs a compatible NVIDIA GPU and driver. Official Linux archives target x86_64 Ubuntu 24.04 or newer and use your desktop's ALSA and Vulkan/OpenGL drivers.

### PS4 audio and video

Connect the PS4's HDMI output to your capture card and the card to your PC over USB. For gameplay capture, clear **Settings > System > Enable HDCP** ([PlayStation guide](https://manuals.playstation.net/document/en/ps4/settings/hdcp.html)). Set **Settings > Sound and Screen > Audio Output Settings > Audio Format (Priority)** to **Linear PCM** ([PlayStation guide](https://manuals.playstation.net/document/en/ps4/settings/audiooutput.html)).

In TackleCast, press Escape and select the capture card's audio input and your speakers/headphones as output. Check that audio is Connected and unmuted. Mono input is duplicated to stereo, and different input/output sample rates are converted. For the widest capture-card compatibility, use stereo PCM; surround conversion assumes the conventional speaker order described in [BUILD.md](BUILD.md).

If sound is missing, check the selected input is the capture card rather than your microphone or desktop's default source, and inspect the input/output formats in `logs/`. Unit tests cover channel conversion and resampling; console-specific behavior still depends on the card and needs hardware validation.

## Building from Source

See [BUILD.md](BUILD.md) for full build instructions.

```bash
cargo build --release
```

## Controls

| Action | Key |
|---|---|
| Open/close settings | Escape |
| Fullscreen | F11 |

## How It Works

TackleCast has a three-tier decode pipeline that automatically selects the best path for your hardware:

| Tier | Path | When |
|---|---|---|
| Zero-copy | nvJPEG decode to shared DX12 buffer to wgpu | NVIDIA GPU with CUDA support (Windows) |
| GPU decode + readback | nvJPEG decode to host memory to wgpu | CUDA available, DX12 interop unavailable |
| Software decode | ffmpeg CPU decode to wgpu | Linux or no CUDA/nvJPEG available |

Auto selects a format from the card's supported modes at the requested resolution and frame rate. It tries NV12 first, then MJPEG, then other supported formats. Manual format selections are strict; capture does not silently reduce resolution or frame rate. The menu shows the actual format after negotiation. The overlay distinguishes the nominal mode from measured frame delivery.

Color Auto follows stream/frame metadata. When metadata is missing, the UI labels the fallback as a default rather than presenting it as detected information. BT.2020 selects a conversion matrix; it does not implement HDR tone mapping.

## Architecture

```
src/
  main.rs          - winit event loop, app state, settings
  capture.rs       - DirectShow / V4L2 capture via ffmpeg-next, strict mode negotiation
  capabilities/    - Native DirectShow / V4L2 mode discovery
  frame_rate.rs    - Rational nominal frame rates
  color.rs         - Color metadata and explicit defaults
  gpu_decode.rs    - NVIDIA nvJPEG GPU MJPEG decode (feature-gated: gpu-decode)
  dx12_interop.rs  - DX12 shared buffers for CUDA/wgpu zero-copy (feature-gated: gpu-decode)
  render.rs        - wgpu DX12/Vulkan renderer, YUV->RGB WGSL shader
  audio/           - Audio passthrough via cpal (WASAPI on Windows, PipeWire/ALSA on Linux)
  devices.rs       - Cross-platform video + audio device enumeration
  settings.rs      - JSON settings load/save
  ui.rs            - egui overlay and settings menu
  logger.rs        - Rotating file logger via tracing
```

## Known Limitations

- **GPU decode is NVIDIA-only** (AMD AMF and Intel Quick Sync planned for a future release). Non-NVIDIA GPUs fall back to software decode automatically.
- **Some capture cards may throttle at high framerates.** Smaller USB passthrough dongles (e.g. Genki ShadowCast 2 Pro) can thermally throttle their internal MJPEG encoder at sustained 1440p@120fps, causing frame delivery to drop to ~60fps. This is a hardware limitation, not a software issue. Devices with better thermal design (e.g. ShadowCast 3) are unaffected.
- Laptop GPUs may thermal throttle at sustained 4K@60fps or 1440p@120fps (a cooling pad helps).
- Webcams may partially work but are not officially supported.

## Acknowledgments

- **[@NeverForgetful](https://www.youtube.com/@NeverForgetful)** - Testing and QA

## License

MIT
