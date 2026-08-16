param(
    [string]$PackageName = "TackleCast-Rust",
    [switch]$Zip
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

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
$distRoot = Join-Path $root "dist"
$outDir = Join-Path $distRoot $PackageName
$releaseExe = Join-Path $root "target\x86_64-pc-windows-msvc\release\tacklecast.exe"
$settingsSrc = Join-Path $root "tacklecast_settings.json"
$iconSrc = Join-Path $root "assets\icon.ico"
$ffmpegDir = $env:FFMPEG_DIR

if ([string]::IsNullOrWhiteSpace($ffmpegDir)) {
    throw "FFMPEG_DIR is not set. Set it to your FFmpeg root (contains bin/, lib/, include/)."
}

$ffmpegBin = Join-Path $ffmpegDir "bin"
Ensure-Exists -Path $ffmpegBin -Message "FFMPEG_DIR\bin not found: $ffmpegBin"

Push-Location $root
try {
    Write-Host "Building release binary..."
    cargo build --release

    Ensure-Exists -Path $releaseExe -Message "Release executable missing: $releaseExe"
    Ensure-Exists -Path $iconSrc -Message "Icon missing: $iconSrc"

    if (Test-Path -LiteralPath $outDir) {
        Remove-Item -LiteralPath $outDir -Recurse -Force
    }

    New-Item -ItemType Directory -Path $outDir | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $outDir "assets") | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $outDir "logs") | Out-Null

    Copy-Item -LiteralPath $releaseExe -Destination (Join-Path $outDir "TackleCast.exe")
    Copy-Item -LiteralPath $iconSrc -Destination (Join-Path $outDir "assets\icon.ico")

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

    $readmePath = Join-Path $outDir "README.txt"
    @"
TackleCast Rust Build
=====================

1. Run TackleCast.exe.
2. Press Escape to open settings.
3. Press F11 for fullscreen.

Notes:
- Log files are written to the logs\ folder.
- If capture fails, verify your capture card appears in Windows camera devices.
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
