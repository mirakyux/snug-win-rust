#Requires -Version 7.0
[CmdletBinding()]
param([string]$ArtifactDirectory)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
. (Join-Path $PSScriptRoot 'rust-source-extraction.ps1')
if (-not $ArtifactDirectory) { $ArtifactDirectory = Join-Path $repo 'target/driver-init-tests' }
$artifacts = (New-Item -ItemType Directory -Path $ArtifactDirectory -Force).FullName
$sourcePath = Join-Path $repo 'vendor/virtual-display-rs/virtual-display-driver/src/callbacks.rs'
$callbacks = Get-Content -LiteralPath $sourcePath -Raw
$items = foreach ($head in @('pub extern "C-unwind" fn adapter_init_finished', 'pub extern "C-unwind" fn device_d0_entry')) {
    Get-RustItem $callbacks $head
}
$template = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'init-harness.template.rs') -Raw
$template.Replace('// PRODUCTION_ITEMS', ($items -join "`n")) |
    Set-Content -LiteralPath (Join-Path $artifacts 'harness.rs') -Encoding utf8
foreach ($kind in @('debug', 'release')) {
    $flags = @()
    if ($kind -eq 'release') { $flags += '-O' }
    & rustc +nightly-2024-07-26 --edition 2021 --test -D warnings "$artifacts/harness.rs" -o "$artifacts/$kind.exe" @flags
    if ($LASTEXITCODE -ne 0) { throw "Initialization regression compilation failed: $kind" }
    & "$artifacts/$kind.exe" --test-threads=1 --nocapture 2>&1 | Tee-Object "$artifacts/$kind-results.log"
    if ($LASTEXITCODE -ne 0) { throw "Initialization regressions failed: $kind" }
}
Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256 | Select-Object Path,Hash |
    ConvertTo-Json | Set-Content -LiteralPath "$artifacts/source-hashes.json" -Encoding utf8
