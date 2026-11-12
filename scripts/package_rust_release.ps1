# Build the standalone Windows release: a single TackleCast.exe with FFmpeg
# linked statically and the MF GPU bridge, nvJPEG/CUDA runtime and license
# notices built in. Run scripts/setup_windows_dependencies.ps1 -StaticFfmpeg
# (-IncludeGpuRuntime for NVIDIA decode) first; pass -Gnu to both for MinGW.
param(
    [string]$PackageName = "TackleCast-Windows-x86_64",
    [switch]$NoGpuRuntime,
    [switch]$Gnu
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if ($PackageName -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$') {
    throw 'PackageName must be a file name, without path separators.'
}

$root = Split-Path -Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'windows_build_environment.ps1')
if ($Gnu) { . (Join-Path $PSScriptRoot 'windows_gnu_build_environment.ps1') }

if (-not (Test-Path -LiteralPath (Join-Path $env:FFMPEG_DIR 'lib\pkgconfig\libavdevice.pc')) -or
    (Get-ChildItem -Path (Join-Path $env:FFMPEG_DIR 'bin') -Filter 'avcodec-*.dll' -ErrorAction SilentlyContinue)) {
    throw "FFMPEG_DIR ($env:FFMPEG_DIR) is not a static FFmpeg build. Run setup_windows_dependencies.ps1 -StaticFfmpeg."
}
$features = if ($NoGpuRuntime) { 'static-ffmpeg' } else { 'standalone' }
if (-not $NoGpuRuntime -and -not $env:GPU_RUNTIME_DIR) {
    throw 'GPU_RUNTIME_DIR is not set. Run setup_windows_dependencies.ps1 -IncludeGpuRuntime, or pass -NoGpuRuntime.'
}

$distRoot = Join-Path $root 'dist'
$outExe = Join-Path $distRoot "$PackageName.exe"
# A separate target directory keeps the static build from evicting the
# shared-FFmpeg development build out of Cargo's cache, and vice versa.
$targetRoot = Join-Path $root 'target\standalone'

Push-Location $root
try {
    Write-Host "Building standalone release ($features)..."
    $cargoArgs = [System.Collections.Generic.List[string]]::new()
    if ($Gnu) { $cargoArgs.Add('+stable-x86_64-pc-windows-gnu') }
    $cargoArgs.AddRange([string[]]@('build', '--release', '--locked', '--features', $features, '--target-dir', $targetRoot))
    & cargo $cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "Release build failed ($LASTEXITCODE)" }

    New-Item -ItemType Directory -Force -Path $distRoot | Out-Null
    Copy-Item -LiteralPath (Join-Path $targetRoot 'release\tacklecast.exe') -Destination $outExe -Force

    $python = (Get-Command python -ErrorAction SilentlyContinue | Where-Object { $_.Source -notmatch 'WindowsApps' } | Select-Object -First 1).Source
    if ($python) {
        & $python -I (Join-Path $PSScriptRoot 'verify_package.py') --exe $outExe
        if ($LASTEXITCODE -ne 0) { throw 'Standalone verification failed' }
    } else {
        Write-Warning 'Python not found; skipped the DLL import check (scripts/verify_package.py --exe).'
    }
    $hash = (Get-FileHash -LiteralPath $outExe -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath "$outExe.sha256" -Value "$hash  $PackageName.exe" -NoNewline
    Write-Host "Standalone executable ready: $outExe ($([math]::Round((Get-Item $outExe).Length / 1MB, 1)) MB)"
}
finally {
    Pop-Location
}
