# Contributing

Do not commit cookies, config files, webhooks, logs, or real Twitch payloads.
Use synthetic or redacted fixtures only.

Before submitting a change, run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --quiet
cargo build --workspace --release --locked
./scripts/verify-go-baseline.ps1 -GoRoot ../Twitch-Channel-Points-Miner
```

Protocol changes need a sanitized fixture, parser test, and parity-matrix
update. Run `tests/contract/tests/parser_robustness.rs` as part of the normal
suite; it is the bounded arbitrary-input regression check for protocol
parsers. Release changes need `CHANGELOG.md`, the protocol inventory,
container/release docs, and image-smoke updates.

Pull requests use `.github/pull_request_template.md`. Never create fixtures
from real cookies, account IDs, webhooks, logs, or request payloads. Produce
minimal synthetic JSON/text that demonstrates only the relevant contract.

Security issues should be reported privately as described in `SECURITY.md`.
Include the revision or image digest and a sanitized `--support-bundle` result
if useful; do not attach runtime data. Maintainers should acknowledge a report,
reproduce it with synthetic data, prepare a fix and release/rollback plan, and
publish an advisory only after affected users have a safe update path.
