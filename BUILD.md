# Building and releasing TackleCast

TackleCast supports Windows 10/11 and Linux desktops. CI uses Rust 1.93.1, Windows Server 2022 runners, and Ubuntu 24.04 runners. CUDA/nvJPEG decoding and DX12 interop are Windows-only; Linux uses software decoding with GPU rendering.

## Windows

Install Rust, Visual Studio C++ Build Tools (MSVC), and LLVM/libclang. No CUDA SDK is needed to compile the application. Run the dependency setup script in the same PowerShell session as Cargo:

```powershell
.\scripts\setup_windows_dependencies.ps1 -IncludeGpuRuntime
. .\scripts\windows_build_environment.ps1
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --no-default-features
cargo clippy --locked --all-targets --no-default-features -- -D warnings
```

The setup downloads an exact FFmpeg 8.1.3 LGPL shared build and NVIDIA redistributables, verifies their SHA256 hashes, and extracts them into `.deps/`. Pins are in `scripts/windows-dependencies.json`. The FFmpeg monthly archive is retained by its publisher for two years; update that pin deliberately when upgrading dependencies. `LIBCLANG_PATH` defaults to `C:\Program Files\LLVM\bin`. Development builds link the shared FFmpeg, so `cargo run` finds its DLLs on the PATH that the setup script sets.

You can instead set `FFMPEG_DIR` to an existing FFmpeg development build with `lib/` and `include/` (and `bin/` for a shared build). The Windows environment helper preserves explicit settings, then searches `.deps/ffmpeg` and `C:\ffmpeg`; changes apply only to the current PowerShell session. `scripts/build_windows.ps1 -Command test` also loads this helper. These defaults do not inject Windows paths into Linux builds.

The Media Foundation GPU bridge (`src/mf_gpu_native.cpp`) is compiled into every Windows build by `build.rs`, using the same C++ compiler as Rust. Set `TACKLECAST_MF_GPU=0` to turn off native GPU capture when troubleshooting.

GPU decoding requires a compatible NVIDIA GPU and driver. Releases carry nvJPEG and cudart, but never the NVIDIA driver DLL `nvcuda.dll`. Machines without compatible hardware automatically use software decoding.

### Standalone release executable

The Windows release is a single `TackleCast.exe` with nothing beside it. The README's [What's inside the Windows EXE](README.md#whats-inside-the-windows-exe) describes it for users. To build it:

```powershell
.\scripts\setup_windows_dependencies.ps1 -IncludeGpuRuntime -StaticFfmpeg
.\scripts\package_rust_release.ps1
```

That produces `dist\TackleCast-Windows-x86_64.exe` and its `.sha256`. It works like this:

- **Static FFmpeg.** `-StaticFfmpeg` downloads the pinned FFmpeg 8.1.3 source (`ffmpeg_source` in `windows-dependencies.json`) and builds it with [`scripts/build_static_ffmpeg.sh`](scripts/build_static_ffmpeg.sh) into `.deps\ffmpeg-static-msvc`. It is trimmed to DirectShow capture; the MJPEG, raw video, H.264 and HEVC decoders; and swscale, with no network code or external libraries, under the LGPL. The build needs MSYS2 at `C:\msys64`, and it installs `make`, `diffutils` and `nasm` with pacman. For MSVC it enters the Visual Studio developer environment. It is rebuilt only when the source pin or the build script changes. It then sets `FFMPEG_DIR` to the static build.
- **Cargo features.** `package_rust_release.ps1` builds with `--features standalone`:
  - `static-ffmpeg` links FFmpeg statically. `build.rs` adds the Windows libraries FFmpeg needs, from its pkg-config files.
  - `embed-gpu-runtime` stores the nvJPEG and CUDA runtime from `GPU_RUNTIME_DIR` inside the EXE. On first use they are unpacked to `%LOCALAPPDATA%\TackleCast\runtime\<version>-<content hash>\`, written to a temporary name and renamed into place, then loaded from there.
- **Static C runtime.** `.cargo/config.toml` sets `+crt-static` for Windows targets, so the EXE needs no Visual C++ Redistributable. The MSVC FFmpeg build uses `-MT` to match.
- **License notices.** `build.rs` assembles them from the FFmpeg and NVIDIA licenses actually linked or embedded. The app shows them under Settings > Display > About > Licenses.
- **Verification.** The packaging script runs `scripts/verify_package.py --exe`. It reads the EXE's import and delay-import tables and fails if any DLL isn't part of Windows. Release CI also runs the synthetic smoke test on the EXE copied alone into an empty folder.
- **Separate build folder.** The standalone build uses `target\standalone`, so switching between it and shared-FFmpeg development builds doesn't rebuild everything.

Pass `-NoGpuRuntime` to leave out the NVIDIA runtime; the EXE is about 5 MB smaller and MJPEG always decodes on the CPU. To relink against a different FFmpeg, point `FFMPEG_DIR` at your own static build and run the packaging script; `lib\pkgconfig` must list its link dependencies.

### Windows GNU fallback

If MSVC installation is unavailable, an installed MSYS2 MinGW compiler at
`C:\msys64\mingw64` can build and test the application with the official GNU
Rust toolchain. LLVM/libclang and the pinned FFmpeg package are still required.
This is an alternative local validation path; release CI continues to use MSVC.

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnu --profile minimal --component rustfmt --component clippy
. .\scripts\windows_gnu_build_environment.ps1
cargo +stable-x86_64-pc-windows-gnu test --locked
cargo +stable-x86_64-pc-windows-gnu test --locked gpu_shader_color_and_picture_reference -- --ignored --nocapture
```

The standalone executable builds the same way with `-Gnu` on both scripts. The C++ runtime (`libstdc++`, `libgcc`, `winpthread`) is linked statically, so the EXE needs no MinGW DLLs:

```powershell
.\scripts\setup_windows_dependencies.ps1 -IncludeGpuRuntime -StaticFfmpeg -Gnu
.\scripts\package_rust_release.ps1 -Gnu
```

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

CPAL 0.15 uses ALSA on Linux. PipeWire and PulseAudio work through their ALSA compatibility plugins, rather than native CPAL backends. Audio input Auto follows a unique name match for the selected video device. If no unique match exists, audio stays disconnected until an input is explicitly selected. Auto never substitutes another capture card or the system microphone. Ensure the logged-in user has access to `/dev/video*`; synthetic tests do not require a capture device.

## Audio conversion

Input and output streams use their native configurations. Mono is duplicated to stereo, stereo can be averaged to mono, and a streaming cubic resampler handles different sample rates with two input frames of lookahead. Tests verify that splitting audio into arbitrary callback sizes leaves the output unchanged. Output samples are clamped before conversion to the device's sample type.

Surround-to-stereo conversion retains center dialogue, LFE, and surround speakers with normalized gains. It assumes conventional interleaved order: FL, FR, FC, LFE, BL, BR, SL, SR. Quad omits FC/LFE, 5.0 omits LFE, and 6.1 uses BC/SL/SR after LFE. CPAL does not report speaker masks; configure a source with a different speaker order for stereo PCM or route it to this order.

## Validation without a capture card

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

PRs and pushes to `main` run formatting, tests, Clippy, and release builds on Windows and Linux, with default and software-only feature configurations. Default Windows builds also compile the native MF GPU bridge with MSVC, and release packages must include it. Clippy failures are fatal. Cargo uses the checked-in lockfile; dependency downloads and Rust builds are cached.

Create a version tag matching `Cargo.toml` (for example, `v2.2.0`) to release. The workflow resolves that existing tag to an exact commit, validates that commit through the reusable CI workflow, builds packages, extracts and verifies them, runs synthetic renderer tests, and uploads archives plus SHA256 files. Publication occurs only after both platforms pass, and only in `btawa/TackleCast`.

Manual dispatch requires an existing version tag and builds that tag's commit. Publication is off by default; enable the `publish` input only when you want a GitHub Release. Forks can validate and generate artifacts without publishing upstream releases. Build jobs have read-only repository access; only the publication job receives release-writing permissions.

Both packages include `build-info.json` with their source commit and version, and dependency license/source information. Packaging always invokes Cargo before copying the executable, so an existing binary cannot silently bypass the build.
