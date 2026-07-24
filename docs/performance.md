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

## Sanitized replay

The Twitch-free replay drives the real actor and reducers through point gains,
deduplication, prediction placement and settlement, presence reconnect storms,
campaign selection, scheduling, and a full 64-command queue. It contains no
account data and performs no network requests:

```powershell
cargo run -p tm-integration-tests --example replay_benchmark --release --locked
./scripts/measure-replay.ps1 -Iterations 5 -OutputPath ./replay-performance-report.json
```

The example reports p50/p95/p99 end-to-end command latency, mean actor queue
wait, maximum queue depth, throughput, post-burst snapshot responsiveness, and
snapshot-clone latency at 17, 100, and 1,000 streamers. The wrapper additionally
records whole-process CPU time and peak resident memory. It deliberately does
not install an allocator shim: allocation instrumentation is deferred unless
profiles first show a material snapshot cost. Replay timing is uploaded as CI
evidence but has no wall-clock pass/fail threshold.

On the initial Windows x64 development run for this hardening branch, snapshot
p95 was 53 microseconds at 17 streamers, 120 microseconds at 100, and 736
microseconds at 1,000; the 1,000-streamer p99 was 787 microseconds. Those
measurements do not justify replacing the simple full-state snapshot design.

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
