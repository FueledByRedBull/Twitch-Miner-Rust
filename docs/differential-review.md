# Differential Review

## Executive summary

**Baseline:** `3e2a715286b2ddb8ada1bd73767870f66770fc6c`

| Severity | Count |
| --- | ---: |
| Critical | 0 |
| High | 0 |
| Medium | 0 |
| Low | 0 |

**Overall risk:** Low after local verification

**Recommendation:** Approve after the pinned Linux CI/deep-quality jobs,
immutable-image checks, and live Pi gates pass.

The change makes the Go/Rust prediction comparison materially harder to game
and defers Tokio construction until after Clap handles `--help` and `--version`.
It does not change mining algorithms, prediction policy, arithmetic, serialized
types, public APIs, transport behavior, persistence, credentials, or network
calls.

## What changed

The implementation has seven changed paths: one production entry point, one
unit-test module, two benchmark harnesses, one comparison script, one workflow,
and one performance document. This review document is updated in place rather
than adding another report.

| Area | Risk | Result |
| --- | --- | --- |
| Process startup | Medium | Clap parses before runtime creation; commands that continue construct the same enabled multi-thread Tokio runtime. |
| Benchmark harnesses | Low | Both implementations consume a complete decision, vary balances, and expose matching output and semantic checksums. |
| Comparison script | Low | Separate checked builds prevent a stale miner binary; schema and output parity are enforced. |
| CI | Low | The existing pinned Rust action and toolchain now run a bounded comparison in the Go baseline job. |
| Documentation | Low | Methodology, work differences, measurements, and rejected optimizations are explicit. |

No dependency, manifest, lockfile, unsafe block, external endpoint, secret
source, or deployment configuration changes.

## Production-path analysis

### CLI and Tokio lifecycle

The baseline `#[tokio::main]` macro created a multi-thread runtime before
entering `main`. The replacement parses `Cli` first, then calls a private
`build_runtime()` only when Clap has not already completed or rejected the
command.

The builder uses `new_multi_thread().enable_all()` and blocks on the unchanged
`run_cli(cli)` future. Therefore mining startup, immediate operator commands,
task supervision, signal handling, and graceful shutdown retain the same
runtime flavor and drivers. A focused unit test asserts
`RuntimeFlavor::MultiThread`; the full application suite exercises the
unchanged command and runtime behavior.

**Adversarial check:** Malformed arguments still fail inside Clap before
application state is loaded. Deferring the runtime does not add a fallback,
second executor, alternate configuration path, or way to bypass validation.

**Blast radius:** `main` is the sole production caller. `build_runtime` is
crate-private and has one test caller.

### Comparison integrity

The previous harness checksum covered only `amount` and used a constant
balance. The new schema cycles eight balances and validates the complete final
decision plus two checksums:

- an operation checksum consumes choice, outcome-ID length, and amount on every
  iteration;
- a stable FNV checksum consumes every choice, amount, and outcome-ID byte in a
  separate deterministic verification pass, keeping validation overhead out of
  the timed production decision;
- the report compares choice, outcome ID, amount, schema, workload, run count,
  and iteration count before it can pass.

Rust passes the complete decision to `black_box`; Go passes it to
`runtime.KeepAlive`. The comparison also discloses the remaining production
differences: Rust's size-oriented profile, exact `i128` percentage arithmetic,
and owned string output versus Go's default speed optimizer, `float64`
truncation, and shallow string-header copy.

The script now builds `tm-app` and the Rust example separately. This closes a
measurement-integrity bug where a combined Cargo invocation containing
`--example` could leave an older `tm-app` executable in place.

**Adversarial check:** A harness cannot pass with a different decision,
constant-only fixture, stale miner executable, mismatched run shape, or
amount-only checksum. Equal-length intermediate outcome IDs cannot converge to
a passing final decision because every decision contributes its bytes to the
semantic checksum. The script uses only sanitized fixtures and removes its
temporary Go source copy in `finally`.

## Historical context

Git history traces the application runtime entry point to `f8fc4cf`. The
change preserves that commit's async `run_cli` body and only replaces macro
construction with the equivalent explicit builder. No validation,
authorization, credential, privacy, retry, or mutation-safety line is removed.

The language comparison was introduced during the campaign/release work
leading to `22b8314`. Its purpose was evidence, not a public contract. Schema 2
tightens that evidence and retains the pinned Go baseline; it does not copy or
expose the Go selector.

## Test and quality evidence

Completed local evidence:

- format and `gofmt` checks;
- strict workspace/all-target/all-feature Clippy with warnings denied;
- warning-free workspace rustdoc;
- full locked workspace/all-target/all-feature tests;
- documentation, architecture, and release-hygiene gates;
- exact CI-pinned Go revision comparison with matching schema, checksums, and
  decision output;
- equivalent-output alternating 100-pair startup A/B measurement.

The startup experiment used the same executable filename and full revision
metadata. Normalized `--version` and exact `--help` output matched. Median
startup improved from 7.980 ms to 7.326 ms and p95 from 9.225 ms to 8.573 ms;
the candidate was 1,536 bytes smaller.

Windows cargo-fuzz compilation cannot supply the required sanitizer coverage
runtime (`STATUS_DLL_NOT_FOUND`, then unresolved `sancov` symbols without the
sanitizer). Existing arbitrary-byte parser tests pass locally. The pinned Linux
deep-quality job remains the authoritative fuzz execution and is a blocking
release gate, not an inferred pass.

## Remaining release gates

- Pass the committed Linux CI dependency, license, secret, coverage, docs,
  architecture, and Go/Rust comparison jobs.
- Pass the manually dispatched pinned deep-quality branch-coverage, replay,
  sanitizer-fuzz, and mutation jobs on the accepted revision.
- Verify reproducible builds and the offline source bundle from the commit.
- Build and attest the exact multi-architecture image, then verify its embedded
  revision, SBOM, provenance, smoke behavior, and size.
- Canary and deploy the immutable ARM64 digest to the Pi without building
  there, establish a fresh post-SIGTERM clock, and pass the 24-hour and 72-hour
  acceptance audits.
- Remove rebuildable artifacts and obsolete release state only after the
  72-hour gate while retaining protected rollback evidence.

## Methodology and confidence

This was a focused security differential review of every changed path against
`3e2a715`. Production changes received line-by-line before/after analysis,
history inspection, caller counting, invariant checks, adversarial scenarios,
and test review. Benchmark and workflow changes received output-integrity and
supply-chain review. Dependencies were not re-audited because manifests and
lockfiles are unchanged.

Confidence is high for the source diff and local behavior. Deployment
confidence remains conditional until the exact committed image passes CI,
deep-quality, immutable-image, canary, and fresh live-soak gates.
