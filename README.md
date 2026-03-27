# TackleCast

<p align="center">
  <img src="assets/icon.png" alt="TackleCast" width="128">
</p>

**A lightweight, low-latency capture card viewer for Windows.** No recording, no bloat — just your game on your screen.

Built for capture cards like the Genki ShadowCast, Elgato, AVerMedia, and other UVC-compliant devices. TackleCast uses [mpv](https://mpv.io/) under the hood for GPU-accelerated rendering, giving you crisp video at up to 1440p@120fps with minimal latency.

## Features

- **GPU-accelerated video** via mpv — hardware MJPG decode, DirectX rendering
- **Low-latency audio passthrough** to your speakers or headphones
- **Resolution presets** — 720p, 1080p, 1440p, 4K at various framerates
- **Live FPS counter** with real measured framerate
- **Auto-detect capture cards** via DirectShow
- **Dark theme UI** with auto-hiding controls
- **Fullscreen support** (F11)
- **Zero recording overhead** — purely a viewer
- **Settings persistence** — remembers your device selections

## Quick Start (Download)

1. Download the latest `TackleCast-v1.0-win64.zip` from [Releases](../../releases)
2. Extract anywhere
3. Double-click `TackleCast.exe`

No Python or other software required.

## Quick Start (From Source)

**Requirements:** Python 3.12+, [7-Zip](https://7-zip.org)

```
git clone https://github.com/SaltedByte/TackleCast.git
cd TackleCast
setup.bat
run.bat
```

`setup.bat` creates a virtual environment, installs dependencies, downloads mpv, and builds the launcher exe.

## Controls

| Action | Key/Mouse |
|---|---|
| Show controls | Move mouse to bottom 25% of window |
| Pin/unpin controls | Tab |
| Fullscreen | F11 |
| Exit fullscreen | Escape |

## Resolution Presets

| Preset | Format | Notes |
|---|---|---|
| 720p @60 | NV12 | Uncompressed |
| 720p @120 | NV12 | Uncompressed |
| 1080p @60 | NV12 | Uncompressed |
| 1080p @120 | MJPG | GPU decoded |
| 1440p @60 | NV12 | Uncompressed |
| 1440p @120 | MJPG | GPU decoded |
| 4K @30 | NV12 | Uncompressed |
| 4K @60 | MJPG | GPU decoded |

Available presets depend on your capture card's capabilities. The presets above are based on the Genki ShadowCast 3.

## Architecture

TackleCast is intentionally minimal:

- **mpv** — handles DirectShow capture, MJPG decode (hardware-accelerated), and GPU rendering directly into the app window
- **PyQt6** — dark-themed UI with floating overlay and control bar
- **sounddevice** — low-latency audio passthrough from capture card to speakers
- **imageio-ffmpeg** — device enumeration via bundled ffmpeg

## Building a Standalone Distribution

To create a portable zip for distribution:

```
python build_dist.py
```

Output: `dist/TackleCast/` — zip this folder. Users just extract and run `TackleCast.exe`.

## License

MIT
