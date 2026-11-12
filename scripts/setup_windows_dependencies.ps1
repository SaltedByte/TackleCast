# -StaticFfmpeg builds the trimmed static FFmpeg that the standalone EXE links,
# with MSVC by default or MSYS2 MinGW with -Gnu, and points FFMPEG_DIR at it.
param([switch]$IncludeGpuRuntime, [switch]$StaticFfmpeg, [switch]$Gnu)
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

function Get-VerifiedFile([string]$Name, [string]$Extension) {
    $entry = $manifest.$Name
    $file = Join-Path $deps "$Name$Extension"
    if (-not (Test-Path $file) -or (Get-FileHash $file -Algorithm SHA256).Hash -ne $entry.sha256) {
        Invoke-WebRequest -UseBasicParsing -Uri $entry.url -OutFile $file
    }
    if ((Get-FileHash $file -Algorithm SHA256).Hash -ne $entry.sha256) {
        throw "Checksum mismatch for $Name"
    }
    return $file
}

function Build-StaticFfmpeg {
    $source = Get-VerifiedFile 'ffmpeg_source' '.tar.xz'
    $toolchain = if ($Gnu) { 'gcc' } else { 'msvc' }
    $prefix = Join-Path $deps "ffmpeg-static-$toolchain"
    $script = Join-Path $PSScriptRoot 'build_static_ffmpeg.sh'
    # Rebuild only when the pinned source or the build recipe changes.
    $stamp = $manifest.ffmpeg_source.sha256 + ' ' + (Get-FileHash $script -Algorithm SHA256).Hash
    $stampFile = Join-Path $prefix '.build-stamp'
    if ((Test-Path $stampFile) -and (Get-Content $stampFile -Raw).Trim() -eq $stamp) {
        Write-Host "Static FFmpeg is up to date: $prefix"
        return $prefix
    }

    $msys = 'C:\msys64'
    $bash = Join-Path $msys 'usr\bin\bash.exe'
    if (-not (Test-Path $bash)) { throw "MSYS2 is required to build FFmpeg: $bash not found." }
    # Windows PowerShell turns a native tool's stderr into a terminating error
    # when output is redirected; judge pacman and the build by exit code alone.
    $ErrorActionPreference = 'Continue'
    # configure needs make and cmp; nasm enables the SIMD decode paths. Tool
    # output goes to the console (Out-Host), not into this function's result.
    & (Join-Path $msys 'usr\bin\pacman.exe') -S --noconfirm --needed make diffutils nasm | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'Installing FFmpeg build tools with pacman failed.' }

    if ($Gnu) {
        $env:MSYSTEM = 'MINGW64'
    } else {
        if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
            $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
            $vsPath = if (Test-Path $vswhere) {
                & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
            }
            if (-not $vsPath) { throw 'Visual Studio C++ build tools are required (or pass -Gnu).' }
            & (Join-Path $vsPath 'Common7\Tools\Launch-VsDevShell.ps1') -Arch amd64 -HostArch amd64 -SkipAutomaticLocation | Out-Host
        }
        # Keep the MSVC environment visible inside MSYS2. FFmpeg's configure
        # finds MSVC's link.exe itself, despite coreutils' link on PATH.
        $env:MSYSTEM = 'MSYS'
        $env:MSYS2_PATH_TYPE = 'inherit'
    }
    $env:CHERE_INVOKING = '1'
    if (Test-Path $prefix) { Remove-Item -LiteralPath $prefix -Recurse -Force }
    $unix = { param($path) $path -replace '\\', '/' }
    & $bash -lc "bash '$(& $unix $script)' '$(& $unix $source)' '$(& $unix $prefix)' $toolchain" | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "Static FFmpeg build failed ($LASTEXITCODE)." }
    $ErrorActionPreference = 'Stop'
    Set-Content -LiteralPath $stampFile -Value $stamp
    return $prefix
}

$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
if ($StaticFfmpeg) {
    $ffmpegRoot = Build-StaticFfmpeg
    $env:FFMPEG_DIR = $ffmpegRoot
} else {
    $ffmpegArchive = Get-VerifiedArchive 'ffmpeg'
    $ffmpegRoot = (Get-ChildItem $ffmpegArchive -Directory | Select-Object -First 1).FullName
    $env:FFMPEG_DIR = $ffmpegRoot
    $env:PATH = "$(Join-Path $ffmpegRoot 'bin');$env:PATH"
}
if ($env:GITHUB_ENV) {
    "FFMPEG_DIR=$ffmpegRoot" | Out-File $env:GITHUB_ENV -Encoding utf8 -Append
    "LIBCLANG_PATH=$env:LIBCLANG_PATH" | Out-File $env:GITHUB_ENV -Encoding utf8 -Append
    if (-not $StaticFfmpeg) {
        (Join-Path $ffmpegRoot 'bin') | Out-File $env:GITHUB_PATH -Encoding utf8 -Append
    }
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
Write-Host "Pinned $(if ($StaticFfmpeg) { 'static' } else { 'shared' }) FFmpeg ready: $ffmpegRoot"
