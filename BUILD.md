# Building and releasing TackleCast

TackleCast supports Windows 10/11 and Linux desktops. CI uses Rust 1.93.1, Windows Server 2022 runners, and Ubuntu 24.04 runners. CUDA/nvJPEG decoding and DX12 interop are Windows-only; Linux uses software decoding with GPU rendering.

## Windows

The UI uses egui/egui-winit/egui-wgpu **0.35** and egui-components **0.3.0**.
The component crate's published manifest requires egui 0.35; its crate-level
description mentioning 0.34 is stale. The matching egui-wgpu backend requires
wgpu **29**, with Windows bindings **0.62** for DX12 interop. Keep the checked-in
lockfile: gpu-allocator's Windows binding must match wgpu-hal's binding rather
than CPAL's older, separate Windows dependency. The existing CI Rust 1.93.1 and
local Rust 1.94.1 meet the UI stack's Rust 1.92 requirement.

The component theme follows Vice/Classic. Settings use egui-components tabs,
selects, switches, sliders, checkboxes and buttons, with official egui_extras
strip cells enforcing shared columns. Numeric fields use egui's built-in
DragValue; the component crate's 0.3.0 NumberInput paints its opaque background
after its child text editor. No local fork or widget geometry is required.

Install Rust, Visual Studio C++ Build Tools (MSVC), and LLVM/libclang. No CUDA SDK is needed to compile the application. Run the dependency setup script in the same PowerShell session as Cargo:

```powershell
.\scripts\setup_windows_dependencies.ps1 -IncludeGpuRuntime
. .\scripts\windows_build_environment.ps1
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --no-default-features
cargo clippy --locked --all-targets --no-default-features -- -D warnings
.\scripts\package_rust_release.ps1 -PackageName TackleCast-Windows-x86_64 -RequireGpuDlls -Zip
```

The setup downloads an exact FFmpeg 8.1.3 LGPL build and NVIDIA redistributables, verifies their SHA256 hashes, and extracts them into `.deps/`. Pins are in `scripts/windows-dependencies.json`. The FFmpeg monthly archive is retained by its publisher for two years; update that pin deliberately when upgrading dependencies. `LIBCLANG_PATH` defaults to `C:\Program Files\LLVM\bin`.

You can instead set `FFMPEG_DIR` to an existing shared development build with `bin/`, `lib/`, and `include/`. The Windows environment helper preserves explicit settings, then searches `.deps/ffmpeg` and `C:\ffmpeg`; changes apply only to the current PowerShell session. `scripts/build_windows.ps1 -Command test` also loads this helper. These defaults do not inject Windows paths into Linux builds.

`GPU_RUNTIME_DIR` can point to already-extracted NVIDIA runtime DLLs. Packaging's `-CudaDllDir` takes priority, followed by `GPU_RUNTIME_DIR` and installed CUDA toolkits. Both nvJPEG and cudart must come from one directory with compatible runtime versions (11, 12, or 13). Packaging does not search old distribution folders. Automated releases use pinned runtime files. `-SkipGpuDlls` produces a software-only runtime bundle. FFmpeg's unused `postproc` library is not required.

GPU decoding requires a compatible NVIDIA GPU and driver. The package includes nvJPEG and cudart, but never the NVIDIA driver DLL `nvcuda.dll`. Machines without compatible hardware automatically use software decoding.

## Windows audio and DXGI compatibility

Automatic capture selection chooses the first unambiguous video/audio-input pair
by meaningful friendly-name tokens. For example, `HD (ShadowCast 3)` matches
`ShadowCast 3`; generic words such as USB, HD, Audio and Device do not establish a
match. Identical product names on multiple endpoints require explicit selection.
Name pairing cannot identify every webcam versus capture card.

An explicit video choice guides automatic audio; explicit saved inputs are
preserved, including unavailable selections. Automatic values stay automatic
when saved. No match leaves an actionable selection state, without starting an
unrelated camera/microphone or silently substituting a test pattern. Playback
output selection remains independent and can use the system default. DirectShow audio
remains an explicit input choice for devices such as the AVerMedia GC550 from
issue #16. Selecting DirectShow explicitly does not silently switch to a microphone
if the device has no usable audio pin; inspect the audio status and choose another
input if needed.

The experimental ReShade/DXGI runtime-patching workaround is disabled. Startup
only checks for a possible local `dxgi.dll` next to the executable and logs a
warning; it does not load, modify, remove, or block that file. Presence alone does
not prove incompatibility. A proxy can still cause startup to fail: disable
ReShade or move its proxy DLL out of the application directory and retry. Native
DX12 rendering, FSR and RTX VSR are retained; their usual hardware requirements
still apply.

Hardware regression checklist (unit tests do not replace these checks):

- Automatic friendly-name pairing with a webcam listed before the capture card,
  a headset input present, and unmatched/ambiguous names. Verify no unrelated
  camera or microphone opens when there is no pair.
- Explicit WASAPI and DirectShow selection, including GC550 capture audio.
- Explicit DirectShow with a missing audio pin: video continues and audio status
  reports the missing input, without silently opening the default microphone.
- Native DX12 startup and FSR/RTX VSR on supported hardware without a proxy.
- Local proxy diagnostic: a warning is logged, startup is not deliberately
  blocked, and the proxy file is unchanged. Repeat without the proxy if startup
  fails; injected-proxy compatibility is not guaranteed.

## Linux

On Ubuntu/Debian, install the build tools and audio headers:

```sh
sudo apt-get update
sudo apt-get install -y build-essential nasm libasound2-dev libclang-dev pkg-config libxkbcommon-x11-0
bash scripts/setup_linux_dependencies.sh
export PKG_CONFIG_PATH="$PWD/.deps/ffmpeg/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export LD_LIBRARY_PATH="$PWD/.deps/ffmpeg/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
bash scripts/package_linux_release.sh
```

The setup verifies and builds FFmpeg 8.1.3 from source, enabling V4L2 input, MJPEG/raw video/H.264 decoding, parsing, and scaling. External codec libraries, networking, and command-line programs are disabled. `BUILD_JOBS` controls FFmpeg build concurrency (default 2). Source, build outputs, and shared libraries remain in `.deps/`.

The Linux archive includes all seven FFmpeg shared libraries in `lib/`, with their symlinks, license, source URL, and build configuration. Launch with `./run.sh`; it searches the bundled directory before system libraries. FFmpeg need not be installed on the user's PC. Official archives are built for x86_64 Ubuntu 24.04 or newer. The desktop still supplies glibc, ALSA, its X11/Wayland libraries, and Vulkan/OpenGL graphics drivers. X11 sessions require the `libxkbcommon-x11` runtime (`libxkbcommon-x11-0` on Ubuntu/Debian). Other distributions can build from source.

CPAL 0.15 uses ALSA on Linux. PipeWire and PulseAudio work through their ALSA compatibility plugins, rather than native CPAL backends. If a unique name match is unavailable, explicitly select your capture card's video and audio input. Ensure the logged-in user has access to `/dev/video*`; synthetic tests do not require a capture device.

## Audio conversion

Input and output streams use their native configurations. Mono is duplicated to stereo, stereo can be averaged to mono, and a streaming cubic resampler handles different sample rates with two input frames of lookahead. Tests verify that splitting audio into arbitrary callback sizes leaves the output unchanged. Output samples are clamped before conversion to the device's sample type.

Surround-to-stereo conversion retains center dialogue, LFE, and surround speakers with normalized gains. It assumes conventional interleaved order: FL, FR, FC, LFE, BL, BR, SL, SR. Quad omits FC/LFE, 5.0 omits LFE, and 6.1 uses BC/SL/SR after LFE. CPAL does not report speaker masks; configure a source with a different speaker order for stereo PCM or route it to this order.

## Capture and audio recovery regression checks

Unit tests cover permanent capture read errors/EOF, cancellation during empty
reads, video-stall deadlines, nonblocking worker retirement, callback monitoring
including legitimate silence, and bounded recovery budgets. On physical hardware:

- Unplug USB during capture: the UI must remain responsive, display a capture
  error, and stop showing stale delivered FPS. Reconnect during automatic retries
  or use Reinitialize devices after retries are exhausted.
- Change resolution/FPS while capture is stalled and while a restart is pending.
  Only the newest applied settings should open; workers and shared GPU buffers
  must not overlap. Close the app while the device is unavailable.
- Repeat with NV12/software decode, MJPEG GPU decode, and explicit DirectShow
  audio where available. An operating-system driver stuck in teardown can delay
  reconnection; the app must remain usable while waiting.
- Interrupt WASAPI input or output: verify recovery uses the same endpoint IDs
  even if enumeration order or system defaults change. Recovery must not open
  another microphone or output when an endpoint is missing.
- Test PS5/ShadowCast 3 across console sleep/wake and HDMI mode changes. Use
  Reinitialize devices if necessary, and retain the log around onset/recovery.
  Silence alone must not trigger restarts. A hardware reboot may still be needed.
- Verify three failed automatic retries stop; a manual retry restores the budget.
  Check audio-only recovery leaves WASAPI video capture running and that explicit
  DirectShow audio retires/restarts its shared capture worker first.

Reinitialize devices is available in every settings tab. It saves the current
draft, stops capture and audio, waits for capture retirement, clears cached
capabilities, and enumerates devices/modes again on a worker before reopening
the selected routes. Settings stay open; the window and renderer remain alive.
Automatic choices are paired again; explicit audio choices are remapped by a
unique remembered name. Missing/ambiguous choices remain unavailable until
selected, rather than opening a different microphone or default output. Older
settings without remembered names remain readable. Synthetic previews never
start real device discovery.

On Windows, a read-only USB hub query can identify a selected USB camera/card's
actual link by its PnP instance. A confirmed USB 2 (or slower) link shows a
warning that connection speed may restrict capture modes, with cable/port and
reinitialization guidance. Unknown links produce no guessed warning. The V2
connection flags are required because the older USB speed field alone also
reports High-Speed for SuperSpeed devices. No driver/device configuration is
changed. Linux currently performs rediscovery without USB speed diagnostics.

These checks do not establish the cause of issue #22's original freeze or of an
intermittent PS5 audio loss; physical reporter retesting is still required.

## Settings UI validation

Layout tests compare actual widget bounds across all four tabs, both themes,
320x360 through 1920x1080 windows, and 100–200% display scaling. They check label
and control columns, vertical centering, slider/value edges, clipping and menu
size. Input regressions cover numeric editing/clamping, switches, selectors,
picture reset, and overlay checkbox/reorder controls. The native zero-frame test
uses the real egui-winit bridge and draws only when repaint requests schedule it;
it supplies no capture frames.

For optional static visual review through egui-wgpu, set an output directory and
run the normally ignored graphics test (it opens no window or devices):

```powershell
$env:TACKLECAST_UI_PREVIEWS = 'C:\Temp\TackleCast-UI-Review'
cargo test --locked settings_pages_render_with_readable_controls -- --ignored
```

This exports all four pages and the expanded statistics editor in both themes,
and verifies that visible numeric fields contain rendered text. It is separate
from redraw scheduling and real capture/device interaction validation.

## Validation without a capture card

The Windows test `settings_input_schedules_redraw_with_zero_capture_frames`
uses a hidden native window and the production egui/winit event bridge. It sends
no capture frames and draws only on a scheduled repaint. It checks unconsumed
pointer movement, tab navigation, device selection, applying a selection, both
Escape transitions, and idle settling. This complements the synthetic renderer
tests below; continuously arriving frames cannot establish no-frame interaction.

Manual Windows check: while capture is failing or disconnected, open settings
with Escape, navigate tabs, choose another device and use Apply & close. Reopen
and close with Escape, and verify the menu updates immediately without frames.
Retain the runtime log for actual device opening and any launch failure.

```sh
cargo fmt --all --check
cargo run -- --test-nv12
cargo run -- --test-mjpeg
```

Package checks should use the extracted archive, rather than the build directory:

```sh
python3 scripts/verify_package.py --package /path/to/TackleCast-Linux-x86_64
python3 scripts/smoke_test.py --binary /path/to/TackleCast-Linux-x86_64/tacklecast
```

On Windows, use `python`, `--require-gpu`, and the extracted folder containing `TackleCast.exe`. The verifier loads the Windows DLLs from the package and checks Linux FFmpeg resolution with no development-library paths. The smoke test uses a different working directory and requires a positive rendered/uploaded FPS summary after 35 seconds. A desktop session is required; Linux CI uses Xvfb and Mesa software rendering.

On an NVIDIA Windows PC, the ignored hardware tests exercise both owned nvJPEG decoding and DX12/CUDA shared-buffer decoding:

```powershell
$env:PATH = "$PWD\.deps\gpu-runtime;$env:PATH"
cargo test --locked hardware_nvjpeg -- --ignored --test-threads=1
```

Synthetic tests validate the renderer and conversion code. They do not prove real capture-card negotiation, PS4 HDMI behavior, or sustained capture performance.

Use `--list-capture-modes` to print connected devices and their reported resolution/format/rate combinations. Windows probes standard intermediate rates and both interval limits through DirectShow `SetFormat`/`GetFormat`; Linux uses V4L2 discrete modes or the driver's declared step/continuous constraints. Rates remain rational (for example `60000/1001`), independently of measured FPS. No calibration wizard is included. Mode discovery cannot prove that a card or HDMI source will sustain its advertised throughput; verify actual frame delivery on hardware.

The ignored `gpu_shader_color_and_picture_reference` test renders production shader output and checks readback pixels for color/range and picture adjustments. It requires a working graphics adapter:

```sh
cargo test --locked gpu_shader_color_and_picture_reference -- --ignored
```

## CI and automated releases

PRs and pushes to `main` run formatting, tests, Clippy, and release builds on Windows and Linux, with default and software-only feature configurations. Clippy failures are fatal. Cargo uses the checked-in lockfile; dependency downloads and Rust builds are cached.

Create a version tag matching `Cargo.toml` (for example, `v2.2.0`) to release. The workflow resolves that existing tag to an exact commit, validates that commit through the reusable CI workflow, builds packages, extracts and verifies them, runs synthetic renderer tests, and uploads archives plus SHA256 files. Publication occurs only after both platforms pass.

Manual dispatch requires an existing version tag and builds that tag's commit. Publication is off by default; enable the `publish` input only when you want a GitHub Release. Forks can validate and generate artifacts without publishing upstream releases. Build jobs have read-only repository access; only the publication job receives release-writing permissions.

Both packages include `build-info.json` with their source commit and version, and dependency license/source information. Packaging always invokes Cargo before copying the executable, so an existing binary cannot silently bypass the build.

## Frame generation

The frame generation code lives in `src/framegen/`, `src/render_framegen.rs` and `src/render_framegen_fix.rs`, and builds with the normal steps above (default features). Nothing extra is needed at compile time:

- RIFE: `onnxruntime.dll`, `onnxruntime_providers_shared.dll`, `DirectML.dll` and the models are loaded at runtime from a `rife` folder next to the exe. Take that folder from the release zip, or rebuild the models with `scripts/rife/export_models.py`.
- NVIDIA FRUC: `nvcuda.dll` comes with the driver, `NvOFFRUC.dll` / `cudart64_110.dll` come from your own Optical Flow SDK 5.0.7 and can't be redistributed.

`cargo test -- --ignored` also runs the GPU tests (needs a working wgpu adapter).

## VI plus frame generation: complete portable Windows candidate

The integrated candidate keeps VI's egui0.35/wgpu29/Windows0.62 stack. Frame
Gen uses the same themed, aligned component controls as the other pages.
Generated/unique/presented FPS and FG state are optional rows in the existing
reorderable overlay. Existing VI layouts retain their order/enabled choices;
new rows start disabled. Legacy PR28 diagnostic-overlay preferences migrate
once into those rows. The separate diagnostic overlay is no longer drawn.

The portable bundle includes RIFE4.26/4.6 half-resolution-motion models at720p,
1080p and1440p, ONNX Runtime DirectML1.24.4 and DirectML1.15.4. Runtime/model/license
bytes are pinned in `scripts/framegen-assets.json`. Prepare a verified asset
folder with the manifest's paths, including its `licenses/` entries, and pass
it as `-FrameGenDir` (or place it in `.deps/rife`). Model export is a build-time
operation using `scripts/rife/export_models.py`; users do not run Python or
download models. Preserve model dimensions/stems and verify ONNX graph/weight
completeness before changing the asset manifest. Do not commit binary weights.

```powershell
. .\scripts\windows_build_environment.ps1
.\scripts\package_rust_release.ps1 -PackageName TackleCast-VI-FrameGen-Test -RequireGpuDlls -RequireFrameGen -FrameGenDir C:\BuildAssets\rife -Zip
python scripts/verify_package.py --package dist/TackleCast-VI-FrameGen-Test --require-gpu
```

Packaging checks every pinned asset before copying it and excludes other models.
It also copies Microsoft's licensed release x64 CRT DLLs app-locally from the
installed Visual Studio Redist directory (`-VcRuntimeDir` can select it), checks
Microsoft signatures, and records versions/hashes in `build-info.json`. App-local
CRT servicing belongs to each package update; test extraction on clean Windows
as well as checking a developer PC with development DLL directories removed.
No driver, Python, CUDA toolkit, NVIDIA Optical Flow SDK or TensorRT install is
required for the bundled RIFE path. FRUC remains a developer-only CLI choice;
manual SDK paths are not exposed in normal settings. Full-resolution-motion and
4K model files are not included. Unsupported FG sizes leave normal video usable.

Frame generation remains off by default. The existing half-period/adaptive
scheduling and GPU composition path from PR28 are retained; no experimental
DirectML graph capture or TensorRT changes from separate research are enabled.
Internal scheduling delay is not total HDMI-to-screen latency. Validate model
changes and reinitialization/resolution changes with actual device testing;
unit and synthetic GPU tests do not establish physical latency or visual quality.

## v3.VI overlay presets

Package version3.0.0 is branded v3.VI. Overlay mode is Hidden/Compact/Custom;
only Custom has the position/statistics editor. New settings default to Compact,
while older saved masters migrate to Hidden/Custom and retain their layout.
The Compact arrow reports unique-source to application-presented FPS; it must
not use delivered capture FPS as its source when capture contains duplicates.
Rates are formatted compactly; capture/requested fractions remain exact in the
settings/capture pipeline. Recovery status remains independent of Hidden mode.
The zero-capture-frame native test exercises all three selector choices and
preserves the custom layout. Offscreen previews include Compact FG on/off in
both themes, in addition to the settings pages and expanded Custom editor.

## Automated bundled-model preparation

Windows default-feature PR CI and the tagged Windows release build both run the
same complete-payload preparation and require FG files during package verification.
`python scripts/setup_framegen.py` downloads hash-pinned ONNX Runtime/DirectML
NuGet packages, RIFE4.26/4.6 weights and the upstream license, then exports exactly
the six supported models with the pinned CPU-only Python3.12 build requirements.
Install those requirements in a build-only venv with
`python -m pip install -r scripts/rife/requirements-build.txt`. There is no model
export or Python requirement on the end user's PC. Every generated model/runtime/
notice must match `scripts/framegen-assets.json` before installation into `.deps/rife`.
The script verifies an existing cache before reusing it. `--verify-only` validates
an existing output without downloads/export. `--output`/`--cache` support isolated
build checks. Binary weights/models are not committed to Git.

Release publication is still triggered only by an explicit version tag or manual
release dispatch with publication enabled. Merging the feature PR into main runs
CI, including package validation, without creating a tag or GitHub Release.

## Manual GitHub Releases updater

Open Settings and select **Check for updates** to query the latest stable GitHub
Release. Select **Update to vX.Y.Z** to download and stage an eligible update,
then **Restart to update** to install it. Startup and idle sessions make no
updater network requests. Pass `--no-updates` on launch to hide the control.
Recovery from a previously interrupted installation still runs when needed.

In-place installation requires a writable x86_64 Windows or Linux portable
package, a pinned Ed25519 public key, and the matching release archive,
`.sha256`, and `.sha256.sig` assets. Source builds, read-only installs, and
releases without a signature offer the GitHub release page instead. Asset URLs
must match the release repository and tag. All redirects must use HTTPS on
GitHub's release hosts. Archives are limited to 2 GiB downloaded and 4 GiB
extracted, with at most 10,000 entries.

The installer stages into a unique directory under `.update-staging/`. Archive
paths and install parents are validated before replacement. Linux archives can
contain same-directory symlinks that resolve to files in the package. The
installer protects `tacklecast_settings.json`, `logs/`, `recordings/`, and
`NvOFFRUC.dll`. Files absent from the new archive are kept.

Close other instances of TackleCast in the same install folder before selecting
**Restart to update**. The helper waits for the old process to exit, locks the
install, and saves the originals and a recovery journal before replacing any
files. Startup restores the originals if a transaction was interrupted. If the
main executable cannot start, run the staged executable with `--recover-update`
from its `payload` folder. On Linux, run
`LD_LIBRARY_PATH="$PWD/lib" ./tacklecast --recover-update` there. If recovery
fails, the staging folder retains the journal and backup for another attempt.
Completed transactions are cleaned up on startup.

### Signing release updates

Keep the Ed25519 private key offline. When building portable packages, set
`TACKLECAST_UPDATE_PUBLIC_KEY` to the raw 32-byte public key as 64 hex
characters. Release builds without that setting offer manual downloads.
The built-in repository is `SaltedByte/TackleCast`. For a fork, also set
`TACKLECAST_UPDATE_REPO=owner/repo` at compile time.

The signing script verifies the SHA-256 of each archive and signs a message
binding the checksum to its repository, tag, and filename. On the offline
signing machine, place the two official archives and their `.sha256` files
in `release-assets/`. Each checksum file uses the standard
`<sha256>  <filename>` format. Run `python3 scripts/sign_update.py` with
`TACKLECAST_UPDATE_PUBLIC_KEY`, `TACKLECAST_UPDATE_SIGNING_KEY` (the PEM private
key), `TACKLECAST_SIGN_REPO`, and `TACKLECAST_SIGN_TAG` set. The script requires
Python 3.11 or later and OpenSSL 3. Attach the archives, checksums, and generated
`.sha256.sig` files to the draft GitHub release before publishing it.
