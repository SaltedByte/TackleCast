param(
    [Parameter(Mandatory = $true)][string]$Source,
    [Parameter(Mandatory = $true)][string]$Package
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$pinsPath = Join-Path $PSScriptRoot 'framegen-assets.json'
$pins = Get-Content -LiteralPath $pinsPath -Raw | ConvertFrom-Json
$sourceRoot = [IO.Path]::GetFullPath($Source) + [IO.Path]::DirectorySeparatorChar
# Validate the entire allowlist before writing anything. Do not copy unrelated
# experimental models, settings, executables, logs or tools from the source.
foreach ($entry in $pins.files) {
    $inputPath = [IO.Path]::GetFullPath((Join-Path $Source $entry.path))
    if (-not $inputPath.StartsWith($sourceRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Asset path escapes source: $($entry.path)"
    }
    if (-not (Test-Path -LiteralPath $inputPath -PathType Leaf)) { throw "Missing FG asset: $($entry.path)" }
    if ((Get-Item -LiteralPath $inputPath).Length -ne $entry.bytes -or
        (Get-FileHash -LiteralPath $inputPath -Algorithm SHA256).Hash -ne $entry.sha256) {
        throw "FG asset hash/size mismatch: $($entry.path)"
    }
}
$targetRoot = Join-Path $Package 'rife'
if (Test-Path -LiteralPath $targetRoot) { throw "FG destination already exists: $targetRoot" }
foreach ($entry in $pins.files) {
    $relative = $entry.path
    $outputPath = if ($relative.StartsWith('licenses/')) {
        Join-Path (Join-Path $Package 'licenses') ([IO.Path]::GetFileName($relative))
    } else { Join-Path $targetRoot $relative }
    New-Item -ItemType Directory -Force -Path (Split-Path $outputPath -Parent) | Out-Null
    Copy-Item -LiteralPath (Join-Path $Source $relative) -Destination $outputPath
    if ((Get-FileHash -LiteralPath $outputPath -Algorithm SHA256).Hash -ne $entry.sha256) {
        throw "Copied FG asset hash mismatch: $relative"
    }
}
Copy-Item -LiteralPath $pinsPath -Destination (Join-Path $Package 'licenses/framegen-assets.json')
Write-Host 'Bundled pinned RIFE 4.26/4.6 models, ONNX Runtime and DirectML.'
