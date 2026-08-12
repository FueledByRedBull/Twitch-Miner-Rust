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

The fixed, Twitch-free replay drives the real runtime through point gains,
deduplication, prediction placement and settlement, presence reconnect storms,
campaign selection, and a 200-streamer concurrent-event burst. It contains no
account data and performs no network requests:

```powershell
cargo run -p tm-runtime --example replay_benchmark --release --locked
```

The example uses `Instant` for one wall-clock measurement and emits one compact
JSON object with the source revision, host, elapsed time, processed events,
throughput, and campaign-selection checks. The
scheduled/manual Deep Quality workflow runs this fixed workload once and stores
the JSON as review evidence; it is not a timing pass/fail gate.

Manual multiarch builds also export an ARM64 `pi-replay-benchmark` artifact
built from the exact workflow revision. Download that artifact on the release
operator host, copy only the binary to the Pi, and run it natively:

```sh
chmod 0755 ./replay_benchmark
./replay_benchmark > replay-performance-report.json
```

This keeps compilers and source trees off the production Pi while recording
measurements from Pi-class hardware. Confirm the report revision matches the
candidate image revision, archive the sanitized JSON as release evidence, and
remove the transferred binary after verification.

## Archived language comparison

The one-time Rust/Go prediction microbenchmark is no longer a build or release
gate. Its final clean checkpoint (`add8f417e4f47a44d08dbf9c90b9b995af561920`)
measured 74.10 million Rust decisions/s versus 57.03 million Go decisions/s
(29.9% Rust lead), with identical operation and full-sequence semantic
checksums. It measured a 7.001 ms Rust help-startup median and a 7,870,976-byte
stripped Rust binary versus 6.582 ms and 7,486,976 bytes for Go. These figures
are historical design evidence for a narrow CPU-bound operation, not a product
requirement or a claim that either language is universally faster.

The production ARM64 image remains a two-layer `scratch` image. The measured
Pi baseline was 3,008,991 compressed bytes, with a 6.31 MB uncompressed static
binary layer and roughly 8 KB of `/data` directory metadata. Removing the
non-root user, health command, writable-data path, or volume semantics to save
that metadata is not an acceptable optimization.

## Streamer bootstrap reads

A one-time, Twitch-free delayed-I/O harness on clean revision `8e9491b2` tested
the smallest concurrency change that preserves startup semantics: within each
still-sequential streamer, pair only the independent channel-ID and initial
channel-points-context reads. Claims, goal contributions, presence, stream
metadata, streak recovery, cache writes, and streamer order stayed sequential.
Five runs per case produced these medians:

| Streamers | Per-request delay | Sequential | Paired reads | Reduction |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 5 ms | 42.04 ms | 36.55 ms | 13.1% |
| 5 | 5 ms | 210.89 ms | 185.47 ms | 12.1% |
| 17 | 5 ms | 721.02 ms | 628.88 ms | 12.8% |
| 50 | 5 ms | 2,119.37 ms | 1,851.76 ms | 12.6% |
| 17 | 20 ms | 2,506.04 ms | 2,158.96 ms | 13.8% |
| 50 | 20 ms | 7,374.33 ms | 6,354.99 ms | 13.8% |

The harness asserted identical final streamer state and request-kind counts.
The deployed 17-streamer image took 26.4 seconds for full live bootstrap; that
includes real Twitch latency and every ordered mutation/presence step, so it is
not an apples-to-apples estimate of the isolated saving. The temporary harness
was removed instead of becoming permanent benchmark surface.

To sample resident memory and CPU for an already running local process, pass its
PID and a workload label (for example, `idle`, `normal`, or `burst`):

```powershell
./scripts/measure-performance.ps1 -ProcessId 1234 -SampleSeconds 60 -Label normal
```

During a real session, `runtime-status.json` exposes bounded measurements for
processed events and local transport-to-state latency. `--status` prints that
document without account data. Record idle, normal mining, and event-burst
samples separately; do not compare debug builds with release builds. Measure
reconnect/recovery time from the sanitized health heartbeat and reconnect
counters around a controlled network interruption.
