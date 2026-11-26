#Requires -Version 7.0
[CmdletBinding()]
param([string]$ArtifactDirectory)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
. (Join-Path $PSScriptRoot 'rust-source-extraction.ps1')
if (-not $ArtifactDirectory) { $ArtifactDirectory = Join-Path $repo 'target/driver-input-tests' }
$artifacts = (New-Item -ItemType Directory -Path $ArtifactDirectory -Force).FullName
$ipcPath = Join-Path $repo 'vendor/virtual-display-rs/virtual-display-driver/src/ipc.rs'
$entryPath = Join-Path $repo 'vendor/virtual-display-rs/virtual-display-driver/src/entry.rs'
$ipc = Get-Content -LiteralPath $ipcPath -Raw
$entry = Get-Content -LiteralPath $entryPath -Raw
$items = @()
$items += Get-RustItem $ipc 'const EOF'
$items += Get-RustItem $ipc 'const MAX_MESSAGE_SIZE'
$items += Get-RustItem $ipc 'fn buffer_message'
$retry = (Get-RustItem $entry 'std::thread::spawn(move ||') + ');'
$template = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'input-harness.template.rs') -Raw
$template.Replace('// PRODUCTION_ITEMS', ($items -join "`n")).Replace('// RETRY_BODY', $retry) |
    Set-Content -LiteralPath "$artifacts/harness.rs" -Encoding utf8
foreach ($kind in @('debug', 'release')) {
    $flags = @()
    if ($kind -eq 'release') { $flags += '-O' }
    & rustc +nightly-2024-07-26 --edition 2021 --test -D warnings "$artifacts/harness.rs" -o "$artifacts/$kind.exe" @flags
    if ($LASTEXITCODE -ne 0) { throw "Input regression compilation failed: $kind" }
    & "$artifacts/$kind.exe" --test-threads=1 --nocapture 2>&1 | Tee-Object "$artifacts/$kind-results.log"
    if ($LASTEXITCODE -ne 0) { throw "Input regressions failed: $kind" }
}
Get-FileHash -LiteralPath $ipcPath, $entryPath -Algorithm SHA256 | Select-Object Path,Hash |
    ConvertTo-Json | Set-Content -LiteralPath "$artifacts/source-hashes.json" -Encoding utf8
