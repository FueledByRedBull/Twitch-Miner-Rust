## Summary

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --locked --quiet`
- [ ] Docs/Compose checks pass when affected.
- [ ] Protocol changes include a sanitized fixture and inventory/parity update.
- [ ] No cookies, config values, webhook URLs, raw logs, or generated runtime data are included.

## Reviewer checklist

- [ ] Validation/auth/TLS behavior remains fail-closed.
- [ ] Config/cookie migrations preserve rollback safety.
- [ ] External-call retries and health behavior remain observable.
- [ ] Release/deployment changes use immutable image digests.
