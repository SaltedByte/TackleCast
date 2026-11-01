param(
    [string]$SdkRoot,
    [string]$JpegPath,
    [switch]$Dx12,
    [switch]$Dx12Bridge,
    [ValidateRange(1,10000)][int]$Frames = 1,
    [ValidateRange(64,7680)][int]$SyntheticWidth = 1920,
    [ValidateRange(64,4320)][int]$SyntheticHeight = 1080
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path $PSScriptRoot -Parent
$probeRoot = Join-Path $repoRoot '.deps\amf-probe'
New-Item -ItemType Directory -Force $probeRoot | Out-Null
$logPath = Join-Path $probeRoot 'probe.log'
Start-Transcript -Path $logPath -Force | Out-Null
try {
    if (-not [Environment]::Is64BitProcess) { throw 'Run this in 64-bit Windows PowerShell.' }
    if ($Dx12 -and $Dx12Bridge) { throw 'Choose either -Dx12 or -Dx12Bridge.' }
    Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion | Format-Table
    # Prefer MSVC; MSYS2 MinGW includes the required Windows headers too.
    $mingwCompiler = 'C:\msys64\mingw64\bin\g++.exe'
    if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue) -and -not (Test-Path $mingwCompiler)) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        if (-not (Test-Path $vswhere)) { throw 'Install Visual Studio C++ build tools and the Windows SDK.' }
        $vsPath = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if (-not $vsPath) { throw 'No Visual Studio C++ build tools installation found.' }
        & (Join-Path $vsPath 'Common7\Tools\Launch-VsDevShell.ps1') -Arch amd64 -HostArch amd64 -SkipAutomaticLocation
    }
    if ([string]::IsNullOrWhiteSpace($SdkRoot)) {
        $cachedSdk = Join-Path $probeRoot 'sdk-path.txt'
        if (Test-Path $cachedSdk) { $SdkRoot = (Get-Content $cachedSdk -Raw).Trim() }
        if (-not $SdkRoot -or -not (Test-Path (Join-Path $SdkRoot 'amf\public\include\core\Factory.h'))) {
            # Resolve once to an immutable revision; retain the revision and archive hash.
            $commit = Invoke-RestMethod 'https://api.github.com/repos/GPUOpen-LibrariesAndSDKs/AMF/commits/master'
            $revision = $commit.sha
            if ($revision -notmatch '^[0-9a-f]{40}$') { throw 'Invalid AMF revision returned by GitHub.' }
            $archive = Join-Path $probeRoot 'amf-sdk.zip'
            Write-Host "Downloading official AMD AMF headers at $revision"
            Invoke-WebRequest "https://codeload.github.com/GPUOpen-LibrariesAndSDKs/AMF/zip/$revision" -OutFile $archive -UseBasicParsing -TimeoutSec 180
            Get-FileHash $archive -Algorithm SHA256 | Format-List
            Expand-Archive $archive -DestinationPath $probeRoot -Force
            $SdkRoot = Join-Path $probeRoot "AMF-$revision"
            Set-Content $cachedSdk $SdkRoot
        }
    }
    $SdkRoot = (Resolve-Path -LiteralPath $SdkRoot).Path
    $includeRoot = Join-Path $SdkRoot 'amf\public\include'
    if (-not (Test-Path (Join-Path $includeRoot 'core\Factory.h'))) { throw "Invalid AMF SDK root: $SdkRoot" }
    Write-Host "AMF SDK: $SdkRoot"
    $exePath = Join-Path $probeRoot 'amf_probe.exe'
    Push-Location $probeRoot
    try {
        if (Get-Command cl.exe -ErrorAction SilentlyContinue) {
            & cl.exe /nologo /EHsc /std:c++17 /W4 "/I$includeRoot" (Join-Path $PSScriptRoot 'amf_probe.cpp') "/Fe:$exePath" /link d3d11.lib d3d12.lib dxgi.lib ole32.lib
        } else {
            $env:PATH = (Split-Path $mingwCompiler -Parent) + ';' + $env:PATH
            & $mingwCompiler -std=c++17 -Wall -municode -static -I $includeRoot (Join-Path $PSScriptRoot 'amf_probe.cpp') -o $exePath -ld3d11 -ld3d12 -ldxgi -lole32
        }
        if ($LASTEXITCODE -ne 0) { throw "Probe compilation failed ($LASTEXITCODE)." }
    } finally { Pop-Location }
    Add-Type -AssemblyName System.Drawing
    if ([string]::IsNullOrWhiteSpace($JpegPath)) {
        $JpegPath = Join-Path $probeRoot "synthetic-${SyntheticWidth}x${SyntheticHeight}.jpg"
        $bitmap = New-Object System.Drawing.Bitmap $SyntheticWidth,$SyntheticHeight
        try {
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            try {
                $rectangle = New-Object System.Drawing.Rectangle 0,0,$SyntheticWidth,$SyntheticHeight
                $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush $rectangle,([System.Drawing.Color]::Red),([System.Drawing.Color]::Blue),45.0
                try { $graphics.FillRectangle($brush, $rectangle) }
                finally { $brush.Dispose() }
                $graphics.FillRectangle([System.Drawing.Brushes]::Lime, 137, 213, 419, 317)
                $graphics.FillRectangle([System.Drawing.Brushes]::White, 1003, 719, 517, 211)
                $graphics.DrawLine([System.Drawing.Pens]::Black, 0, 0, 1919, 1079)
            }
            finally { $graphics.Dispose() }
            $bitmap.Save($JpegPath, [System.Drawing.Imaging.ImageFormat]::Jpeg)
        } finally { $bitmap.Dispose() }
    }
    $JpegPath = (Resolve-Path -LiteralPath $JpegPath).Path
    $image = [System.Drawing.Image]::FromFile($JpegPath)
    try { $imageWidth = $image.Width; $imageHeight = $image.Height }
    finally { $image.Dispose() }
    Write-Host "Testing $JpegPath ($imageWidth x $imageHeight)"
    if ($Dx12Bridge) { & $exePath $JpegPath $imageWidth $imageHeight bridge $Frames | Out-Host }
    elseif ($Dx12) { & $exePath $JpegPath $imageWidth $imageHeight dx12 $Frames | Out-Host }
    else { & $exePath $JpegPath $imageWidth $imageHeight dx11 $Frames | Out-Host }
    if ($LASTEXITCODE -ne 0) { throw "AMF decode probe failed ($LASTEXITCODE). See $logPath" }
    Write-Host 'PASS: at least one GPU output format decoded and copied to host memory.'
    Write-Host 'This does not yet establish capture-card compatibility, picture accuracy, or latency.'
} finally {
    Stop-Transcript | Out-Null
    Write-Host "Probe log: $logPath"
}

