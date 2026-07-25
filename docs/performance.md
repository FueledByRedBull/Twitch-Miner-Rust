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
./scripts/measure-replay.ps1 -Iterations 5 -HardwareClass desktop -OutputPath ./replay-performance-report.json
```

The example aggregates raw latency and snapshot samples across every requested
repetition. The wrapper retains every process report and then reports
distributions across all process runs, so `repetitions` never describes
discarded work. Reports include p50/p95/p99 end-to-end command latency, mean
actor queue wait, maximum queue depth, throughput, post-burst snapshot
responsiveness, snapshot-clone latency at 17, 100, and 1,000 streamers,
whole-process CPU time, and peak resident memory. It deliberately does not
install an allocator shim: allocation instrumentation is deferred unless
profiles first show a material snapshot cost. Both direct and wrapped reports
record the OS, architecture, hardware-class label, and source revision; use
`-HardwareClass pi-class` for a native Pi measurement.

Manual multiarch builds also export an ARM64 `pi-replay-benchmark` artifact
built from the exact workflow revision. Download that artifact on the release
operator host, copy only the binary to the Pi, and run it natively:

```sh
chmod 0755 ./replay_benchmark
TM_REPLAY_REPETITIONS=10 \
TM_REPLAY_HARDWARE_CLASS=pi-class \
./replay_benchmark > replay-performance-report.json
```

This keeps compilers and source trees off the production Pi while recording
measurements from Pi-class hardware. Confirm the report revision matches the
candidate image revision, archive the sanitized JSON as release evidence, and
remove the transferred binary after verification.

The deterministic PR replay remains evidence-only. The scheduled/manual Deep
Quality workflow runs a longer five-process, ten-repetition sample and compares
only stable 200-streamer latency/throughput, 1,000-streamer snapshot latency,
and peak RSS against
`benchmarks/replay-baseline.github-ubuntu-x64.json`. Its 1.75x latency, 0.6x
throughput, and 1.5x RSS tolerances are deliberately noise-tolerant: they catch
material regressions without turning shared-runner timing variance into a PR
gate. Update the baseline only from an archived clean-run report, record the
source revision, and review the change like production code.

On the initial Windows x64 development run for this hardening branch, snapshot
p95 was 53 microseconds at 17 streamers, 120 microseconds at 100, and 736
microseconds at 1,000; the 1,000-streamer p99 was 787 microseconds. Those
measurements do not justify replacing the simple full-state snapshot design.

## Actor queue capacity

The actor uses a bounded queue and awaits capacity instead of dropping transport
events. Its ignored release-mode sweep is reproducible without exposing a
production tuning knob:

```powershell
cargo test -p tm-runtime --release actor_queue_capacity_sweep -- --ignored --nocapture --test-threads=1
```

Five Windows x64 runs of a 5,000-event burst across 17 channels produced these
median results:

| Capacity | Throughput | p95 latency |
| ---: | ---: | ---: |
| 32 | 656,564 commands/s | 4,151 us |
| 64 | 652,733 commands/s | 4,219 us |
| 128 | 657,990 commands/s | 4,288 us |

The differences are noise-scale. Capacity 64 remains the balanced middle: it
absorbs a larger burst than 32 without the extra queued state of 128, and
backpressure remains explicit. The mixed replay separately drives the queue to
its configured limit while exercising prediction, presence, campaign, and
snapshot work.

## Rust and Go comparison

Use an explicit clean checkout of the pinned Go reference. The script builds
both stripped release applications on the same host, measures their native help
startup, and runs the identical production `MOST_VOTED` prediction decision
with the same sanitized inputs, iteration count, and checksum:

```powershell
./scripts/measure-language-comparison.ps1 `
  -GoRoot C:/path/to/pinned-go-checkout `
  -OutputPath ./target/language-comparison.json
```

The comparison deliberately reports two categories:

- comparable: stripped binary size, CLI parse/help startup, and the normalized
  production prediction-decision kernel;
- not comparable: Rust's single-writer actor, bounded queue, state snapshots,
  transport supervision, and live campaign/recovery behavior, because the Go
  baseline has no equivalent workload.

On the initial Windows x64 run against Go revision
`91f00698314dbbdd6c757d7b525458c82173e622`, the stripped Go executable was
7,486,464 bytes and the Rust executable was 7,814,656 bytes. Thirty help
launches had medians of 6.979 ms for Go and 8.115 ms for Rust. The matching
prediction checksum ran at roughly 55-57 million decisions/s in Go and
16 million decisions/s in Rust. Prediction decisions occur once per event, so
both are far beyond runtime demand; this result does not make Go universally
faster or measure network-bound mining throughput.

The production ARM64 image remains a two-layer `scratch` image. The measured
Pi baseline was 3,008,991 compressed bytes, with a 6.31 MB uncompressed static
binary layer and roughly 8 KB of `/data` directory metadata. Removing the
non-root user, health command, writable-data path, or volume semantics to save
that metadata is not an acceptable optimization.

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
