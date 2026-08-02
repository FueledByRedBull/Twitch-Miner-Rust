# Twitch Miner Differential Review — 2026-08-02

## Review scope

- Baseline: `db4a45abf9bfacaa46da1a8cc662027117a12790` (`main`, PR #60).
- Functional candidate: `1403265c40fd923d5abb42db6ae868362aa78787` on
  `harden/remote-endpoints-shared-ids`. This report's evidence-only follow-up
  commit changes documentation, not the measured runtime source.
- Repository scale: medium Rust workspace, 95 checked-in Rust source/test files.
- Diff at review freeze: 27 existing files changed, approximately 1,025
  insertions and 156 deletions, plus this review artifact.
- Risk level: high. The diff touches auth-session paths, outbound requests,
  Twitch-supplied URLs, prediction decision ownership, serialization support,
  release CI, and benchmark integrity.
- Reviewers: the primary agent plus independent GPT-5.6 Luna/max security,
  performance/semantic, and release-differential reviewers.

## Intended behavior

1. Reject invalid or path-unsafe Twitch usernames before any cookie read,
   directory creation, or write, while preserving the path of every accepted
   existing login and both supported JSON cookie formats.
2. Route Twitch-supplied settings, HLS, segment, and Spade endpoints through a
   persistent client that does not follow redirects or inherit proxies, rejects
   URL credentials, validates every DNS address, and pins that result for the
   connection. Explicit loopback HTTP remains available only to injected local
   test endpoints.
3. Share immutable prediction outcome IDs between parsed outcomes and owned
   actor decisions through `Arc<str>`, eliminating the per-decision allocation
   without changing exact arithmetic, actor ownership, JSON strings, or
   semantic output.
4. Expand fuzz, mutation, and branch-coverage scope to the new critical paths.

## Blast radius

| Area | Direct effect | Downstream consumers checked |
| --- | --- | --- |
| `tm-auth` | Username normalization and fallible cookie paths | app bootstrap, session load/save, device login, existing cookie fixtures |
| `tm-twitch` | Dedicated remote client, DNS policy, redirect/proxy policy, sanitized error classes | settings discovery, playback priming, Spade POST, task-health classification |
| `tm-domain` | `String` to `Arc<str>` for prediction IDs | EventSub/PubSub parsing, runtime effects, settlement, summaries, JSON, contract tests, benchmarks |
| CI/fuzz | Broader coverage and mutation targets | PR/release Deep Quality acceptance |

The internal crates are `publish = false`; therefore the Rust field-type change
is an intentional internal source API change, not a published crate API or
persisted/wire migration. Serde still emits and consumes ordinary JSON strings.
The `serde/rc` feature does not preserve allocation identity when deserializing;
identity is not part of this data contract, and a decision re-shares the parsed
outcome's allocation.

## Adversarial review and resolved findings

### Auth and configuration

- **Resolved — path traversal and cross-platform filename collisions.** The
  normalizer accepts only non-empty ASCII letters, digits, and underscores up
  to 25 characters, lowercases with ASCII semantics, rejects the built-in
  placeholder and Windows device names, and verifies one ordinary path
  component before joining `data/cookies/<username>.json`.
- **Resolved — Unicode case-folding bypass.** `AuthSession::new` no longer uses
  Unicode lowercasing that could fold a rejected character into ASCII before
  validation. A Kelvin-sign regression test covers the specific bypass.
- **Resolved — validation split-brain.** `--check-config`, config fallback, and
  startup now call the same auth username normalizer, so a config cannot pass
  the operator check and then fail at session-path setup for stricter syntax.
- **Accepted compatibility boundary.** A historical operator configuration
  using a Windows reserved device name is now rejected on every platform. That
  is deliberate containment behavior; valid Twitch logins keep the same path.

### Remote endpoints and secret hygiene

- **Resolved — DNS/redirect SSRF gap.** Every Twitch-supplied remote-document
  request uses a no-redirect/no-proxy client with a custom resolver. Every
  resolved address must be public; mixed answers fail closed, and the exact
  accepted set is returned to the connector.
- **Resolved — production loopback allowance.** Loopback HTTP is enabled only
  when code explicitly injects a loopback endpoint. Default application
  construction remains public HTTPS only.
- **Resolved — URL credential propagation.** URLs containing username or
  password userinfo are rejected before request construction.
- **Resolved — dynamic URL leakage through errors.** Playback, Spade, settings
  page, and settings script network failures retain only a typed failure class
  and fixed context; the reqwest error and full signed URL are not logged.
- **Resolved — cookie downgrade.** Default Twitch cookies are added only to an
  HTTPS Twitch origin.
- **Residual trust boundary (advisory).** Internal test/injection constructors
  that accept a caller-built reqwest client necessarily trust that caller's
  redirect policy. Production construction uses the protected builders. No
  untrusted runtime input selects those constructors.
- **Residual acceptance item.** Deterministic tests cover private, loopback,
  mixed-address, IPv4/IPv6, redirect, resolver pinning, and credential cases.
  The release canary must still prove current Twitch/CDN hostnames work under
  the public-address policy; no speculative hostname allowlist was added.

### Prediction performance and benchmark integrity

- **Resolved — repeated decision allocation.** Outcomes and owned decisions
  share the same immutable ID allocation; the actor channel remains owned and
  `Send + Sync`.
- **Resolved — benchmark checksum blind spot.** The semantic checksum consumes
  every iteration's choice, amount, and full outcome-ID bytes, so equal-length
  intermediate ID mismatches cannot escape detection.
- **Verified — no semantic drift.** Contract, parser, runtime, replay, and JSON
  tests retain the same decision output and exact `i128` amount arithmetic.
- **Clean performance evidence.** Functional revision `1403265c40fd` measured
  69.05 million Rust decisions/s versus 58.09 million Go decisions/s with
  identical full-sequence checksum `eae8f061b8e4d2d5`, a 6.937 ms median
  startup, and a 7,870,976-byte stripped Rust binary. Both source trees were
  clean.

## Verification completed on the source candidate

- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets --all-features --locked`
- `cargo test --workspace --all-targets --all-features --locked`
- workspace Clippy with `-D warnings`
- production lib/bin/example Clippy with panic/unwrap/expect/todo/unimplemented denied
- rustdoc with warnings denied
- focused app/auth/Twitch tests after final review fixes
- `cargo audit` with no advisory finding
- both fuzz targets compile under pinned nightly; native execution is not
  credited because the Windows libFuzzer binary exited with
  `STATUS_DLL_NOT_FOUND`, so the authoritative 120-second Linux runs remain the
  release gate
- documentation, release-hygiene, and architecture scripts
- Go baseline: 20 Go definitions, 19 active Rust definitions, three documented
  Go-only definitions, and two documented Rust-only definitions
- clean five-process/ten-repetition replay at `1403265c40fd`: queue depth 64,
  campaign pin retained, 200-streamer median throughput 615,440 commands/s,
  and 1,000-streamer snapshot-clone p95 605 microseconds
- two isolated reproducible release builds at `1403265c40fd` produced identical
  executable SHA-256
- independent security, performance/semantic, and differential reviews: READY

No secrets, cookie contents, account identifiers, private configuration, raw
Twitch payloads, or signed URLs were inspected or recorded during this review.

## Required immutable-revision acceptance

This source review is **conditionally approved**, not yet release-approved.
The exact PR head must still pass:

1. PR CI, dependency policy/audit, reproducibility, secret scan, docs, and
   architecture checks;
2. manual Deep Quality on pinned Linux: critical-core branch coverage at or
   above 60%, application branch coverage at or above its 46% ratchet, every
   non-empty bounded mutation shard killed, both fuzz targets for 120 seconds,
   and replay-regression comparison;
3. the existing `db4a45a` immutable-image 72-hour gate without backdating;
4. merge, verified multiarchitecture manifest/revision/SBOM/provenance, and an
   exclusive Pi canary using the exact new digest;
5. guarded deployment, normal-SIGTERM recovery, health/EventSub/PubSub,
   campaign/claim/prediction/point-acquisition evidence, rebuildable-artifact
   cleanup, and a fresh non-backdated soak baseline.

Any source change after this freeze requires rerunning the relevant review and
tests; any commit amendment after immutable-image publication invalidates the
image evidence.
