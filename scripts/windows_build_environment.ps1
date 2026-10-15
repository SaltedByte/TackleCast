# Dot-source this helper before invoking Cargo directly. Defaults apply only to
# this PowerShell session and do not affect Linux or persistent Windows settings.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$buildRoot = Split-Path $PSScriptRoot -Parent
if ([string]::IsNullOrWhiteSpace($env:FFMPEG_DIR)) {
    $localFfmpeg = Join-Path $buildRoot '.deps\ffmpeg'
    $localCandidates = @()
    if (Test-Path -LiteralPath $localFfmpeg) {
        if (Test-Path -LiteralPath (Join-Path $localFfmpeg 'include')) { $localCandidates += $localFfmpeg }
        $localCandidates += @(Get-ChildItem -LiteralPath $localFfmpeg -Directory | Sort-Object Name | Select-Object -ExpandProperty FullName)
    }
    $localCandidates += 'C:\ffmpeg'
    foreach ($candidate in $localCandidates) {
        if ((Test-Path -LiteralPath (Join-Path $candidate 'include')) -and
            (Test-Path -LiteralPath (Join-Path $candidate 'lib')) -and
            (Test-Path -LiteralPath (Join-Path $candidate 'bin'))) {
            $env:FFMPEG_DIR = $candidate
            break
        }
    }
}
if ([string]::IsNullOrWhiteSpace($env:FFMPEG_DIR) -or
    -not (Test-Path -LiteralPath (Join-Path $env:FFMPEG_DIR 'include')) -or
    -not (Test-Path -LiteralPath (Join-Path $env:FFMPEG_DIR 'lib')) -or
    -not (Test-Path -LiteralPath (Join-Path $env:FFMPEG_DIR 'bin'))) {
    throw 'Set FFMPEG_DIR to an FFmpeg development build containing bin/, include/, and lib/, or run scripts/setup_windows_dependencies.ps1.'
}
if ([string]::IsNullOrWhiteSpace($env:LIBCLANG_PATH)) { $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin' }
if (-not (Test-Path -LiteralPath (Join-Path $env:LIBCLANG_PATH 'libclang.dll'))) {
    throw "libclang.dll was not found in LIBCLANG_PATH: $env:LIBCLANG_PATH"
}
$env:PATH = (Join-Path $env:FFMPEG_DIR 'bin') + ';' + $env:PATH
