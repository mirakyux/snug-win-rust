#Requires -Version 7.0
[CmdletBinding()]
param([string]$ArtifactDirectory)
$ErrorActionPreference='Stop'
$repo=Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
. (Join-Path $PSScriptRoot 'rust-source-extraction.ps1')
if (-not $ArtifactDirectory) { $ArtifactDirectory=Join-Path $repo 'target/driver-lifecycle-tests' }
$artifacts=(New-Item -ItemType Directory -Path $ArtifactDirectory -Force).FullName
$ipc=Get-Content "$repo/vendor/virtual-display-rs/virtual-display-driver/src/ipc.rs" -Raw
$context=Get-Content "$repo/vendor/virtual-display-rs/virtual-display-driver/src/context.rs" -Raw
$callbacks=Get-Content "$repo/vendor/virtual-display-rs/virtual-display-driver/src/callbacks.rs" -Raw
$edid=Get-Content "$repo/vendor/virtual-display-rs/virtual-display-driver/src/edid.rs" -Raw
$core=Get-Content "$repo/vendor/virtual-display-rs/driver-ipc/src/core.rs" -Raw
$items=@()
$items += 'impl DeviceContext {'+(Get-RustItem $context 'pub fn create_monitor')+'}'
foreach ($head in @('struct Reservation','static MONITOR_RESERVATIONS','fn valid_owner','fn snug_check','fn expire_pending_reservations','fn snug_reserve','fn snug_create','fn snug_replace','fn snug_release','fn legacy_ids_reserved','fn monitor_state','fn mutate_monitors','fn notify','fn snug_update','fn apply_snug_update','fn depart_monitor','fn notify_locked','fn arrive_monitors','fn remove_all','fn remove','fn remove_owned','pub trait FlattenModes','pub struct ModeItem','impl FlattenModes for Vec<Mode>')) { $items += Get-RustItem $ipc $head }
foreach ($head in @('fn display_info','pub fn target_mode','pub extern "C-unwind" fn monitor_query_modes','pub extern "C-unwind" fn parse_monitor_description')) { $items += Get-RustItem $callbacks $head }
$items += '#[derive(Debug, Clone, Copy, PartialEq, Eq)]'+(Get-RustItem $core 'pub enum SnugUpdateResult')
$items += Get-RustItem $core 'pub fn validate_monitors'
$items += Get-RustItem $core 'pub fn merge_snug_update'
$template=Get-Content "$PSScriptRoot/lifecycle-harness.template.rs" -Raw
$template.Replace('// PRODUCTION_ITEMS',($items -join "`n")).Replace('// PRODUCTION_EDID',$edid).Replace('// EDID_REGRESSIONS',(Get-Content "$PSScriptRoot/edid-regressions.rs" -Raw)) | Set-Content "$artifacts/harness.rs" -Encoding utf8
@'
[package]
name = "snug-driver-lifecycle-tests"
version = "0.0.0"
edition = "2021"
[lib]
path = "harness.rs"
[dependencies]
bytemuck = { version = "=1.19.0", features = ["derive"] }
[workspace]
'@ | Set-Content "$artifacts/Cargo.toml" -Encoding utf8
foreach ($kind in @('debug','release')) {
    $extra=@()
    if($kind -eq 'release'){$extra+= '--release'}
    & cargo +nightly-2024-07-26 test --offline --manifest-path "$artifacts/Cargo.toml" --lib @extra -- --test-threads=1 --nocapture 2>&1 | Tee-Object "$artifacts/$kind-results.log"
    if($LASTEXITCODE -ne 0){throw "Driver lifecycle regressions failed: $kind"}
}
Get-FileHash -LiteralPath "$repo/vendor/virtual-display-rs/virtual-display-driver/src/ipc.rs","$repo/vendor/virtual-display-rs/virtual-display-driver/src/context.rs","$repo/vendor/virtual-display-rs/virtual-display-driver/src/callbacks.rs","$repo/vendor/virtual-display-rs/virtual-display-driver/src/edid.rs" -Algorithm SHA256 | Select-Object Path,Hash | ConvertTo-Json | Set-Content "$artifacts/source-hashes.json"
