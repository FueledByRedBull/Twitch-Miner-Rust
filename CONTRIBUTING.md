# Contributing

Do not commit cookies, config files, webhooks, logs, or real Twitch payloads.
Use synthetic or redacted fixtures only.

## Make your first change

Start with a small, deterministic domain behavior. For example, the workspace
map in [`docs/architecture.md`](docs/architecture.md) assigns pure logic to
`tm-domain`; its formatting code and focused unit tests are together in
`crates/tm-domain/src/formatting.rs`.

1. Locate the implementation and its existing test from the repository root:

   ```powershell
   rg -n "format_drop_progress|progress_percent" crates/tm-domain/src
   ```

2. Make the smallest behavior change, then add or adjust a synthetic assertion
   in the existing `#[cfg(test)]` module (for example, `formats_progress`).
3. Run only that focused test:

   ```powershell
   cargo test -p tm-domain formatting::tests::formats_progress --lib --locked
   ```

4. Check formatting and lint the owning crate:

   ```powershell
   cargo fmt --all -- --check
   cargo clippy -p tm-domain --lib --all-features --locked -- -D warnings
   ```

Once the focused checks pass, run the complete gate block below. Apply the
additional protocol, architecture, or other scope-specific requirements when
your change reaches those boundaries.

## Before submitting a change

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo clippy --workspace --lib --bins --examples --all-features --locked -- -D warnings -D clippy::unwrap_used -D clippy::expect_used -D clippy::panic -D clippy::todo -D clippy::unimplemented
cargo test --workspace --all-targets --all-features --locked --quiet
$env:RUSTDOCFLAGS = '-D warnings'
cargo doc --workspace --all-features --locked --no-deps
cargo check --manifest-path fuzz/Cargo.toml --locked --all-targets
cargo build --workspace --release --locked
./scripts/verify-build-integrity.ps1
./scripts/verify-architecture.ps1
./scripts/verify-docs.ps1
./scripts/tests/verify-docs.tests.ps1
./scripts/verify-release-hygiene.ps1
./scripts/verify-go-baseline.ps1 -GoRoot <path-to-go-checkout>
```

The Go baseline gate needs Go 1.21+ and a checkout of the pinned Go baseline;
see the [parity matrix](docs/parity-matrix.md#go-baseline-check). The other
commands run from this repository alone.

The Markdown check uses tracked files; include new, unstaged documents with
`./scripts/verify-docs.ps1 -AdditionalPaths path/to/new-document.md`.

The `Deep Quality` workflow runs in required CI, on manual dispatch, and weekly.
It pins its nightly and analysis executables, preserves the 60% critical-core
branch floor and a separate 46.0% `tm-app` ratchet, and runs bounded pure-parser
fuzzing from the isolated `fuzz/` workspace. Do not expand it to network effects
or weaken the coverage floors.

## Scope-specific requirements

Protocol changes need a sanitized fixture, parser test, and parity-matrix
update. Run `crates/tm-app/tests/parser_robustness.rs` as part of the normal
suite; it is the bounded arbitrary-input regression check for protocol
parsers. Release changes need `CHANGELOG.md`, the protocol inventory, the
operations and release docs, and image-smoke updates.

Crate dependency directions are intentional. Run
`scripts/verify-architecture.ps1` after changing a workspace manifest or moving
responsibilities between crates. Update the allowlist only when the architecture
itself is deliberately changing; do not weaken it to make an accidental
dependency pass. Substantial unit-test source belongs under the owning crate's
`tests/unit/` directory and is included privately with `cfg(test)`.

Pull requests use `.github/pull_request_template.md`. Never create fixtures
from real cookies, account IDs, webhooks, logs, or request payloads. Produce
minimal synthetic JSON/text that demonstrates only the relevant contract.

## Measuring performance

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

## Security issues

Report security issues privately as described in [SECURITY.md](SECURITY.md).
