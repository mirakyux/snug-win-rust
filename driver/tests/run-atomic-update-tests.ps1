#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$DriverSource,
    [string]$ArtifactDirectory,
    [switch]$Offline
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
if (-not $DriverSource) { $DriverSource = Join-Path $repo 'vendor/virtual-display-rs' }
$source = (Resolve-Path -LiteralPath $DriverSource).Path
$core = Join-Path $source 'driver-ipc/src/core.rs'
if (-not (Test-Path -LiteralPath $core -PathType Leaf)) {
    throw 'DriverSource must be the virtual-display-rs workspace directory'
}
if (-not $ArtifactDirectory) { $ArtifactDirectory = Join-Path $repo 'target/driver-atomic-update-tests' }
$artifacts = (New-Item -ItemType Directory -Path $ArtifactDirectory -Force).FullName
@'
[package]
name = "snug-driver-atomic-update-tests"
version = "0.0.0"
edition = "2021"

[workspace]

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[lib]
path = "harness.rs"
'@ | Set-Content -LiteralPath (Join-Path $artifacts 'Cargo.toml') -Encoding utf8NoBOM
$rustCorePath = $core.Replace('\', '/').Replace('"', '\"')
"#[path = `"$rustCorePath`"] pub mod core;" |
    Set-Content -LiteralPath (Join-Path $artifacts 'harness.rs') -Encoding utf8NoBOM
$cargoArgs = @('test', '--manifest-path', (Join-Path $artifacts 'Cargo.toml'), '--lib', 'snug_update_tests')
if ($Offline) { $cargoArgs += '--offline' }
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw "Atomic update regressions failed ($LASTEXITCODE)" }
