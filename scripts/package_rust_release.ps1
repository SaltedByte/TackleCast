param(
    [string]$PackageName = "TackleCast-Rust",
    [switch]$Zip,
    [switch]$SkipGpuDlls,
    [switch]$RequireGpuDlls,
    [string]$CudaDllDir,
    [switch]$MfGpu,
    [switch]$Gnu
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if ($PackageName -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$') {
    throw 'PackageName must be a directory name, without path separators.'
}
if ($SkipGpuDlls -and $RequireGpuDlls) { throw 'GPU runtime cannot be both skipped and required.' }

function Resolve-Root {
    Split-Path -Path $PSScriptRoot -Parent
}

function Ensure-Exists {
    param([string]$Path, [string]$Message)
    if (-not (Test-Path -LiteralPath $Path)) {
        throw $Message
    }
}

$root = Resolve-Root
. (Join-Path $PSScriptRoot 'windows_build_environment.ps1')
if ($Gnu) { . (Join-Path $PSScriptRoot 'windows_gnu_build_environment.ps1') }
$distRoot = Join-Path $root "dist"
$outDir = Join-Path $distRoot $PackageName
$targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
if (-not [IO.Path]::IsPathRooted($targetRoot)) { $targetRoot = Join-Path $root $targetRoot }
$releaseExe = Join-Path $targetRoot "release\tacklecast.exe"
$settingsSrc = Join-Path $root "tacklecast_settings.json"
$iconSrc = Join-Path $root "assets\icon.ico"
$ffmpegDir = $env:FFMPEG_DIR
if ([string]::IsNullOrWhiteSpace($ffmpegDir) -and (Test-Path "C:\ffmpeg")) {
    $ffmpegDir = "C:\ffmpeg"
}

if ([string]::IsNullOrWhiteSpace($ffmpegDir)) {
    throw "FFMPEG_DIR is not set. Set it to your FFmpeg root (contains bin/, lib/, include/)."
}

$ffmpegBin = Join-Path $ffmpegDir "bin"
Ensure-Exists -Path $ffmpegBin -Message "FFMPEG_DIR\bin not found: $ffmpegBin"

Push-Location $root
try {
    Write-Host "Building current release binary..."
    if ($Gnu) { cargo +stable-x86_64-pc-windows-gnu build --release --locked }
    else { cargo build --release --locked }
    if ($LASTEXITCODE -ne 0) { throw "Release build failed ($LASTEXITCODE)" }

    Ensure-Exists -Path $releaseExe -Message "Release executable missing: $releaseExe"
    Ensure-Exists -Path $iconSrc -Message "Icon missing: $iconSrc"

    if (Test-Path -LiteralPath $outDir) {
        $absoluteOut = [IO.Path]::GetFullPath($outDir)
        $absoluteDist = [IO.Path]::GetFullPath($distRoot) + [IO.Path]::DirectorySeparatorChar
        if (-not $absoluteOut.StartsWith($absoluteDist, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Package directory is outside dist: $absoluteOut"
        }
        Remove-Item -LiteralPath $outDir -Recurse -Force
    }

    New-Item -ItemType Directory -Path $outDir | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $outDir "assets") | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $outDir "logs") | Out-Null

    Copy-Item -LiteralPath $releaseExe -Destination (Join-Path $outDir "TackleCast.exe")
    if ($Gnu) {
        foreach ($runtime in @('libgcc_s_seh-1.dll', 'libstdc++-6.dll', 'libwinpthread-1.dll')) {
            Copy-Item -LiteralPath (Join-Path 'C:\msys64\mingw64\bin' $runtime) -Destination $outDir
        }
        $mingwLicenses = Join-Path $root '.deps\mingw-release-licenses'
        New-Item -ItemType Directory -Force -Path $mingwLicenses | Out-Null
        foreach ($component in @('gcc-libs', 'winpthreads')) {
            $componentLicenses = Join-Path 'C:\msys64\mingw64\share\licenses' $component
            Ensure-Exists -Path $componentLicenses -Message "MinGW runtime licenses missing: $componentLicenses"
            Copy-Item -LiteralPath $componentLicenses -Destination $mingwLicenses -Recurse -Force
        }
    }
    if ($MfGpu) {
        $bridge = Join-Path $outDir 'tacklecast_mf_gpu.dll'
        if ($Gnu) {
            & 'C:\msys64\mingw64\bin\g++.exe' -std=c++17 -Wall -Wextra -Werror -shared -static src/mf_gpu_native.cpp -o $bridge -lmfplat -lmf -lmfreadwrite -lmfuuid -lole32 -ld3d11 -ld3d12 -ldxgi
        } else {
            $bridgeObj = Join-Path $targetRoot 'release\mf_gpu_native.obj'
            & cl /nologo /std:c++17 /EHsc /W4 /LD /MT src/mf_gpu_native.cpp "/Fo$bridgeObj" /link "/OUT:$bridge" mfplat.lib mf.lib mfreadwrite.lib mfuuid.lib ole32.lib d3d11.lib d3d12.lib dxgi.lib
        }
        if ($LASTEXITCODE -ne 0) { throw "MF GPU bridge build failed ($LASTEXITCODE)" }
        @'
@echo off
setlocal
set "TACKLECAST_MF_GPU=1"
set "TACKLECAST_MF_GPU_DLL=%~dp0tacklecast_mf_gpu.dll"
start "" /D "%~dp0" "%~dp0TackleCast.exe" %*
'@ | Set-Content -LiteralPath (Join-Path $outDir 'Run-MF-GPU.cmd') -Encoding ascii
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'MF_GPU_SETTINGS_AUDIT.md') -Destination $outDir
    }
    Copy-Item -LiteralPath $iconSrc -Destination (Join-Path $outDir "assets\icon.ico")
    Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination $outDir
    $licensesDir = Join-Path $outDir 'licenses'
    New-Item -ItemType Directory -Force -Path $licensesDir | Out-Null
    if ($Gnu) { Copy-Item -LiteralPath $mingwLicenses -Destination (Join-Path $licensesDir 'MinGW') -Recurse }
    if (Test-Path (Join-Path $ffmpegDir 'LICENSE.txt')) {
        Copy-Item (Join-Path $ffmpegDir 'LICENSE.txt') (Join-Path $licensesDir 'FFmpeg-LICENSE.txt')
    }
    Copy-Item (Join-Path $PSScriptRoot 'windows-dependencies.json') (Join-Path $licensesDir 'dependency-sources.json')

    if (Test-Path -LiteralPath $settingsSrc) {
        Copy-Item -LiteralPath $settingsSrc -Destination (Join-Path $outDir "tacklecast_settings.json")
    }

    $dllPatterns = @(
        "avcodec-*.dll",
        "avformat-*.dll",
        "avdevice-*.dll",
        "avutil-*.dll",
        "swresample-*.dll",
        "swscale-*.dll",
        "avfilter-*.dll"
    )

    foreach ($pattern in $dllPatterns) {
        $matches = Get-ChildItem -Path $ffmpegBin -Filter $pattern -File
        if (-not $matches) {
            throw "Missing FFmpeg DLL pattern '$pattern' in $ffmpegBin"
        }
        foreach ($dll in $matches) {
            Copy-Item -LiteralPath $dll.FullName -Destination (Join-Path $outDir $dll.Name)
        }
    }

    # nvJPEG + CUDA runtime, so GPU decode works on a machine without the CUDA
    # Toolkit installed. Without these the app still runs, but silently falls
    # back to software decode - which defeats the point of testing the GPU path.
    #
    # nvcuda.dll is deliberately NOT bundled: it ships with the NVIDIA display
    # driver and has to match the driver on the target machine.
    $gpuDllsCopied = @()
    if (-not $SkipGpuDlls) {
        if ($CudaDllDir -and -not (Test-Path -LiteralPath $CudaDllDir -PathType Container)) {
            throw "CudaDllDir does not exist: $CudaDllDir"
        }
        $searchDirs = New-Object System.Collections.Generic.List[string]
        if ($CudaDllDir) { $searchDirs.Add($CudaDllDir) }
        if ($env:GPU_RUNTIME_DIR) { $searchDirs.Add($env:GPU_RUNTIME_DIR) }
        if ($env:CUDA_PATH) {
            $searchDirs.Add((Join-Path $env:CUDA_PATH "bin\x64"))
            $searchDirs.Add((Join-Path $env:CUDA_PATH "bin"))
        }
        $cudaRoot = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA"
        if (Test-Path -LiteralPath $cudaRoot) {
            # Newest toolkit first.
            Get-ChildItem -LiteralPath $cudaRoot -Directory |
                Sort-Object { [version]($_.Name -replace '^v', '') } -Descending |
                ForEach-Object {
                    $searchDirs.Add((Join-Path $_.FullName "bin\x64"))
                    $searchDirs.Add((Join-Path $_.FullName "bin"))
                }
        }

        $selectedGpuDir = $null
        foreach ($dir in $searchDirs) {
            if (-not (Test-Path -LiteralPath $dir)) { continue }
            foreach ($major in @(13, 12, 11)) {
                $jpeg = Join-Path $dir "nvjpeg64_$major.dll"
                $runtimeName = if ($major -eq 11) { 'cudart64_110.dll' } else { "cudart64_$major.dll" }
                $runtime = Join-Path $dir $runtimeName
                if ((Test-Path -LiteralPath $jpeg) -and (Test-Path -LiteralPath $runtime)) {
                    foreach ($dllPath in @($jpeg, $runtime)) {
                        Copy-Item -LiteralPath $dllPath -Destination $outDir
                        $gpuDllsCopied += [IO.Path]::GetFileName($dllPath)
                    }
                    $selectedGpuDir = $dir
                    break
                }
            }
            if ($selectedGpuDir) { break }
            if ($CudaDllDir -and $dir -eq $CudaDllDir) {
                throw "CudaDllDir must contain a supported nvJPEG/CUDA pair from the same major runtime: $dir"
            }
        }
        if (-not $selectedGpuDir) {
            if ($RequireGpuDlls) { throw 'A complete compatible nvJPEG/CUDA runtime pair is required.' }
            Write-Warning 'No complete compatible nvJPEG/CUDA pair was found; NVIDIA MJPEG decoding will use software. Native MF GPU NV12 is independent of CUDA.'
        } elseif (Test-Path -LiteralPath (Join-Path $selectedGpuDir 'licenses')) {
            Copy-Item (Join-Path $selectedGpuDir 'licenses\*') $licensesDir -Force
        }
    }
    $commit = git -C $root rev-parse HEAD
    if ($LASTEXITCODE -ne 0) { throw 'Cannot record the source commit.' }
    @{
        version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "([^"]+)"').Matches[0].Groups[1].Value
        commit = $commit
        platform = 'windows-x86_64'
        working_tree_changes = [bool](git -C $root status --porcelain)
        gpu_runtime_bundled = ($gpuDllsCopied.Count -eq 2)
        mf_gpu_bundled = [bool]$MfGpu
        toolchain = $(if ($Gnu) { 'windows-gnu' } else { 'windows-msvc' })
    } | ConvertTo-Json | Set-Content (Join-Path $outDir 'build-info.json')

    if ($gpuDllsCopied.Count -gt 0) {
        $gpuNote = @"

GPU decode
----------
Bundled: $($gpuDllsCopied -join ', ')

GPU (nvJPEG) decode needs an NVIDIA GPU and a recent driver. These DLLs are from
the CUDA runtime, which requires a compatible NVIDIA driver. On an older
driver the app logs that CUDA is unavailable and falls back to software decode -
it still works, just with more CPU load.

To confirm which path is active, open logs\ and look for:
  "nvJPEG GPU decoder initialized in zero-copy mode"   <- best case
  "nvJPEG GPU decoder initialized"                     <- GPU decode, host copy
  "nvcuda.dll not found" / "GPU decode unavailable"    <- software decode
"@
    } else {
        $gpuNote = @"

GPU decode
----------
No CUDA DLLs were bundled. NVIDIA MJPEG decoding requires a compatible nvJPEG /
CUDA runtime on the target machine; otherwise MJPEG uses software decoding.
The experimental native MF GPU NV12 path is independent of CUDA and remains
available through Run-MF-GPU.cmd when included.
"@
    }

    $readmePath = Join-Path $outDir "README.txt"
    @"
TackleCast
==========

Requirements: Windows 10/11 64-bit. Everything needed is in this folder - no
installer, nothing to put on PATH. Keep the files together.

Running it
----------
1. Run TackleCast.exe.
2. Press Escape to open settings; pick your capture device, resolution, and FPS.
3. Press F11 for fullscreen. Escape closes the menu.

Experimental native NV12 GPU capture (when Run-MF-GPU.cmd is included):
Run Run-MF-GPU.cmd to enable the MF GPU path. Normal TackleCast.exe launch uses
the existing capture path. The GPU path needs DX12 and native NV12 support at
the selected mode; it honors explicit MJPEG/other format selections. Check for
"NV12 (MF GPU)" in the overlay. It performs GPU copies and CPU coordination;
this does not establish zero-copy USB transport or improved measured latency.

The mouse cursor hides after 3 seconds of stillness while the menu is closed.
The display is kept awake while frames are arriving and the window is visible.

Settings are saved to tacklecast_settings.json next to the exe when you exit.
Deleting that file resets everything to defaults.
Audio input Auto (follow video) uses only a unique match for the selected video
device. Without a match, audio stays disconnected. Select an explicit input to
use audio independently of video. Output Default uses the system output.
$gpuNote

Scaling filters
---------------
Settings has Video, Picture, Audio, and Display tabs in TackleCast's original
coral/navy colors. Scaling choices are Bilinear, Bicubic, and Lanczos.
Picture sliders update live, with no backdrop dimming while a slider is active.
Color Space and Color Range can use source metadata or manual overrides.
Auto labels identify any default explicitly. BT.2020 selects a matrix, not HDR.

Capture modes
-------------
Resolution, frame rate, and pixel format are matched to the selected device.
Fractional nominal rates remain distinct from measured delivered FPS.
Auto shows the opened format, for example Auto (NV12). Manual format requests
are checked against the opened stream; resolution/FPS are not silently lowered.
If mode discovery fails, the UI reports the failure rather than inventing modes.

If something goes wrong
-----------------------
- Log files are in the logs\ folder, newest last. They record the negotiated
  capture format, decode path, frame rate, and any errors. Send the newest one.
- A 30-second summary line reports rendered/uploaded FPS and GPU temperature.
- If capture fails, first check the card appears in Windows Camera or Device
  Manager, and that nothing else (OBS, Camera app, a browser tab) is already
  using it - most capture cards allow only one consumer at a time.
- If the picture is black but FPS is counting up, try a different resolution or
  frame rate; the log line starting "negotiated" shows what the card accepted.
"@ | Set-Content -Path $readmePath -NoNewline

    if ($Zip) {
        $zipPath = Join-Path $distRoot "$PackageName.zip"
        if (Test-Path -LiteralPath $zipPath) {
            Remove-Item -LiteralPath $zipPath -Force
        }
        Compress-Archive -Path (Join-Path $outDir "*") -DestinationPath $zipPath
        Write-Host "Created zip: $zipPath"
    }

    Write-Host "Package ready: $outDir"
}
finally {
    Pop-Location
}
