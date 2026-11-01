# Experimental MF GPU settings audit

2026-10-31. This records source-path inspection and automated checks, not a
claim that every menu control has been physically exercised on every device.

| Menu setting | Runtime effect |
| --- | --- |
| Video device | Restarts capture with the selected device; audio Auto routing is reevaluated. MF requires an unambiguous friendly name. |
| Resolution | Restarts capture; MF requires a matching native NV12 size. |
| Frame rate / custom FPS | Restarts capture using the selected rate, including 100 ns interval rounding. No silent rate downgrade. |
| Pixel format | NV12 uses MF when opted in. Auto uses MF only if NV12 is advertised at the requested mode. Explicit MJPEG/YUY2/UYVY/I420 use the existing capture path. |
| Scaling filter | Updates the existing production shader live and on apply; shared by CPU and MF GPU frames. |
| Color space | Auto resolves native MF YUV-matrix metadata. Explicit selection overrides metadata in the shader. Unknown metadata retains labeled defaults. |
| Color range | Auto resolves native MF full/limited-range metadata. Explicit selection overrides metadata in the shader. Unsupported ranges are not mislabeled as detected. |
| Brightness | Updates shader uniform live and on apply. |
| Contrast | Updates shader uniform live and on apply. |
| Saturation | Updates shader uniform live and on apply. |
| Gamma | Updates shader uniform live and on apply. |
| Reset picture | Restores neutral values for the four shader adjustments. |
| Audio input | Auto follows only a unique name match for the selected video device; no unrelated capture-card or system-input fallback. Explicit selection remains independent. Missing explicit inputs stay disconnected. |
| Audio output | Restarts independent audio routing; unchanged by GPU capture. |
| Volume | Updates the audio pipeline on settings apply without restarting video. |
| Mute | Updates the audio pipeline on settings apply without restarting video. |
| Overlay enabled | Controls overlay drawing after settings apply. |
| Detailed overlay | Adds scaling/color information after settings apply; uses renderer source metadata. |
| Fullscreen button / F11 | Changes the window's fullscreen state independently of capture. |
| Exit | Stops capture and audio, saves settings, and exits the event loop. |
| Escape / click outside | Saves draft settings and applies restart/live handlers. |

Fixed: the original MF override ignored explicit pixel-format selection and
always attempted NV12. Backend selection now honors explicit selections and
keeps Auto usable for modes requiring MJPEG (for example 4K120).

Added: native matrix/range extraction from the negotiated Media Foundation
media type, actual negotiated rate logging, metadata interpretation tests,
manual-override checks, and an added GPU reference case for contrast. The
settings restart test enumerates every serialized Settings field and fails if
a new field is added without audit coverage.

Validation: 48 default unit tests pass; strict Clippy and software-only checks
pass; the production shader reference test passes on RX 9070 XT/DX12 across
NV12 and planar 4:2:2 with all scaling filters. This is correctness coverage,
not a measured latency improvement. Native bridge compiles with
`-Wall -Wextra -Werror`.

Live checks on the 4K X / RX 9070 XT with `tacklecast-mf-gpu-color.exe`:

- Native 1440p120 and 1440p60 both report matrix=1, range=2 (Rec.709 limited).
  Auto labels correctly show detected values, without the default qualifier.
- Explicit MJPEG at 1440p120 switches to `MJPEG (SW)` on this PC. Returning
  to Auto at 1440p60 opens MF GPU and delivers approximately 60 FPS.
- Selecting 1080p60 reconnects to MF GPU with a correctly resized buffer pool.
- Brightness, contrast, saturation, and gamma each visibly change the live
  GPU image. Saturation zero produces grayscale; Reset restores all four
  neutral values and color output.
- Manual BT.601 and Full range selections update the live image and persist
  on apply. Lanczos selection also persists and renders correctly. All three
  scaling filters have separate production-shader GPU reference coverage.
- Mute applies and persists; unmute plus 45% volume applies and persists.
  Video continues without a capture restart. Audio status remains Connected.
  Audible output was not independently measured or listened to.
- Compact overlay removes color/scaling details; disabling the overlay hides
  it. The fullscreen menu button enters fullscreen and F11 returns to windowed.
- Settings Exit closes the process. The original settings file was restored
  byte-for-byte and the metadata build relaunched at 1440p120, Auto color,
  bilinear scaling, neutral picture, default audio, and the detailed overlay.

Device switching, alternate audio input/output devices, custom FPS, and every
advertised 4K X mode were checked through code/routing tests, not individually
exercised on hardware in this audit. A controlled source black/white/color
pattern comparison is still needed for end-to-end color calibration; game
screenshots and shader references do not establish source/display accuracy.
Existing native
capture constraints (non-array, exact-size NV12 surfaces), HDR tone mapping,
driver loss recovery, and native chroma-siting interpretation remain limitations.

For the older development build, use `scripts/run_mf_gpu_viewer.ps1 -ColorMetadata`;
add `-Build` to compile both the matching bridge and viewer while it is closed.
This launcher sets the FFmpeg/MinGW DLL search paths. Launching the bare EXE
without those paths can show missing `avdevice-62.dll` / `avcodec-62.dll`
dialogs; the audit relaunch encountered this and was corrected. The final
viewer was verified at 120 delivered FPS with the original settings restored.

As of 2.2.2, packaged TackleCast.exe locates its native bridge in its own directory
and automatically attempts eligible NV12 modes. Missing bridges use DirectShow;
MF GPU errors trigger one DirectShow retry for that configuration. Changing a
video setting permits a fresh GPU attempt. `TACKLECAST_MF_GPU=0` disables native
GPU capture for diagnostics; the absolute DLL override remains for developers.
The package contains no batch launcher, duplicate icon file, or probe documents.
