# TackleCast — Current Progress

Last updated: 2026-10-14

## Current local integration

PR #20 is being integrated with main's Windows/Linux and audio changes. Current behavior is documented in README.md and BUILD.md. Device modes and rational frame rates replace generic menus and resolution/FPS fallback. Auto format shows the actual stream format; color metadata and explicitly labeled defaults replace format-based guesses. Picture adjustments are live, and the four tabs retain the coral/navy branding. FSR is excluded for a separate implementation. Packaging does not require postproc, and Windows environment defaults are session-local.

Picture rows use uniform slider lengths and aligned two-decimal value fields. All four tabs share one menu size, with blank space beneath shorter pages and scrolling on small windows. Native Windows mode queries run in a disposable process with a timeout so a failed vendor driver cannot crash the viewer.

Local validation covers default/software-only tests and Clippy, production shader pixel comparisons on Windows and Linux, and NVIDIA owned/shared decoding on Windows. The attached ShadowCast 3 opened 1080p60 and 1080p59.94 in NV12, plus 4K59.94 in MJPEG with GPU decoding. Nominal rates and measured delivery remain separate. Linux has no attached capture card; physical V4L2 negotiation remains unverified on that machine.

The notes below record earlier development and hardware observations. Their fixed FPS menus, format heuristics, and Windows-only architecture describe those older builds rather than this integration.

## Earlier development notes (April 2026)

## What is TackleCast?

A lightweight, GPU-accelerated capture card viewer for Windows, written in Rust. Designed for low-latency video passthrough from devices like Genki ShadowCast, Elgato, AVerMedia, and other UVC capture cards. Outputs video to a resizable window with an in-app settings menu and FPS overlay.

## Architecture

```
src/
  main.rs          — winit event loop, app state, settings application
  capture.rs       — DirectShow capture via ffmpeg-next, format/resolution fallback
  gpu_decode.rs    — NVIDIA nvJPEG GPU MJPEG decode, zero-copy + owned modes (feature-gated: gpu-decode)
  dx12_interop.rs  — DX12 shared buffer creation for CUDA↔wgpu zero-copy (feature-gated: gpu-decode)
  render.rs        — wgpu DX12 renderer, YUV->RGB WGSL shader, GPU-side buffer→texture copy
  audio.rs         — WASAPI audio passthrough via cpal (input->output with volume)
  ui.rs            — egui overlay pill + settings pause menu
  devices.rs       — DirectShow video + WASAPI audio device enumeration
  settings.rs      — JSON settings load/save (compatible with original Python format)
  logger.rs        — Rotating file logger via tracing
assets/
  icon.ico         — Window icon
```

## Completed Features

### Phase 1: Software Decode (Complete)
- wgpu GPU rendering with YUV->RGB WGSL shader
- egui settings menu (Escape to open) and FPS overlay
- ffmpeg-next DirectShow capture with MJPEG/NV12 decode
- cpal WASAPI audio passthrough with volume control
- Settings compatible with original Python JSON format
- Test pattern mode (`--test`, `--test-nv12`, `--test-yuvj422p`, `--test-alt`)
- Capture buffer tuning: `rtbufsize=16MB`, decode thread at `THREAD_PRIORITY_ABOVE_NORMAL`
- Pixel format fallback: tries nv12 -> mjpeg -> yuyv422 -> uyvy422 -> yuv420p -> auto
- Resolution/FPS fallback: automatically tries lower resolutions and framerates when device rejects requested settings
- Negotiated settings feedback: when fallback occurs, UI updates to show actual capture parameters
- Software pixel format conversion via ffmpeg swscale for unsupported decoder outputs
- F11 fullscreen toggle, window icon, minimize behavior, DPI-aware

### Phase 2A: GPU-Accelerated MJPEG Decode (Complete)
- NVIDIA nvJPEG via CUDA for GPU JPEG decode (tested at 1440p@120fps)
- `src/gpu_decode.rs` — pure `libloading` dynamic loading of nvcuda.dll + nvjpeg64_*.dll
- No CUDA SDK needed at build time; DLLs loaded at runtime
- Automatic fallback to software decode when CUDA/nvJPEG unavailable
- Feature-gated: `gpu-decode` (default on), `--no-default-features` for software-only
- Searches CUDA Toolkit install paths if DLLs not on system PATH
- **DHT injection**: Automatically injects standard JPEG Huffman tables into UVC MJPEG streams that omit them (per UVC spec). Zero-copy fast path when DHT already present. Ensures compatibility with Elgato, AVerMedia, cheap USB dongles, and other capture cards beyond ShadowCast.

### Phase 2A.5 + 2B: Zero-Copy GPU Pipeline (Complete)
Eliminates all CPU-side data movement for the GPU decode path. Three-tier fallback chain:

1. **Zero-copy** (CUDA → shared DX12 buffer → wgpu `copy_buffer_to_texture`):
   - nvJPEG decodes directly into DX12 committed buffers with `D3D12_HEAP_FLAG_SHARED`
   - CUDA imports shared handles via `cuImportExternalMemory` + `cuExternalMemoryGetMappedBuffer`
   - wgpu wraps them via HAL `buffer_from_raw()` and does GPU-side `copy_buffer_to_texture`
   - Frame data **never touches CPU** after decode — zero PCIe bandwidth for decoded frames
   - Double-buffered (2 sets of Y/U/V buffers) for concurrent decode + render
   - wgpu forced to DX12 backend on Windows (required for CUDA interop)

2. **GPU decode + host readback** (fallback when DX12 interop unavailable):
   - Double-buffered host planes with `std::mem::replace` ownership transfer
   - Eliminates the `.to_vec()` copies that were adding ~864 MB/s at 1440p@120fps

3. **Software decode** (fallback when CUDA/nvJPEG unavailable):
   - `queue.write_texture()` with pre-allocated scratch buffers in Renderer
   - No per-frame `Vec` allocations in `pad_rows()` or `deinterleave_nv12()`

- `CaptureFrame` is now an enum: `Cpu { ... }` (with pixel data) or `Gpu { buffer_index }` (zero-copy reference)
- Automatic fallback: zero-copy → host readback → software decode, transparent to user
- Tested at 1440p@120fps on RTX 5080 (desktop) and RTX 3050 (laptop) — see Known Issues for details

### Settings Menu (30/60/120/Custom FPS)
- Frame rate dropdown: 30 FPS, 60 FPS, 120 FPS, Custom (30-240)
- Resolution dropdown: 720p, 1080p, 1440p, 4K
- Scaling filter dropdown: Bilinear, Bicubic, Lanczos
- Color space dropdown: Auto, Rec. 709, BT.601, BT.2020
- Color range dropdown: Auto, Limited (16-235), Full (0-255)
- Video device, audio input/output, volume slider
- Fullscreen toggle, FPS overlay toggle
- Exit button
- Settings applied on menu close (snapshot/apply pattern)

## Known Issues

### Webcam / Non-Capture-Card Devices
Webcams (e.g. Logitech C920) appear in the device list and may partially work, but are **not officially supported**. Known problems:
- Many webcams only support high framerates via H.264, not MJPEG. H.264 over DirectShow/UVC has packet framing issues that cause decode artifacts ("No start code found", corrupted macroblocks).
- The resolution/FPS fallback will find a working mode, but H.264 streams may show visual corruption.
- MJPEG modes on webcams typically max out at 30fps at 1080p. The app will auto-negotiate to a supported mode.
- This is a bonus compatibility feature, not a primary use case. TackleCast is designed for capture cards.

### Laptop Performance at 120fps (RTX 3050 + i5-13420H)
Tested with ShadowCast 3 with zero-copy pipeline:
- **1440p@120fps**: 120fps sustained (with cooling pad). Without cooling, thermal throttling degrades to ~90-110fps after extended 4K sessions. Massive improvement from ~80-100fps before zero-copy.
- **4K@60fps MJPEG**: ~58fps (up from ~48fps before zero-copy). Close to target on a laptop 3050.
- **Thermal throttling**: After sustained heavy load (e.g. 2.5 min of 4K decode), the laptop GPU throttles and 1440p performance degrades. A cooling pad resolves this — the bottleneck is now the GPU's thermal envelope, not software bandwidth.
- **Before zero-copy**: ~80-100fps at 1440p with constant buffer overflow. Root cause was GPU→CPU→GPU data round trip: `cuMemcpyDtoH` + `.to_vec()` + `queue.write_texture()` = ~2.6 GB/s through PCIe 4.0 x4.
- **60fps NV12 works perfectly** on this hardware — zero decode overhead and ~330 MB/s data rate.
- **RTX 3050 laptop has PCIe 4.0 x4** (half the bandwidth of desktop x16), which was the bottleneck before zero-copy.

### Color Range and Color Space Selection (Completed)
Resolved the color range mismatch between NV12 and MJPEG modes and added full color space matrix transformations:
- In WGSL shader: decoupled range decompression (Limited 16-235 vs Full 0-255) from color space matrix transformation (Rec. 709, BT.601, BT.2020).
- In UI settings: interactive dropdowns for Color Space and Color Range.
- Smart Auto defaults: NV12 -> Limited, MJPEG/YUVJ422P -> Full; >= 720p -> Rec. 709, < 720p -> BT.601.
- Immediate real-time application in shader uniforms without capture thread restart.

### ShadowCast 2 Pro: Thermal Throttling at 1440p@120 MJPEG
The ShadowCast 2 Pro's internal MJPEG encoder thermally throttles under sustained 1440p@120fps load. Confirmed via frame arrival timing diagnostics:
- **Cold start**: Delivers ~105-120fps (8.3ms frame intervals) for the first 1-2 minutes
- **Throttled**: Degrades to ~60fps (16.4ms frame intervals) over 2-4 minutes
- **Recovery**: May recover back to 120fps after running throttled for several minutes, then cycle again
- **GPU is not the bottleneck**: RTX 4070 Ti stays at 43-47C and 50-55% utilization throughout. The frames simply stop arriving faster than 60fps from the capture card
- **Cooling helps**: Unplugging the device for 5 minutes and letting it cool extends the initial 120fps window
- **1080p@120 is unaffected**: At 1080p the ShadowCast 2 Pro sends NV12 (raw, no encode), bypassing the MJPEG encoder entirely. Solid 120fps.
- **ShadowCast 3 does not have this issue**: Sustains 1440p@120 MJPEG indefinitely on the same software

This is a hardware limitation of the ShadowCast 2 Pro's encoder chip (small passthrough dongle, no active cooling). Not fixable in software.

### Friend's Testing (ShadowCast 2 Pro + RTX 4070 Ti + i7-12700)
- GPU decode requires NVIDIA driver 570+ for CUDA 13 compatibility
- ShadowCast 2 Pro silently falls back to NV12 at 1080p@120 (only 1440p@120 uses MJPEG)
- Software MJPEG decode at 1440p@120 was ~40fps on i7-12700 (too slow without GPU decode)
- nvJPEG DLLs (nvjpeg64_13.dll + cudart64_13.dll) must be bundled alongside the exe for GPU decode
- See "ShadowCast 2 Pro: Thermal Throttling" above for 1440p@120 performance details

## Build Environment

### Prerequisites
- **Rust toolchain**: `C:\Users\Aluca\.cargo\bin`
- **FFmpeg**: `C:\ffmpeg` (set `FFMPEG_DIR`, add `bin` to PATH)
- **LLVM/libclang**: `C:\Program Files\LLVM\bin` (set `LIBCLANG_PATH`)
- **CUDA Toolkit 13.2**: `C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v13.2` (optional, for GPU decode development)
- Uses `ffmpeg-next = "8"` (not 7 — header compatibility issue with avfft.h)

### Build Commands
```bash
cargo build              # debug build
cargo build --release    # release build
cargo test               # run tests
cargo run -- --test      # test pattern mode (no capture card needed)
```

### Distribution
- Release exe + FFmpeg DLLs + nvJPEG DLLs (for GPU decode)
- Packaging script: `scripts/package_rust_release.ps1`
- GPU decode bundle: `dist/TackleCast-GPU-Decode/`
- Zero-copy bundle: `dist/TackleCast-ZeroCopy/`
- See `BUILD.md` for full packaging details

## Next Steps

### Multi-Vendor GPU Decode Support
Currently GPU decode is NVIDIA-only (nvJPEG/CUDA). Future work to support:
- **AMD**: Investigate AMD AMF (Advanced Media Framework) or Mesa VA-API for MJPEG decode on Radeon GPUs
- **Intel**: Intel Quick Sync / oneVPL for integrated and Arc GPUs
- **Portable fallback**: Investigate Vulkan Video extensions as a cross-vendor decode path

Each vendor path would follow the same pattern as nvJPEG: dynamic library loading at runtime, automatic fallback to software decode when unavailable, no SDK required at build time.

### Other Future Work
- Broader hardware validation across more capture card brands
- UI/menu polish pass based on live feedback
- Audio polish and edge case handling

### Capture settings integration (local validation)
- Integrates the capture controls with the current Windows/Linux and audio code.
- Reads actual DirectShow media headers, checks common rates within interval bounds,
  and confirms the returned rate; Linux uses native V4L2 format/size/interval enumeration.
- Preserves fractional nominal rates separately from measured delivery FPS.
- Auto format selection uses device modes; explicit format requests are strict.
- Tracks source range/matrix metadata independently of plane layout and labels defaults.
- Adds live Picture adjustments and four branded tabs; sliders clear backdrop dimming.
- FSR and its sharpness controls are excluded for a future independent feature PR.
- Windows build defaults are session-local helpers and never leak Windows paths into Linux Cargo builds.
- Packaging requires no postproc library and selects complete GPU runtime pairs from explicit/toolkit sources.
- Calibration and full-screen settings with a separate video preview remain future work.
