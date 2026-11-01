# Compile the native Media Foundation bridge. Used by packaging and CI.
param(
    [Parameter(Mandatory = $true)][string]$Output,
    [switch]$Gnu
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$source = Join-Path (Split-Path $PSScriptRoot -Parent) 'src\mf_gpu_native.cpp'
$Output = [IO.Path]::GetFullPath($Output)
New-Item -ItemType Directory -Force -Path (Split-Path $Output -Parent) | Out-Null

if ($Gnu) {
    & 'C:\msys64\mingw64\bin\g++.exe' -std=c++17 -Wall -Wextra -Werror -shared -static $source -o $Output -lmfplat -lmf -lmfreadwrite -lmfuuid -lole32 -ld3d11 -ld3d12 -ldxgi
} else {
    if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        if (-not (Test-Path -LiteralPath $vswhere)) { throw 'Install Visual Studio C++ build tools, or package with -Gnu.' }
        $vsPath = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if (-not $vsPath) { throw 'No Visual Studio C++ build tools installation found.' }
        & (Join-Path $vsPath 'Common7\Tools\Launch-VsDevShell.ps1') -Arch amd64 -HostArch amd64 -SkipAutomaticLocation
    }
    # Keep the object, import library and export file out of the package.
    $intermediate = Join-Path ([IO.Path]::GetTempPath()) ("tacklecast-mf-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $intermediate | Out-Null
    try {
        & cl /nologo /std:c++17 /EHsc /W4 /LD /MT $source "/Fo$intermediate\mf_gpu_native.obj" /link "/OUT:$Output" "/IMPLIB:$intermediate\tacklecast_mf_gpu.lib" mfplat.lib mf.lib mfreadwrite.lib mfuuid.lib ole32.lib d3d11.lib d3d12.lib dxgi.lib
    } finally {
        Remove-Item -LiteralPath $intermediate -Recurse -Force
    }
}
if ($LASTEXITCODE -ne 0) { throw "MF GPU bridge build failed ($LASTEXITCODE)" }
