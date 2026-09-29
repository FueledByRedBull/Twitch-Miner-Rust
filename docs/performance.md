# Performance Measurement

Performance changes are evidence-driven. Build the release binary first, then
use PowerShell's native timing when a concrete comparison is needed:

```powershell
cargo build --workspace --release --locked
Measure-Command { ./target/release/tm-app.exe --version }
```

Record the exact clean revision, binary version and size, host architecture,
Rust version, and repeated median. A dirty measurement is useful during
development but is not release evidence.

To sample an already running local process, use `Get-Process`:

```powershell
Get-Process -Id 1234 | Select-Object CPU, WorkingSet64, PeakWorkingSet64
```

During a real session, `runtime-status.json` exposes bounded measurements for
processed events and local transport-to-state latency. `--status` prints that
document without account data. Record idle, normal mining, and event-burst
samples separately; do not compare debug builds with release builds. Measure
reconnect/recovery time from the sanitized health heartbeat and reconnect
counters around a controlled network interruption.
