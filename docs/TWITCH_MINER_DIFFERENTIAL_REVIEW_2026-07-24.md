# Differential Review: Code Quality And Reproducibility Candidate

## Executive summary

| Severity | Open findings |
|---|---:|
| Critical | 0 |
| High | 0 |
| Medium | 0 |
| Low | 0 |

**Overall risk:** Medium operational risk because the candidate refactors the
runtime actor and Twitch transport orchestration, but low residual code risk
after local verification.

**Recommendation:** Conditional approval. Merge only after the normal PR checks
and the manually dispatched Deep Quality workflow pass. Deploy only by the
resulting immutable multiarch digest and complete the documented read-only
canary and Pi acceptance gates.

The review covered all 51 implementation files in the pre-report working tree
(3,934 additions and 953 deletions, including the isolated fuzz lockfile).
All high-risk state, prediction, authentication-adjacent, transport, and
external-effect changes received focused line-by-line review. No validation,
authorization, deduplication, single-attempt mutation, TLS, or secret-redaction
control was removed.

## What changed

**Baseline:** `2d0e380da4a853ab7cec198fc6397e17cc3db70f`

The candidate:

- separates cohesive actor, EventSub, PubSub, runtime-effect, minute-watcher,
  startup, canary, CLI, observability, and streak-recovery lifecycles without
  changing public configuration or Twitch behavior;
- forbids unsafe production code and adds a production-only Clippy gate against
  panic shortcuts;
- adds bounded balance, replay-deduplication, retry, and prediction-settlement
  properties plus isolated parser fuzzing and focused mutation/branch-coverage
  workflows;
- adds a Twitch-free replay workload and measures snapshot cloning at 17, 100,
  and 1,000 streamers;
- pins build inputs and analysis tools, verifies clean-build executable
  reproducibility, and provides a locked offline-source bundle procedure.

Two defects discovered during hardening are fixed and covered:

1. extreme point/history and prediction-pool arithmetic could overflow in
   debug or adversarial inputs; affected accumulation now saturates or widens to
   `i128`;
2. Windows/MSVC embedded volatile linker metadata, so isolated optimized builds
   did not hash equally; path remapping plus `/Brepro` now produces identical
   executables.

## Risk and blast-radius analysis

| Area | Risk | References | Review result |
|---|---|---:|---|
| `RuntimeActor` command dispatch | High | All runtime commands | Same single receiver and state owner; command order, replies, notifications, and shutdown break are preserved. |
| `apply_event_with_outcome` reducer | High | 18 textual references | No event branch or mutation-ID guard changed; the exhaustive single-writer reducer remains intact. |
| Runtime effects and prediction placement | High | 8 effect-dispatch references | Context ownership replaces repeated arguments; Twitch mutations remain single-attempt and state recording still follows successful placement only. |
| EventSub/PubSub orchestration | High | 3 spawn references each | Stop, drain, reconnect, health, fallback, and effect order are preserved. Authorization and viewer-compatibility source policy are unchanged. |
| Minute watcher and startup | Medium | 2 bootstrap references | Snapshot, metadata refresh, campaign selection, request timeout, interval, presence, and streak reconciliation order are preserved. |
| Point/history arithmetic | Medium | 14/6 references | Saturation prevents externally driven overflow without changing ordinary values. |
| Prediction payout arithmetic | Medium | 2 references | Wider/saturating intermediate arithmetic preserves normal rounding and caps unrepresentable payouts at `i64::MAX`. |
| Build/release scripts | Medium | Release pipeline | Staging and cleanup paths are resolved beneath `target/`; immutable inputs and exact hashes fail closed. |

Key reviewed boundaries include
`crates/tm-runtime/src/actor.rs:160`,
`crates/tm-runtime/src/state.rs:174`,
`crates/tm-app/src/runtime_effects.rs:17`,
`crates/tm-app/src/eventsub.rs:17`,
`crates/tm-app/src/minute_watcher.rs:22`, and
`scripts/verify-build-integrity.ps1:49`.

## Adversarial analysis

### Malicious or malformed Twitch event

An authenticated upstream connection can deliver duplicated, reordered, or
extreme point/prediction fields. The candidate still routes events through the
actor, deduplicates external IDs before returning effects, never retries a
points-changing mutation, and avoids arithmetic panics. A duplicate point event
is suppressed only while its post-application balance still matches; an equal
legitimate event after a stake or other balance movement is applied.

### Transport disconnect or task failure

A network peer can close EventSub/PubSub, withhold keepalives, or return a
revocation/error. The refactor retains bounded backoff, stop cancellation,
health failure classes, EventSub fallback presence, and PubSub topic
supervision. Reconnect loops do not replay prior runtime effects.

### Prediction mutation uncertainty

Twitch can accept a bet and drop the response. The candidate retains the
existing fail-closed single-attempt behavior: placement is invoked once, local
stake recording occurs only after a confirmed success, and an error stops
tracking instead of scheduling another mutation. No new reconciliation
fallback was added.

### Secret and artifact exposure

Fuzz corpora and replay fixtures are synthetic. The offline bundle is built
from `git archive` plus locked registry sources, so it excludes runtime config,
cookies, logs, and uncommitted files. Build and benchmark artifacts remain
under ignored target directories and are removed before publication.

## Historical context

Git history shows the point reducer originated in `8341456` and its
state-aware replay-key behavior was added by `7b22ffe`. This candidate preserves
that dedupe design and only replaces overflow-prone arithmetic. Prediction and
transport refactors retain fixes from the recent pending-settlement, outage,
read-only-retry, all-channel-rotation, and campaign-pinning commits.

No removed line traced to a security/CVE fix without an equivalent moved
implementation. The large deletion sets in EventSub, actor, watcher, startup,
and runtime effects are ownership-preserving moves rather than removed checks.

## Test and verification evidence

- `cargo fmt --all -- --check`
- strict workspace/all-target/all-feature Clippy
- production-only `unwrap`/`expect`/panic/`todo!`/`unimplemented!` rejection
- warning-free workspace rustdoc
- 344 workspace/all-target/all-feature tests, zero failures
- isolated locked fuzz-workspace compile
- root and fuzz lockfiles: 1,169-advisory RustSec database, zero findings
- docs and release-hygiene scripts
- deterministic sanitized replay at 1/10/50/200 streamers
- two isolated optimized builds with identical SHA-256
  `7dc0d8341dc889e670b443bd0a7a94120f918662011e270fde8a92d2ed1cc561`

The dependency duplicate tree contains expected transitive version splits
(`getrandom`, `webpki-roots`, and Windows support crates); most older Windows
variants are pulled only through development `tempfile`. No direct duplicate
or unused dependency change was introduced by this candidate.

## Remaining gates

The local review cannot substitute for:

- GitHub-hosted branch coverage, bounded fuzzing, and mutation execution;
- Linux multiarch Docker builds and registry attestations;
- read-only live Twitch canary against the exact published digest;
- ARM64 Pi deployment, runtime acceptance, and non-backdated soak evidence.

These are release gates, not unresolved code findings.

## Methodology and confidence

**Strategy:** Focused review for a medium Rust codebase (83 Rust files), with
100% review of changed files and deep review of high-risk state, external-call,
and value-transfer paths.

The review compared the working tree with the baseline, inspected relevant git
history and blame, counted textual call references, checked removed validation
patterns, traced state/effect ordering, modeled malformed input and outage
scenarios, and ran the verification ladder above.

**Confidence:** High for behavior preservation and local code safety; medium
for live Twitch compatibility until the exact image completes canary and Pi
acceptance.
