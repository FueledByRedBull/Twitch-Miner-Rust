# Contributing

Do not commit cookies, config files, webhooks, logs, or real Twitch payloads.
Use synthetic or redacted fixtures only.

Before submitting a change, run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo clippy --workspace --exclude tm-contract-tests --exclude tm-integration-tests --lib --bins --examples --all-features --locked -- -D warnings -D clippy::unwrap_used -D clippy::expect_used -D clippy::panic -D clippy::todo -D clippy::unimplemented
cargo test --workspace --all-targets --all-features --locked --quiet
$env:RUSTDOCFLAGS = '-D warnings'
cargo doc --workspace --all-features --locked --no-deps
cargo check --manifest-path fuzz/Cargo.toml --locked --all-targets
cargo build --workspace --release --locked
./scripts/verify-build-integrity.ps1
./scripts/verify-architecture.ps1
./scripts/verify-release-hygiene.ps1
./scripts/verify-go-baseline.ps1 -GoRoot ../Twitch-Channel-Points-Miner
```

Performance-sensitive changes should also run the sanitized, network-free
replay. Timing is review evidence, not a wall-clock pass/fail gate:

```powershell
./scripts/measure-replay.ps1 -Iterations 5
```

Changes shared with the Go reference should use an explicit clean Go checkout
and retain the generated report only as review/release evidence:

```powershell
./scripts/measure-language-comparison.ps1 `
  -GoRoot C:/path/to/pinned-go-checkout
```

The manually dispatched/weekly `Deep Quality` workflow pins its nightly and
analysis executables. It preserves the 60% critical-core branch floor and a
separate 46% `tm-app` ratchet, runs bounded pure-parser fuzzing from the isolated
`fuzz/` workspace, and mutates only the functions named in the workflow. Do not
expand it to network effects or treat timing as a correctness gate.

Protocol changes need a sanitized fixture, parser test, and parity-matrix
update. Run `tests/contract/tests/parser_robustness.rs` as part of the normal
suite; it is the bounded arbitrary-input regression check for protocol
parsers. Release changes need `CHANGELOG.md`, the protocol inventory,
container/release docs, and image-smoke updates.

Crate dependency directions are intentional. Run
`scripts/verify-architecture.ps1` after changing a workspace manifest or moving
responsibilities between crates. Update the allowlist only when the architecture
itself is deliberately changing; do not weaken it to make an accidental
dependency pass. Substantial unit-test source belongs under the owning crate's
`tests/unit/` directory and is included privately with `cfg(test)`.

Pull requests use `.github/pull_request_template.md`. Never create fixtures
from real cookies, account IDs, webhooks, logs, or request payloads. Produce
minimal synthetic JSON/text that demonstrates only the relevant contract.

Security issues should be reported privately as described in `SECURITY.md`.
Include the revision or image digest and a sanitized `--support-bundle` result
if useful; do not attach runtime data. Maintainers should acknowledge a report,
reproduce it with synthetic data, prepare a fix and release/rollback plan, and
publish an advisory only after affected users have a safe update path.
