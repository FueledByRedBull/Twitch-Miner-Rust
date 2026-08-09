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
   above 60%, application branch coverage at or above its 46.0% ratchet, every
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

## Promotion evidence

- Exact PR head `add8f417e4f47a44d08dbf9c90b9b995af561920`
  passed CI run `30757982356` and Deep Quality run `30757980509`, including
  critical-core branch coverage 65.92%, application branch coverage 33.56%,
  every bounded mutation shard, replay regression, and both 120-second Linux
  fuzz targets.
- The preceding `db4a45a` image gate was explicitly ended early by user
  direction at `2026-08-02T19:37:00Z`, after 25:09:53 of its scheduled 72-hour
  window. It was healthy at closure, but this is a duration waiver rather than
  a 72-hour pass.
- Merge revision `23636d0d8c3b561b449eb531f26d4d2af6aac3aa`
  passed post-merge CI run `30763785031` and Multiarch Build run `30763785032`.
  The published manifest is
  `sha256:9c38b58e5f9b7ef6b5055bdaa973b94fba882b676232616fa7cfd2a8c4395e5f`.
  AMD64, ARM64, and ARMv7 child images passed workflow smoke tests, and each
  platform has both SPDX SBOM and SLSA provenance data.
- The exact manifest passed the exclusive credential-safe canary, guarded Pi
  deployment, and an exit-code-zero normal-SIGTERM recovery. At
  `2026-08-02T20:34:26Z` the recovered process reported schema 5, all ten tasks
  clean, EventSub 10/10, PubSub 53/53, and restart count zero. The immediately
  following onboarding/documentation release supersedes this candidate before
  timed acceptance, so none of its runtime is credited to the next soak.
- Runtime revision `a9817e7c4b17805dea7e86084e014e3493888be4`
  passed exact-head CI run `30767248284` and Multiarch Build run `30767248294`.
  The published manifest is
  `sha256:36b40d7bc4c46092b6a5cbb893c5d4bc9d52ec1c8cd571926d4a4c81f6242c0d`;
  independent Pi inspection confirmed AMD64, ARM64, and ARMv7 runtime
  descriptors plus per-platform SLSA provenance and SPDX documents. The later
  `e308e86` commit changes only Mermaid punctuation and was intentionally pushed
  with `[skip ci]` after GitHub's renderer was verified clean.
- The onboarding manifest passed read-only candidate and rollback config
  preflights, exact revision checks, an exclusive canary, atomic digest-only
  Compose replacement with a mode-600 backup, guarded deployment, and a normal
  SIGTERM stop with exit code zero. The final container start is
  `2026-08-02T21:51:42.429110602Z`; its fresh schema-5 session recovered all ten
  tasks, EventSub 10/10, PubSub 53/53, zero reconnects, restart count zero, and
  no active error class. Five channels were online and both first ordinary
  WATCH opportunities credited, with zero warnings/errors and exact empty
  claim/prediction parity. Cloudflare and Google HTTPS `Date` sources agreed
  within one second; the conservative non-backdated baseline is
  `2026-08-02T21:53:29Z`, making the 72-hour checkpoint
  `2026-08-05T21:53:29Z`.
- The exact non-backdated 72-hour window completed with no new session or
  shutdown marker. It recorded 1,421 ordinary WATCH rewards / 14,568 points
  against 1,406.95 availability-adjusted opportunities (101.00%), no reward
  while zero channels were online, exact claim-action/CLAIM-reward parity at
  471/471, 14 successful drop claims, and zero prediction placements/results.
  Sixteen configured channels were online during the window; fifteen earned a
  WATCH reward, while the remaining channel was selected twice and both Twitch
  playback preflights failed before its raid ended the broadcast. That is live
  selection evidence rather than fair-rotation starvation. The comparable
  `watch-request` class was 36/72 hours (0.500/h), below the preserved old-image
  observation of 48/72 hours (0.667/h); this is recorded as an observation, not
  attributed to snapshot reuse because the failures are HLS preflights.
- Cloudflare and Google HTTPS `Date` sources agreed exactly at
  `2026-08-06T16:24:32Z`, 18.517 hours after the checkpoint. The Pi clock was
  414 seconds slow, so external time proves the duration without silently
  crediting the host clock. All warning/error classes in the timed window had
  later WATCH rewards; the latest class still had 35 subsequent rewards.
- The Pi rebooted at about `2026-08-06T05:23Z`, after the completed gate. The
  same immutable image/container returned to schema 5 readiness with ten clean
  tasks, EventSub 10/10, PubSub 53/53, zero restart count, eight claimed current
  drop records, 72/72 post-boot claim parity, and 102.2% availability-adjusted
  WATCH yield. The prior-boot journal is unavailable, so the reboot is not
  claimed as graceful; the separately evidenced deployment-time normal-SIGTERM
  recovery remains the release acceptance proof.
- Final mapped cleanup removed 7,295 rebuildable local Cargo files (4.0 GiB),
  empty fuzz-artifact/release-evidence directories, and the unreferenced
  superseded Pi image `1731354e`. Reinspection proved the accepted `36b40d7b`
  image plus protected `9c38b58e` and `f1523451` rollback images remained
  available; the live service stayed ready and healthy with EventSub 10/10 and
  PubSub acknowledgements 53/53. No container, volume, runtime data, private
  configuration, cookie, streak cache, or Compose backup was removed.

## Assurance-only follow-up differential review — 2026-08-06

### Executive summary

| Severity | Findings |
| --- | ---: |
| Critical | 0 |
| High | 0 |
| Medium | 0 |
| Low | 0 |

**Overall risk:** low. **Recommendation:** approve after PR #62's final
evidence-only head repeats the required check suites. The PR check rollup is
the authoritative promotion record.

This follow-up adds a credential-free current-schema example configuration,
repairs the fresh-clone instructions, reconciles final live raid/Drop evidence,
declares Discord as the sole built-in notifier, and adds focused `tm-app` tests
for authentication fallback, task wiring, transport recovery/classification,
and shutdown containment. The Rust additions are confined to existing
`#[cfg(test)]` modules or the external app test suite; no production function,
type, dependency, manifest, Dockerfile, persisted format, external request, or
runtime state path changes.

### Scope and blast radius

- Baseline: `b33ef7a4e9ed72b440861c047cef8952d2710c76` on `main`.
- Codebase class: medium (95 checked-in Rust source/test files), using a focused
  one-hop review of every changed test module and its production subject.
- Production blast radius: zero callers and zero runtime branches changed.
- CI blast radius: the reusable Rust QA job now fails when
  `config.example.json` is invalid, stale, or requires migration. Existing
  Compose validation remains authoritative on Linux.
- Secret surface: the example contains placeholders and an empty webhook; no
  cookie, token, account identifier, private configuration, or signed URL was
  read or added.

Git history shows that the touched orchestration modules came from the earlier
architecture/runtime hardening series. This diff removes no validation,
authorization, TLS, retry, health, state, or mutation code and does not restore
a previously removed production pattern. Expected panics and unwraps occur only
inside tests covered by explicit test-module Clippy allowances.

### Test and review evidence

- The example passes both plain and JSON `--check-config`; the JSON result is
  valid on schema 1 with `migration_required=false`.
- All 114 `tm-app` unit tests and its device-flow integration test pass.
- Workspace formatting, all-target/all-feature check and tests, ordinary strict
  Clippy, production no-panic/no-unwrap/no-expect Clippy, rustdoc, documentation,
  architecture-boundary, release-hygiene, and diff checks pass locally.
- PR head `827462016bdce870b201bd0bd31b7e802b6d4eea` passed CI run
  `31122632185` attempt 2 and Deep Quality run `31122618578` attempt 2. The
  latter includes branch coverage, replay regression, both 120-second Linux
  fuzz targets, and every bounded mutation partition.
- Pinned Linux application branch coverage rose from 252/751 (33.56%) to
  295/751 (39.28%) with the denominator unchanged. The 39.0% ratchet therefore
  retains a two-branch stability margin. The focused source changes were:
  bootstrap 29/46 to 30/46, EventSub 18/64 to 25/64, PubSub 3/64 to 15/64,
  shutdown 2/8 to 6/8, and task wiring 2/6 to 5/6. The external test file is
  absent from the report, so this is exercised production branching rather
  than test-code inflation.
- Baseline `b33ef7a` and PR head `8274620`, built in one checkout with identical
  fixed revision/time metadata, disabled incrementality, reproducible linker
  flags, and one target directory, produced identical release SHA-256
  `C235D7F24A551B19A255249737DDFEBDBB2BF2ABA2EB71B698E94B49929915B2`.
  This proves the follow-up does not create a different production binary.
- The retained Pi image remains the accepted manifest `36b40d7b` at revision
  `a9817e7`, with restart count zero, schema 5, all ten tasks clean, EventSub
  10/10, PubSub 53/53, six current Drop milestones claimed, and exact recent
  claim-action/reward parity. Across 357 same-channel reward intervals, the
  median, p90, and p95 were all six minutes and 97.48% landed within six
  minutes. No deployment or new soak is warranted.

**Confidence:** high. No security finding, runtime change, public-contract
change, production-binary drift, or live-mining regression was found.
