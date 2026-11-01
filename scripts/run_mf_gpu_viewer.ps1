# Experimental local viewer, opt-in only. Close other capture apps first.
param([switch]$Build, [switch]$Pooled, [switch]$ColorMetadata)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    . (Join-Path $PSScriptRoot 'windows_gnu_build_environment.ps1')
    $env:PATH = (Join-Path $env:USERPROFILE '.cargo\bin') + ';' + $env:PATH
    $bridgeRoot = Join-Path $root '.deps\mf-gpu-probe'
    New-Item -ItemType Directory -Force $bridgeRoot | Out-Null
    $bridgeName = if ($ColorMetadata) { 'tacklecast_mf_gpu_color.dll' } elseif ($Pooled) { 'tacklecast_mf_gpu_pool.dll' } else { 'tacklecast_mf_gpu.dll' }
    $exeName = if ($ColorMetadata) { 'tacklecast-mf-gpu-color.exe' } elseif ($Pooled) { 'tacklecast-mf-gpu-pool.exe' } else { 'tacklecast-mf-gpu.exe' }
    $bridge = Join-Path $bridgeRoot $bridgeName
    $exe = Join-Path (Join-Path $root 'target\release') $exeName
    if ($Build) {
        & 'C:\msys64\mingw64\bin\g++.exe' -std=c++17 -shared -static src/mf_gpu_native.cpp -o $bridge -lmfplat -lmf -lmfreadwrite -lmfuuid -lole32 -ld3d11 -ld3d12 -ldxgi
        if ($LASTEXITCODE -ne 0) { throw 'Native bridge build failed.' }
        & cargo +stable-x86_64-pc-windows-gnu rustc --release --locked --bin tacklecast -- "--emit=link=$exe"
        if ($LASTEXITCODE -ne 0) { throw 'GPU viewer build failed.' }
    }
    if (-not (Test-Path $bridge) -or -not (Test-Path $exe)) { throw 'Run with -Build first.' }
    $previousEnabled = $env:TACKLECAST_MF_GPU
    $previousDll = $env:TACKLECAST_MF_GPU_DLL
    try {
        $env:TACKLECAST_MF_GPU = '1'
        $env:TACKLECAST_MF_GPU_DLL = $bridge
        & $exe
        if ($LASTEXITCODE -ne 0) { throw "Experimental viewer exited with $LASTEXITCODE." }
    } finally {
        $env:TACKLECAST_MF_GPU = $previousEnabled
        $env:TACKLECAST_MF_GPU_DLL = $previousDll
    }
} finally { Pop-Location }
