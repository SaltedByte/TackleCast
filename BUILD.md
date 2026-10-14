# Building and releasing TackleCast

TackleCast supports Windows 10/11 and Linux desktops. CI uses Rust 1.93.1, Windows Server 2022 runners, and Ubuntu 24.04 runners. CUDA/nvJPEG decoding and DX12 interop are Windows-only; Linux uses software decoding with GPU rendering.

## Windows

Install Rust, Visual Studio C++ Build Tools (MSVC), and LLVM/libclang. No CUDA SDK is needed to compile the application. Run the dependency setup script in the same PowerShell session as Cargo:

```powershell
.\scripts\setup_windows_dependencies.ps1 -IncludeGpuRuntime
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --no-default-features
cargo clippy --locked --all-targets --no-default-features -- -D warnings
.\scripts\package_rust_release.ps1 -PackageName TackleCast-Windows-x86_64 -RequireGpuDlls -Zip
```

The setup downloads an exact FFmpeg 8.1.3 LGPL build and NVIDIA redistributables, verifies their SHA256 hashes, and extracts them into `.deps/`. Pins are in `scripts/windows-dependencies.json`. The FFmpeg monthly archive is retained by its publisher for two years; update that pin deliberately when upgrading dependencies. `LIBCLANG_PATH` defaults to `C:\Program Files\LLVM\bin`.

You can instead set `FFMPEG_DIR` to an existing shared development build with `bin/`, `lib/`, and `include/`. `GPU_RUNTIME_DIR` can point to already-extracted NVIDIA runtime DLLs. Local packaging can fall back to an installed CUDA toolkit; automated releases require the runtime files explicitly. `-SkipGpuDlls` produces a software-only runtime bundle.

GPU decoding requires a compatible NVIDIA GPU and driver. The package includes nvJPEG and cudart, but never the NVIDIA driver DLL `nvcuda.dll`. Machines without compatible hardware automatically use software decoding.

## Linux

On Ubuntu/Debian, install the build tools and audio headers:

```sh
sudo apt-get update
sudo apt-get install -y build-essential nasm libasound2-dev libclang-dev pkg-config
bash scripts/setup_linux_dependencies.sh
export PKG_CONFIG_PATH="$PWD/.deps/ffmpeg/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export LD_LIBRARY_PATH="$PWD/.deps/ffmpeg/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
bash scripts/package_linux_release.sh
```

The setup verifies and builds FFmpeg 8.1.3 from source, enabling V4L2 input, MJPEG/raw video/H.264 decoding, parsing, and scaling. External codec libraries, networking, and command-line programs are disabled. `BUILD_JOBS` controls FFmpeg build concurrency (default 2). Source, build outputs, and shared libraries remain in `.deps/`.

The Linux archive includes all seven FFmpeg shared libraries in `lib/`, with their symlinks, license, source URL, and build configuration. Launch with `./run.sh`; it searches the bundled directory before system libraries. FFmpeg need not be installed on the user's PC. Official archives are built for x86_64 Ubuntu 24.04 or newer. The desktop still supplies glibc, ALSA, and Vulkan/OpenGL graphics drivers. Other distributions can build from source.

CPAL 0.15 uses ALSA on Linux. PipeWire and PulseAudio work through their ALSA compatibility plugins, rather than native CPAL backends. If automatic audio selection chooses a default source, explicitly select your capture card's audio input. Ensure the logged-in user has access to `/dev/video*`; synthetic tests do not require a capture device.

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

## CI and automated releases

PRs and pushes to `main` run formatting, tests, Clippy, and release builds on Windows and Linux, with default and software-only feature configurations. Clippy failures are fatal. Cargo uses the checked-in lockfile; dependency downloads and Rust builds are cached.

Create a version tag matching `Cargo.toml` (for example, `v2.2.0`) to release. The workflow resolves that existing tag to an exact commit, validates that commit through the reusable CI workflow, builds packages, extracts and verifies them, runs synthetic renderer tests, and uploads archives plus SHA256 files. Publication occurs only after both platforms pass, and only in `SaltedByte/TackleCast`.

Manual dispatch requires an existing version tag and builds that tag's commit. Publication is off by default; enable the `publish` input only when you want a GitHub Release. Forks can validate and generate artifacts without publishing upstream releases. Build jobs have read-only repository access; only the publication job receives release-writing permissions.

Both packages include `build-info.json` with their source commit and version, and dependency license/source information. Packaging always invokes Cargo before copying the executable, so an existing binary cannot silently bypass the build.
