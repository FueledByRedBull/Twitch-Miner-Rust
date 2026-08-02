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
startup, and runs the complete production `MOST_VOTED` prediction decision
with the same varying sanitized balances and iteration count:

```powershell
./scripts/measure-language-comparison.ps1 `
  -GoRoot C:/path/to/pinned-go-checkout `
  -OutputPath ./target/language-comparison.json
```

Schema 3 consumes the complete returned decision in both harnesses on every
iteration. It verifies choice, outcome ID, amount, an operation checksum, and a
stable semantic checksum over every decision in a separate deterministic pass
before accepting a report. The separate pass keeps byte-by-byte validation out
of the timed production decision while ensuring that an equal-length
intermediate outcome-ID mismatch cannot converge to a passing final result.
Balances cycle from 123,456 through 123,463 so the timed loop is not one
constant result. The report also records the important remaining work
differences:

- Rust uses the shipped size-oriented `opt-level = "z"` plus LTO profile; Go
  uses its default speed optimizer and stripped symbols.
- Rust computes the percentage with exact overflow-safe `i128` arithmetic; Go
  multiplies through `float64` and truncates.
- Rust decisions remain owned and self-contained while sharing immutable
  outcome-ID allocations through `Arc<str>`; Go copies immutable string
  headers. Rust therefore pays atomic reference-count operations instead of a
  fresh allocation and byte copy for each decision.

There is intentionally no synthetic "allocation-free kernel" score. The Go
selector is private; duplicating it in a benchmark or adding a production test
hook would stop measuring the shipped implementations. Rust's actor, bounded
queue, snapshots, transport supervision, and live campaign/recovery behavior
also have no equivalent Go workload.

The clean pre-change 2026-07-25 Windows x64 baseline used Rust revision
`3e2a715286b2ddb8ada1bd73767870f66770fc6c` and Go revision
`940c98409e5821900752815cd9550ae5b750b597`. The stripped executables were
7,815,168 Rust bytes (7.45 MiB) and 7,433,216 Go bytes. Thirty help launches had
medians of 7.704 ms for Rust and 6.812 ms for Go. The old amount-only workload
reported 16.47 million Rust decisions/s and 56.26 million Go decisions/s.

The comparison script now builds the miner binary and benchmark example in
separate checked Cargo commands. Passing `--example` in the previous combined
command could leave an existing miner executable stale, making its startup and
size fields unreliable after source changes.

A same-host, alternating 100-pair experiment tested moving Clap parsing ahead
of Tokio runtime construction using independently built old and new binaries.
Both executables used the same filename and full revision metadata; their
normalized version and help output matched. The untouched binary measured
7.980 ms median and 9.225 ms p95; the deferred-runtime binary measured
7.326 ms median and 8.573 ms p95. The retained change improves the median by
8.2% and p95 by 7.1%, preserves the multi-thread runtime and all drivers, and
reduced this isolated executable by 1,536 bytes.

The corrected working comparison measured 16.06 million complete Rust
decisions/s and 55.38 million complete Go decisions/s with identical outputs.
A separate investigative Rust build with `opt-level = "3"` reached 17.43
million decisions/s. That small change does not explain or close the Go gap and
does not justify inflating the shipped binary. These development experiments
are design evidence, not clean release baselines.

The 2026-08-01 protocol-hardening branch was also compared with its exact
`3a9c30c` parent in separate same-host worktrees under the same Rust and Go
toolchains. The parent measured 12.50 million Rust and 46.41 million Go
decisions/s; the candidate measured 14.03 million Rust and 52.80 million Go
decisions/s. Because both languages moved with host conditions, the normalized
Rust/Go ratio changed by only -1.39%, which is noise-scale rather than a
prediction-path regression. Candidate median startup improved from 8.932 ms to
8.515 ms and the stripped binary grew by 6,144 bytes. Both trees produced the
same complete semantic checksum. The candidate report was intentionally dirty
development evidence; clean revision evidence is still required after commit.

On 2026-08-02, the clean `db4a45a` baseline measured 14.42 million Rust and
56.07 million Go decisions/s. A semantics-preserving working-tree candidate
then shared immutable outcome IDs between outcomes and owned decisions through
`Arc<str>`, removing the repeated Rust allocation and byte copy without
removing decision data or changing exact `i128` arithmetic, actor ownership, or
JSON shape. It measured 65.76 million decisions/s in the full comparison script. An
immediately adjacent ten-run sample of five million decisions per run measured
66.62 million Rust versus 50.57 million Go decisions/s with identical operation
and full-sequence semantic checksums. The stripped Rust binary grew by 3,584
bytes. After integrating the next-release security changes, a five-run sample
of five million decisions per run measured 70.71 million Rust versus 58.56
million Go decisions/s (a 20.7% Rust lead), again with identical full-sequence
semantic checksums. That integrated working tree had a 7.248 ms median startup
and a 7,868,928-byte stripped binary. These numbers establish the design
direction but remain dirty development evidence until repeated from the exact
clean revision.

The clean functional revision
`1403265c40fd923d5abb42db6ae868362aa78787` then measured 69.05 million Rust
versus 58.09 million Go decisions/s across five runs of five million decisions
(an 18.9% Rust lead). Both implementations produced operation checksum
`216168750000`, semantic checksum `eae8f061b8e4d2d5`, and the same final
decision. Rust's 30-run median help startup was 6.937 ms and its stripped
binary was 7,870,976 bytes; Go measured 6.549 ms and 7,486,976 bytes. The Rust
worktree and Go `819a2354ccf4a24d648f5c915a44ee68bd46aaaf` checkout were both
clean. This is the release's clean functional evidence; the following
documentation-only evidence commit does not change the measured code path.

Prediction decisions occur once per event, so either implementation remains
millions of times beyond runtime demand. A 40-50 million decisions/s headline
is not a product requirement and must not drive ownership, API, arithmetic, or
binary-profile changes. The comparison measures this narrow operation; it does
not make either language universally faster or measure network-bound mining
throughput.

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
