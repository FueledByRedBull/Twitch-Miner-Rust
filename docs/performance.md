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

## Container image footprint

The production ARM64 image remains a two-layer `scratch` image. The measured
ARM64 deployment baseline was 3,008,991 compressed bytes, with a 6.31 MB
uncompressed static binary layer and roughly 8 KB of `/data` directory
metadata. Removing the
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
