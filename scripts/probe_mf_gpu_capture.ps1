param([switch]$Capture, [ValidateRange(1,2400)][int]$Frames = 60)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path $PSScriptRoot -Parent
$outputRoot = Join-Path $repoRoot '.deps\mf-gpu-probe'
New-Item -ItemType Directory -Force $outputRoot | Out-Null
$compiler = 'C:\msys64\mingw64\bin\g++.exe'
if (-not (Test-Path $compiler)) { throw 'MSYS2 MinGW g++ is required.' }
$env:PATH = (Split-Path $compiler -Parent) + ';' + $env:PATH
$exe = Join-Path $outputRoot 'mf_gpu_capture_probe.exe'
& $compiler -std=c++17 -municode -static (Join-Path $PSScriptRoot 'mf_gpu_capture_probe.cpp') -o $exe -lmfplat -lmf -lmfreadwrite -lmfuuid -lole32 -ld3d11 -ld3d12 -ldxgi
if ($LASTEXITCODE -ne 0) { throw 'Probe compilation failed.' }
$start = New-Object System.Diagnostics.ProcessStartInfo
$start.FileName = $exe
if ($Capture) { $start.Arguments = "--capture $Frames" }
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$start.RedirectStandardOutput = $true
$start.RedirectStandardError = $true
$process = New-Object System.Diagnostics.Process
$process.StartInfo = $start
try {
    if (-not $process.Start()) { throw 'Probe failed to start.' }
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    $timedOut = -not $process.WaitForExit(30000)
    if ($timedOut) { $process.Kill(); $process.WaitForExit() }
    $result = $stdout.GetAwaiter().GetResult() + $stderr.GetAwaiter().GetResult()
    $result | Set-Content (Join-Path $outputRoot 'probe.log')
    Write-Host $result
    if ($timedOut) { throw 'Probe exceeded 30 seconds and was terminated. See .deps/mf-gpu-probe/probe.log.' }
    if ($process.ExitCode -ne 0) { throw "Probe failed ($($process.ExitCode))." }
} finally { $process.Dispose() }
