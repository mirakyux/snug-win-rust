# Reproduce the old Rust ICO decode/resize, or verify every generated pixel:
#   pwsh -File tools/generate_tray_icons.ps1
#   pwsh -File tools/generate_tray_icons.ps1 -Check
# The standalone build stays under target and adds no workspace dependency.
param([switch]$Check)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$lockPath = Join-Path $root 'Cargo.lock'
$lock = Get-Content -LiteralPath $lockPath -Raw
$match = [regex]::Match($lock, '(?m)^name = "image"\r?\nversion = "([^"]+)"')
if (-not $match.Success) { throw 'Cannot find image in the workspace Cargo.lock' }
$imageVersion = $match.Groups[1].Value

$toolRoot = Join-Path $root 'target/tray-icon-generator'
New-Item -ItemType Directory -Force -Path $toolRoot | Out-Null
$source = (Join-Path $PSScriptRoot 'generate_tray_icons.rs').Replace('\', '/')
$sourceToml = ConvertTo-Json -InputObject $source -Compress
$manifest = @"
[workspace]
[package]
name = "snug-tray-icon-generator"
version = "0.0.0"
edition = "2024"
[[bin]]
name = "generate-tray-icons"
path = $sourceToml
[dependencies]
image = { version = "=$imageVersion", default-features = false, features = ["png", "ico"] }
"@
$manifestPath = Join-Path $toolRoot 'Cargo.toml'
Set-Content -LiteralPath $manifestPath -Value $manifest -Encoding utf8
# Reuse the locked transitive versions, updating only this throwaway lockfile.
Copy-Item -LiteralPath $lockPath -Destination (Join-Path $toolRoot 'Cargo.lock') -Force
$cargoArgs = @(
    'run', '--quiet', '--manifest-path', $manifestPath,
    '--target-dir', (Join-Path $toolRoot 'build'), '--', $root
)
if ($Check) { $cargoArgs += '--check' }
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw "Icon generator failed with exit code $LASTEXITCODE" }
