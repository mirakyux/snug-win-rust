# Driver resource lifetime regressions

Run from the repository root on Windows with PowerShell 7, the MSVC linker,
and the pinned Rust toolchain installed:

```powershell
pwsh -File driver/tests/run-resource-regressions.ps1
```

The runner needs no new Cargo dependencies, installed display driver, driver
signing certificate, administrator rights, or display configuration changes.
It writes generated test sources, executables, logs, and production source
hashes under `target/driver-resource-tests/`. Use `-ArtifactDirectory` to choose
a different output directory.

The tests exercise actual production resource-management code:

- The cursor harness compiles the current monitor context, zero-initialized
  storage, callback registration, and cursor setup methods from `context.rs`.
  Its small Rust lexer locates declaration and delimiter boundaries while
  ignoring strings, raw strings, character literals, and nested comments. It
  does not assert that particular source strings or implementation patterns
  exist. The runner also parses and checks the complete production files with
  the pinned `rustfmt` before extracting any items.
- Cursor events are real process-local Win32 event handles. The IddCx setup and
  WDF context lookup functions are fakes. `GetHandleInformation` verifies that
  failed setup and context cleanup release the handles, while repeated setup
  retains the existing event. Tests cover cleanup before initialization,
  repeated cleanup, initialization/access/cleanup, existing Arc owners,
  rejected reinitialization, and concurrent context access versus cleanup.
- The swapchain harness directly includes the complete production
  `swap_chain_processor.rs`. Its fake native APIs force MMCSS registration to
  fail or succeed and end frame acquisition with an error. Counters verify
  that frame processing still runs, the assigned swapchain is deleted once,
  and successful MMCSS registration is reverted.

These are ownership and failure-path tests. They do not validate the real WDK
ABI, compositor output, GPU behavior, driver installation, or phone streaming.
Run the separate driver build/ABI checks and hardware acceptance for those.

## Monitor lifecycle and timing regressions

```powershell
pwsh -File driver/tests/run-lifecycle-regressions.ps1
pwsh -File driver/tests/run-atomic-update-tests.ps1 -Offline
pwsh -File driver/tests/run-init-regressions.ps1
```

The lifecycle runner extracts the current production monitor creation, mutation,
departure, arrival, shared input validation, and mode timing functions. It uses
the same Rust item lexer as the resource runner. Both Debug and Release tests
inject native arrival, context initialization, and departure failures, verify
object deletion and retries, preserve failed removals in State, and compare
Changed snapshots with actual state for partial and concurrent operations.
The timing cases include a portrait mode and 7680 × 4320 at 240 Hz. Native WDF
functions are fakes; these tests do not install a driver or alter displays.

The same lifecycle harness extracts the owned reservation/create/replace/release
functions. It checks lost replies, late creation, foreign and reused IDs, stale
reservations, lease expiry, and token retention through native failures. Empty
reservations expire; accepted monitor ownership does not. NULL mode-buffer queries
and initialization failure propagation have separate regressions in the lifecycle
and initialization runners.

For driver `0.4.0.9`, the lifecycle harness also exercises all four production
tokenless write helpers: Notify, Remove, RemoveAll, and SnugUpdate. Mixed batches
that touch an owned or reserved connector are rejected before any native call.
Snapshots compare monitor data, reservation tokens and deadlines, native object
identities, arrivals, departures, live objects, deletions, and Changed events.
The cases cover empty reservations, unchanged owned rows with pending arrivals,
an owner accepted while a legacy request waits for the mutation lock, unrelated
legacy incremental writes, and clearing then restoring an unowned State. Legacy
requests do not expire reservations; only the owner protocol retains that role.
The current Debug and Release suites each contain 45 passing tests.

The atomic runner directly includes the production protocol module to check
serialization, conflict handling, shared hardware and resource limits, and
batch validation before mutation. Its Cargo outputs use an isolated target
directory under `target/driver-atomic-update-tests/`.

## Pipe input and optional logging regressions

```powershell
pwsh -File driver/tests/run-input-regressions.ps1
```

The input runner extracts the production frame buffer function, payload limit,
delimiter, and complete logging retry thread body. Debug and Release cases cover
unterminated and completed oversized frames, exact-limit frames, coalesced frames
and partial tails, and the maximum current monitor request. Only newly read bytes
are scanned; the limit applies separately to each frame. A fake clock and thread
runner exercise logging timeout and success at the deadline without waiting five
minutes. A fake WDF failure function detects unwanted device failure calls. No
actual pipe, driver, WDF API, or display configuration is used. Results and source
hashes are written under `target/driver-input-tests/`.
