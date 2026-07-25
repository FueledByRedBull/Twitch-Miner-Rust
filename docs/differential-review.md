# Differential Review

## Executive summary

**Baseline:** `d2430b5e767e237c3b1462ad33385cfa80de1b11`

| Severity | Count |
| --- | ---: |
| Critical | 0 |
| High | 0 |
| Medium | 0 |
| Low | 0 |

**Overall risk:** Low after verification
**Recommendation:** Approve after the required Linux CI, image, and live Pi
acceptance gates pass.

The review found no security regression in the campaign, prediction arithmetic,
Twitch client metadata, actor queue, observability, workflow, or benchmark
changes. All 35 changed/new/deleted paths were reviewed; the high-risk external
input and value-decision paths have focused tests and low caller counts.

## What changed

The working change is approximately 971 additions and 972 deletions. Most
deletions are four obsolete dated review reports; one maintained review remains
here.

| Area | Risk | Result |
| --- | --- | --- |
| Campaign completion and watch selection | High | Fully claimed inventory campaign IDs release stale watch pins; incomplete, missing, and unknown states fail safe. |
| Prediction amount and outcome ordering | High | Floating-point value decisions are now exact integer operations with boundary coverage. |
| Twitch client metadata | High | The unused compiled client-version fallback is removed; TLS-fetched build discovery remains mandatory. |
| Runtime queue measurement | Medium | Production capacity and backpressure behavior are unchanged; measurement uses an isolated test-only constructor. |
| GitHub Actions and secret scanning | High | Updated actions are immutable Node 24-capable commits; native Gitleaks is version- and checksum-pinned. |
| Branding, docs, and performance harness | Low | Foreign presentation assets and stale internal reports are removed without removing legal attribution. |

## High-risk path analysis

### Campaign completion

**Entry and trust boundary:** Twitch GQL inventory enters
`inventory_snapshot_from_typed` in `crates/tm-twitch/src/client.rs`; available
campaign IDs enter the minute watcher separately.

**Invariants:**

- A campaign is complete only when it has an ID, at least one drop, and every
  drop explicitly reports `isClaimed=true`.
- Missing campaign IDs, missing progress, partial progress, empty campaigns,
  and transient inventory failures cannot classify a campaign as complete.
- Completion changes only local watch selection; it does not perform a claim or
  other mutation.

**Blast radius:** The new snapshot method has one production caller plus the
existing inventory compatibility facade. The completion predicate has one
production caller. This is a low caller-count, high-value behavior change.

**Adversarial scenario:** A malformed inventory response omits a drop's
`self` state or campaign ID in an attempt to release a campaign pin. The parser
does not add that campaign ID to the completed set, so the available campaign
remains eligible. A response can release a pin only by coming from the
authenticated TLS Twitch endpoint and explicitly marking every non-empty drop
claimed; that is the intended completion signal.

**Coverage:** Typed fixtures cover complete, incomplete, missing-progress, and
missing-ID campaigns. Watch-selection tests prove completed campaigns release
the pin while a new campaign remains eligible.

### Prediction value decisions

**Entry and trust boundary:** Point balances and prediction totals originate in
authenticated Twitch responses and are evaluated by
`PredictionEvent::decide`.

**Invariants:**

- Negative balances become zero.
- Percentage multiplication occurs in `i128`, then saturates only when
  narrowing to `i64`.
- The final amount remains bounded by configured maximum points and the
  non-negative current balance.
- Integer strategies compare integer counters without `f64` precision loss.

**Blast radius:** `decide` has the runtime prediction path and tests as callers;
the new arithmetic helper is private. `select_outcome` remains the same public
interface.

**Adversarial scenario:** An extreme balance or percentage attempts to overflow
the bet calculation or produce an amount above the account balance. The wide
calculation cannot overflow for `i64 * u32`; narrowing saturates and the
subsequent balance cap prevents overspend. Boundary tests include negative,
zero, `2^53+1`, `i64::MAX`, `101%`, and `u32::MAX` inputs.

### Twitch client identity and workflow supply chain

`CLIENT_ID` remains the single browser identity required across OAuth, GQL, and
EventSub. `Client-Version` continues to be extracted from Twitch over TLS before
the first GQL request and cached for ten hours; deleting the never-used compiled
default does not introduce a fallback or alternate credential path.

The secret-scanning workflow downloads one named Gitleaks archive and verifies
its fixed SHA-256 before execution. Updated third-party actions are pinned to
full commits, and their action manifests declare the Node 24 runtime.

## Historical context

Git pickaxe and blame traced the original floating-point prediction arithmetic
to `f8fc4cf` and the typed inventory parser and client-version cache to
`8341456`/`92db087`. No removed line originated in a security, CVE, authorization,
retry, mutation-idempotency, or credential-hardening fix. No previously removed
unsafe behavior is reintroduced.

The campaign change extends the campaign pinning introduced by `9845ec7`,
`a907254`, and `2d0e380`: the pinning rule is preserved while its missing
completion signal is supplied from typed inventory.

## Test and quality evidence

Local evidence for the working diff:

- formatting, strict workspace/all-target/all-feature Clippy, production
  panic-shortcut Clippy, rustdoc warnings, and the full workspace suite pass;
- 354 tests pass and the manual queue profile is intentionally ignored;
- campaign completion, integer-boundary, precision-ordering, formatting
  overflow, webhook, and queue metric behavior have focused tests;
- release-mode queue sweeps show noise-scale differences at capacities 32, 64,
  and 128, so production remains at 64;
- the Rust/Go normalized prediction workload produces the same checksum;
- `cargo audit`, full-history native Gitleaks, documentation links, PowerShell
  parsing, release hygiene, and the pinned Go baseline/parity suite pass.

Windows cannot link the pinned libFuzzer coverage symbols, including with the
sanitizer disabled. This is a host-toolchain limitation, not a fuzz finding.
The repository's scheduled/manual Linux workflow remains the authoritative
ASan fuzz gate for both targets.

## Remaining release gates

- Run the Linux CI dependency policy, coverage, fuzz, mutation, and
  reproducibility jobs on the committed revision.
- Build and attest the exact multi-architecture image from the merged revision.
- Canary and deploy that immutable ARM64 digest to the Pi.
- Re-establish non-backdated health, reward-rate, campaign, and timed-soak
  evidence before pruning rollback material.

## Methodology

The repository has 91 Rust/Go source files, so this used a focused review:
all changed paths were inspected, all high-risk functions received baseline
history, one-hop caller/blast-radius, invariant, adversarial-input, and focused
test analysis, while documentation-only changes received a surface review.
External crate source was not audited beyond RustSec, Cargo policy, pinned
workflow provenance, and checksum verification. Confidence is high for the
changed code and medium until the live Twitch/Pi acceptance gates complete.
