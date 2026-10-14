param([switch]$IncludeGpuRuntime)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$deps = Join-Path $root '.deps'
New-Item -ItemType Directory -Force -Path $deps | Out-Null
$manifest = Get-Content (Join-Path $PSScriptRoot 'windows-dependencies.json') -Raw | ConvertFrom-Json

function Get-VerifiedArchive([string]$Name) {
    $entry = $manifest.$Name
    $archive = Join-Path $deps "$Name.zip"
    if (-not (Test-Path $archive) -or (Get-FileHash $archive -Algorithm SHA256).Hash -ne $entry.sha256) {
        Invoke-WebRequest -UseBasicParsing -Uri $entry.url -OutFile $archive
    }
    if ((Get-FileHash $archive -Algorithm SHA256).Hash -ne $entry.sha256) {
        throw "Checksum mismatch for $Name"
    }
    $destination = Join-Path $deps $Name
    Expand-Archive -LiteralPath $archive -DestinationPath $destination -Force
    return $destination
}

$ffmpegArchive = Get-VerifiedArchive 'ffmpeg'
$ffmpegRoot = (Get-ChildItem $ffmpegArchive -Directory | Select-Object -First 1).FullName
$env:FFMPEG_DIR = $ffmpegRoot
$env:PATH = "$(Join-Path $ffmpegRoot 'bin');$env:PATH"
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
if ($env:GITHUB_ENV) {
    "FFMPEG_DIR=$ffmpegRoot" | Out-File $env:GITHUB_ENV -Encoding utf8 -Append
    "LIBCLANG_PATH=$env:LIBCLANG_PATH" | Out-File $env:GITHUB_ENV -Encoding utf8 -Append
    (Join-Path $ffmpegRoot 'bin') | Out-File $env:GITHUB_PATH -Encoding utf8 -Append
}
if ($IncludeGpuRuntime) {
    $gpuDir = Join-Path $deps 'gpu-runtime'
    $licenses = Join-Path $gpuDir 'licenses'
    New-Item -ItemType Directory -Force -Path $licenses | Out-Null
    foreach ($name in @('cuda_cudart', 'libnvjpeg')) {
        $unpacked = Get-VerifiedArchive $name
        $dlls = @(Get-ChildItem $unpacked -Recurse -File -Filter '*.dll')
        if ($dlls.Count -eq 0) { throw "No runtime DLLs found in $name" }
        $dlls | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $gpuDir -Force }
        Get-ChildItem $unpacked -Recurse -File | Where-Object { $_.Name -match 'LICENSE|EULA' } |
            ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $licenses "$name-$($_.Name)") -Force }
    }
    $env:GPU_RUNTIME_DIR = $gpuDir
    if ($env:GITHUB_ENV) {
        "GPU_RUNTIME_DIR=$gpuDir" | Out-File $env:GITHUB_ENV -Encoding utf8 -Append
    }
}
Write-Host "Pinned FFmpeg ready: $ffmpegRoot"
