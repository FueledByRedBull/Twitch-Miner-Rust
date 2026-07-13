# Performance Measurement

Performance changes are evidence-driven. Build the release binary first, then
run the repeatable startup smoke measurement:

```powershell
cargo build --workspace --release --locked
./scripts/measure-performance.ps1 -OutputPath ./performance-report.json
```

The report records the binary version and size, base revision, dirty-worktree
state, host architecture, Rust version, logical processor count, workspace
package count, and resolved dependency-package count. A dirty report is useful
for development comparison but is not release evidence; release baselines must
come from a clean checkout of the recorded revision.

To sample resident memory and CPU for an already running local process, pass its
PID and a workload label (for example, `idle`, `normal`, or `burst`):

```powershell
./scripts/measure-performance.ps1 -ProcessId 1234 -SampleSeconds 60 -Label normal
```

During a real session, `runtime-status.json` exposes bounded measurements for
processed events, maximum runtime queue depth, command wait time, and local
transport-to-state latency. `--status` prints that document without account
data. Record idle, normal mining, and event-burst samples separately; do not
compare debug builds with release builds. Measure reconnect/recovery time from
the sanitized health heartbeat and reconnect counters around a controlled
network interruption.

When Go 1.21+ is available, run the same normalized fixture/workload against
the adjacent Go baseline and record both revisions. A missing Go toolchain is a
measurement limitation, not evidence that Rust is faster.
