# Optional MSYS2 fallback when the MSVC/Windows SDK installer is unavailable.
# Dot-source this file; all environment changes stay in the current session.
. (Join-Path $PSScriptRoot 'windows_build_environment.ps1')
$mingwBin = 'C:\msys64\mingw64\bin'
if (-not (Test-Path -LiteralPath (Join-Path $mingwBin 'gcc.exe'))) {
    throw "MSYS2 MinGW gcc was not found in $mingwBin."
}
$clangRoot = Split-Path $env:LIBCLANG_PATH -Parent
$clangResources = Join-Path $clangRoot 'lib\clang'
$resourceDirectory = Get-ChildItem -LiteralPath $clangResources -Directory |
    Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'include\mm_malloc.h') } |
    Sort-Object { [int]($_.Name.Split('.')[0]) } -Descending | Select-Object -First 1
if (-not $resourceDirectory) { throw "Clang resource headers not found in $clangResources." }
$env:PATH = "$mingwBin;$env:PATH"
$env:BINDGEN_EXTRA_CLANG_ARGS = '--target=x86_64-w64-windows-gnu -isystem C:/msys64/mingw64/include -resource-dir "' +
    $resourceDirectory.FullName.Replace('\', '/') + '"'
