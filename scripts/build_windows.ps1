param(
    [ValidateSet('build', 'test', 'check', 'clippy')][string]$Command = 'build',
    [switch]$Release,
    [switch]$NoDefaultFeatures
)
. (Join-Path $PSScriptRoot 'windows_build_environment.ps1')
$cargoArguments = @($Command, '--locked')
if ($Release) { $cargoArguments += '--release' }
if ($NoDefaultFeatures) { $cargoArguments += '--no-default-features' }
& cargo @cargoArguments
if ($LASTEXITCODE -ne 0) { throw "Cargo $Command failed ($LASTEXITCODE)." }
