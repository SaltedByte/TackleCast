param(
    [string]$PackageName = "TackleCast-Rust",
    [switch]$Zip,
    [switch]$SkipGpuDlls,
    [switch]$RequireGpuDlls,
    [string]$CudaDllDir,
    [string]$FrameGenDir,
    [switch]$RequireFrameGen,
    [string]$VcRuntimeDir
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
    cargo build --release --locked
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
    Copy-Item -LiteralPath $iconSrc -Destination (Join-Path $outDir "assets\icon.ico")
    Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination $outDir
    $licensesDir = Join-Path $outDir 'licenses'
    New-Item -ItemType Directory -Force -Path $licensesDir | Out-Null
    if (Test-Path (Join-Path $ffmpegDir 'LICENSE.txt')) {
        Copy-Item (Join-Path $ffmpegDir 'LICENSE.txt') (Join-Path $licensesDir 'FFmpeg-LICENSE.txt')
    }
    Copy-Item (Join-Path $PSScriptRoot 'windows-dependencies.json') (Join-Path $licensesDir 'dependency-sources.json')

    # Portable packages start with defaults; never copy developer device/settings files.

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

    # Ensure postproc-*.dll is always bundled when available in the FFmpeg installation
    # (required on Windows when avcodec or avfilter dynamically link against it)
    $postprocMatches = Get-ChildItem -Path $ffmpegBin -Filter "postproc-*.dll" -File
    if (-not $postprocMatches -and (Test-Path "C:\ffmpeg\bin\postproc-*.dll")) {
        $postprocMatches = Get-ChildItem -Path "C:\ffmpeg\bin" -Filter "postproc-*.dll" -File
    }
    foreach ($dll in $postprocMatches) {
        Copy-Item -LiteralPath $dll.FullName -Destination (Join-Path $outDir $dll.Name)
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
        if (Test-Path -LiteralPath (Join-Path $root ".deps\gpu-runtime")) {
            $searchDirs.Add((Join-Path $root ".deps\gpu-runtime"))
        }
        if (Test-Path -LiteralPath (Join-Path $distRoot "TackleCast-v2.2.1")) {
            $searchDirs.Add((Join-Path $distRoot "TackleCast-v2.2.1"))
        }
        if (Test-Path -LiteralPath (Join-Path $distRoot "TackleCast-Windows-x86_64")) {
            $searchDirs.Add((Join-Path $distRoot "TackleCast-Windows-x86_64"))
        }
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
            Write-Warning 'No complete compatible nvJPEG/CUDA pair was found; this bundle uses software decoding.'
        } elseif (Test-Path -LiteralPath (Join-Path $selectedGpuDir 'licenses')) {
            Copy-Item (Join-Path $selectedGpuDir 'licenses\*') $licensesDir -Force
        }
    }
    $framegenBundled = $false
    if (-not $FrameGenDir) {
        $candidateRife = Join-Path $root '.deps\rife'
        if (Test-Path -LiteralPath $candidateRife) { $FrameGenDir = $candidateRife }
    }
    if ($FrameGenDir) {
        & (Join-Path $PSScriptRoot 'bundle_framegen.ps1') -Source $FrameGenDir -Package $outDir
        $framegenBundled = $true
    } elseif ($RequireFrameGen) {
        throw 'The complete pinned RIFE runtime/model bundle is required. Set FrameGenDir.'
    }
    if (-not $VcRuntimeDir) {
        $vsRoots = @($env:ProgramFiles, ${env:ProgramFiles(x86)}) | Where-Object { $_ } | Select-Object -Unique
        $crtCandidates = foreach ($base in $vsRoots) {
            Get-ChildItem -Path (Join-Path $base 'Microsoft Visual Studio\*\*\VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT') -Directory -ErrorAction SilentlyContinue
        }
        $VcRuntimeDir = $crtCandidates |
            Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'msvcp140_1.dll') } |
            Sort-Object { [version](Get-Item (Join-Path $_.FullName 'vcruntime140.dll')).VersionInfo.FileVersionRaw } -Descending |
            Select-Object -First 1 -ExpandProperty FullName
    }
    if (-not $VcRuntimeDir) { throw 'Set VcRuntimeDir to the licensed x64 release VC CRT redistribution directory.' }
    foreach ($name in @('vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll', 'msvcp140_1.dll')) {
        Ensure-Exists (Join-Path $VcRuntimeDir $name) "Missing app-local C++ runtime: $name"
    }
    $crtFiles = @(Get-ChildItem -LiteralPath $VcRuntimeDir -Filter '*.dll' -File)
    foreach ($dll in $crtFiles) {
        $signature = Get-AuthenticodeSignature -LiteralPath $dll.FullName
        if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft') {
            throw "Invalid Microsoft runtime signature: $($dll.Name)"
        }
        Copy-Item -LiteralPath $dll.FullName -Destination $outDir
    }
    @(
        'Microsoft Visual C++ release runtime DLLs are redistributed app-locally from Visual Studio x64 CRT Redist.'
        'Keep these files with the application. These components remain subject to Microsoft license terms.'
        'Deployment and servicing: https://learn.microsoft.com/en-us/cpp/windows/deployment-in-visual-cpp'
        'Redistribution terms: https://visualstudio.microsoft.com/license-terms/'
    ) | Set-Content -LiteralPath (Join-Path $licensesDir 'Microsoft-VC-Runtime.txt') -Encoding UTF8
    $commit = git -C $root rev-parse HEAD
    if ($LASTEXITCODE -ne 0) { throw 'Cannot record the source commit.' }
    @{
        version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "([^"]+)"').Matches[0].Groups[1].Value
        commit = $commit
        platform = 'windows-x86_64'
        working_tree_changes = [bool](git -C $root status --porcelain)
        gpu_runtime_bundled = ($gpuDllsCopied.Count -eq 2)
        framegen_bundled = $framegenBundled
        vc_runtime_bundled = @($crtFiles | ForEach-Object { @{ file = $_.Name; version = $_.VersionInfo.FileVersion; sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() } })
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
No CUDA DLLs were bundled, so GPU decode only works if the target machine has
the CUDA Toolkit installed. Otherwise the app falls back to software decode.
"@
    }

    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'windows-package-readme.txt') -Destination (Join-Path $outDir 'README.txt')
    if ($framegenBundled) {
        Copy-Item -LiteralPath (Join-Path $root 'README-FrameGen.md') -Destination $outDir
    }

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
