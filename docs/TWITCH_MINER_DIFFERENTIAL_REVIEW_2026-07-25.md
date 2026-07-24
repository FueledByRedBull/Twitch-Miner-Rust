# Twitch Miner Rust Differential Review

**Review date:** 2026-07-25
**Baseline:** `2d0e380da4a8` (`Pin active drop campaigns (#31)`)
**Reviewed revision:** `70922da70c34` (`Harden runtime and reproducible release pipeline`)
**Branch:** `hardening/code-quality-repro-performance`
**Worktree before report:** clean
**Scope:** 52 changed files, 4,328 insertions, 954 deletions

## Executive summary

Revision `70922da` is a material improvement over `2d0e380`. The runtime actor
and application loops are decomposed without changing public interfaces, integer
overflow paths now saturate or widen safely, production panic shortcuts and
unsafe Rust are rejected, release builds are checked for byte identity, and an
exact locked revision can be bundled for offline recovery.

No exploitable security regression or functional blocker was found. Two
actionable engineering defects remain:

- **Low:** `-ValidateOnly` checks its output-path restriction only after writing
  the archive and checksum, so an invalid path can still overwrite files before
  the script rejects it.
- **Low:** replay repetitions are executed but discarded, and the wrapper keeps
  only the final replay report. This weakens statistical evidence and makes the
  `repetitions` field easy to overinterpret.

The new branch-coverage, fuzz, and mutation workflow is valuable, but it has not
yet run for this revision. Branch coverage is recorded rather than enforced, and
the replay benchmark is uploaded without a regression threshold.

## Scorecard

| Area | Score | Assessment |
|---|---:|---|
| Code quality | **9.2/10** | Strict formatting, pedantic Clippy, production panic rejection, forbidden unsafe code, rustdoc-as-error, safer arithmetic, and 344 passing tests. |
| Maintainability | **9.0/10** | Long application loops and the runtime actor were decomposed into focused contexts and handlers; architecture and operational documentation improved. Some very large modules remain. |
| Dependency reproducibility | **9.6/10** | Locked graph, pinned toolchain/actions/tools/base images, Debian snapshot, two-build byte comparison, embedded revision metadata, and a validated offline source bundle. |
| Performance engineering | **8.8/10** | Real actor/reducer replay, queue saturation, latency percentiles, throughput, snapshot scaling, CPU/RSS wrapper, and runtime metrics now exist. Aggregation and regression gates remain incomplete. |

## Findings

### [LOW] Validate-only output restriction occurs after destructive writes

**Location:** `scripts/create-offline-source-bundle.ps1:75-90`

The script resolves and creates the requested output directory, invokes
`tar -czf`, and writes the checksum before checking that `-ValidateOnly` output
is under `target/`. A maintainer who supplies `-ValidateOnly` with an invalid
existing path can have that path overwritten before receiving the intended
error.

**Impact:** Local file overwrite under the invoking maintainer's permissions.
This is not remotely exploitable and requires an explicit maintenance command,
so severity is low.

**Recommendation:** Resolve and validate the `-ValidateOnly` output path before
creating its directory or invoking `tar`.

### [LOW] Replay repetitions do not contribute to reported latency distributions

**Locations:**

- `tests/integration/examples/replay_benchmark.rs:286-301`
- `scripts/measure-replay.ps1:41-63`
- `scripts/measure-replay.ps1:92`

`benchmark_report` clears `workloads` and `snapshots` at the beginning of every
repetition. The JSON therefore contains only four workload rows and three
snapshot rows from the final repetition, even when `repetitions` is greater than
one. The PowerShell wrapper repeats the process but assigns each result to
`$lastReplay`, so only the final process's final repetition is preserved.

**Impact:** CPU, wall time, and peak RSS are aggregated across processes, but the
reported p50/p95/p99 actor and snapshot numbers are not. This is not a runtime
miner defect; it limits the reliability of performance regression decisions.

**Recommendation:** Retain raw samples across repetitions or aggregate each
workload key across runs, then compare stable metrics against a versioned
baseline with a noise-tolerant threshold.

## Quality and maintainability analysis

- The production workspace now forbids unsafe Rust.
- CI rejects `unwrap`, `expect`, `panic`, `todo`, and `unimplemented` in
  production libraries, binaries, and examples.
- The runtime actor's monolithic command loop was split into a stateful actor
  with command-specific handlers. Its crate-private interface preserves the
  prior public boundary.
- Application EventSub, PubSub, minute-watcher, startup, prediction-effect,
  canary, and streak-recovery paths use explicit context/state structures rather
  than growing positional argument lists.
- Point balances and history accounting use saturating arithmetic.
- Prediction pool arithmetic widens to `i128`, saturates multiplication and
  addition, and is covered by extreme-value tests.
- Twitch pagination now returns a typed error instead of relying on an internal
  `expect`.
- The number of explicit Clippy allowances remains 20 rather than increasing,
  and the central exhaustive reducer's size allowance is documented.
- Twenty production/test Rust modules remain at or above 500 lines. The largest
  production modules include EventSub parsing, Twitch client/contracts, runtime
  state, configuration, and observability. Their cohesive domain boundaries
  make this acceptable, but they are the main reason maintainability is not
  scored above 9.0.

No validation-like removal yielded an exploitable path. The apparent removals
in the diff are moves into focused helper methods; the corresponding error,
retry, cancellation, authorization, and deduplication decisions remain.

## Dependency and release reproducibility

- The workspace resolves **18 unique direct external crates** and 245 resolved
  non-workspace packages. This is unchanged by the reviewed commit.
- `Cargo.lock` remains committed and all local Cargo validation used
  `--locked --offline`.
- The Rust toolchain, GitHub Actions, QA tool versions, Docker frontend, Rust
  builder image, Debian snapshot, and system package version are pinned.
- `scripts/verify-build-integrity.ps1` fixes revision/time metadata, disables
  incremental compilation, remaps paths, enables deterministic MSVC linking,
  performs two isolated release builds, and requires matching binary hashes.
- Local verification produced identical binary SHA-256:
  `90C1A7080665397F397AB224BECA820CAD14D4305380E53B79ACCF5B4541129B`.
- The offline source bundle validated successfully for the exact 40-character
  revision, including locked vendoring and offline Cargo metadata resolution.

The remaining reproducibility limitation is scope: local byte identity was
proved for Windows/MSVC. The three Linux container platforms were still
building when this report was written, and the workflow does not build each
container platform twice to assert byte-identical image manifests.

## Performance evidence

The release replay directly exercises the real actor and reducers without Twitch
network traffic. A local three-repetition invocation reported:

| Streamers | Throughput | p50 | p95 | p99 | Max queue |
|---:|---:|---:|---:|---:|---:|
| 1 | 638,166 commands/s | 62 us | 115 us | 147 us | 64 |
| 10 | 342,421 commands/s | 57 us | 136 us | 141 us | 64 |
| 50 | 559,300 commands/s | 180 us | 464 us | 481 us | 64 |
| 200 | 544,182 commands/s | 2,404 us | 4,116 us | 4,239 us | 64 |

Snapshot clone latency:

| Streamers | p50 | p95 | p99 |
|---:|---:|---:|---:|
| 17 | 17 us | 26 us | 29 us |
| 100 | 46 us | 76 us | 95 us |
| 1,000 | 440 us | 548 us | 789 us |

These figures are host-specific development evidence, not cross-machine
performance guarantees. The queue reached its configured depth of 64, and the
1,000-streamer snapshot p99 remained below 1 ms in this run.

## Security and blast-radius analysis

**High-risk surfaces reviewed:** external EventSub/PubSub messages, runtime
state mutation, prediction settlement, Twitch pagination, local release scripts,
and container publication metadata.

**Attacker model:** An unauthenticated remote party can influence Twitch-facing
transport messages only through the upstream Twitch service/network path. They
cannot invoke local release scripts. A compromised or malformed upstream
message reaches existing typed parsers and reducers.

No changed path bypasses authorization, deduplication, validation, retry bounds,
or typed error handling. The new structured-input and transport-parser fuzz
targets are appropriate defenses, though their scheduled run is not yet
evidenced.

All newly added callable Rust items are crate-private. The runtime refactor
therefore has a broad internal operational blast radius but does not enlarge the
external API surface.

## Verification performed

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked --offline -- -D warnings`
- strict production Clippy rejection of panic shortcuts
- `cargo test --workspace --locked --offline --quiet` — **344 passed**
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --locked --offline --no-deps`
- `cargo audit --no-fetch --file Cargo.lock` — no vulnerability reported from
  the cached 1,169-advisory database
- `cargo metadata --manifest-path fuzz/Cargo.toml --locked --offline --no-deps`
- PowerShell parse validation for the new/changed release and replay scripts
- `scripts/verify-release-hygiene.ps1`
- `scripts/verify-build-integrity.ps1`
- `scripts/create-offline-source-bundle.ps1 -Revision 70922da... -ValidateOnly`
- release replay benchmark with `TM_REPLAY_REPETITIONS=3`
- `git diff --check 2d0e380..70922da`
- GitHub CI run `30128283448` — completed successfully

Local `cargo deny check` was not run because `cargo-deny` is not installed. The
pinned CI dependency-policy job completed as part of the successful branch CI
run. The weekly Deep Quality workflow (branch coverage, fuzzing, mutation
testing) had no completed run for this revision at review time. Multiarch image
build run `30128283306` was still in progress.

## Methodology and limitations

The review used a focused differential analysis from `2d0e380` to `70922da`,
classified changed trust boundaries, traced internal callers and state mutation
paths, searched removed validation/error-handling lines, reviewed relevant file
history, ran repository-native validation, and measured the new replay on the
local Windows x64 host.

No live Twitch account, credentials, production session, Raspberry Pi, or
cross-platform container runtime was used. No secret-bearing files were read.
Performance results should be compared only with clean runs on a controlled
host. Configured but not-yet-run fuzz, branch, mutation, and multiarch gates
remain evidence gaps rather than assumed passes.

## Remediation closure

Both low-severity findings were fixed without changing Twitch behavior, public
configuration, or the single-writer runtime model.

- `create-offline-source-bundle.ps1` now resolves and validates the
  `-ValidateOnly` output path before creating a staging directory, archive, or
  checksum. Release-hygiene verification protects a sentinel outside `target/`
  and proves that a rejected path cannot overwrite it.
- The replay example now aggregates raw latency and snapshot samples from every
  requested repetition. The PowerShell wrapper retains every child-process
  report and aggregates all process runs. Reports use schema 2, include host
  provenance, and are compared with a versioned GitHub-hosted Linux x64
  baseline using deliberately noise-tolerant thresholds.
- The wrapper's redirected output is drained asynchronously before process
  completion, eliminating a pipe-capacity deadlock found while verifying the
  aggregation fix.
- Critical branch coverage is enforced from `cargo llvm-cov` JSON rather than
  relying on an unsupported command-line flag. The measured critical-crate
  result is 476 of 754 branches (63.13%), above the honest 60% ratchet.
- Both fuzz targets pass their bounded runs. Mutation testing is split across
  runtime reducers and the PubSub, EventSub, and saved-session retry policies;
  an empty selected-mutant list is a hard failure. Three boundary assignments
  that provably write the already-equal value are documented as equivalent
  mutants rather than hidden as test misses.
- The release image remains unchanged by measurement tooling. Manual multiarch
  builds export the exact-revision ARM64 replay executable as a separate
  artifact so it can be measured natively on the Pi without building source
  there; the artifact is removed after evidence collection.

Code-equivalent remediation revision `5d42c6e7cf71` passed CI run
`30132527599` and Deep Quality run `30132527486`. The latter enforced 476/754
critical branches (63.13%), caught 22/22 selected runtime mutants and 4/4
selected retry-policy mutants, passed both bounded fuzz targets, and compared a
five-process/50-inner-run replay successfully. Its 200-streamer p95-of-p95 was
8,569 microseconds, median throughput was 254,284 commands/second,
1,000-streamer snapshot p95-of-p95 was 708 microseconds, and peak RSS p95 was
13.92 MiB.

The original findings are closed. Publication, exact-image Pi canary, and live
reward-rate acceptance remain release gates rather than differential-review
code findings.
