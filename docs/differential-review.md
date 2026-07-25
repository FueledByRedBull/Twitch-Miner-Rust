# Differential Review

## Executive summary

**Baseline:** `4b3de95122c1b20919f8fec4b184bbcc36aa1a60`

| Severity | Count |
| --- | ---: |
| Critical | 0 |
| High | 0 |
| Medium | 0 |
| Low | 0 |

**Overall risk:** Low after verification
**Recommendation:** Approve after the required Linux CI, immutable-image, and
live Pi acceptance gates pass.

This change reduces concentrated module and orchestration complexity without
changing public APIs or mining behavior. The security-sensitive EventSub
parser, prediction validation, bounded deduplication, Twitch response
normalization, config composition, and single-writer runtime behavior were
moved behind existing facades rather than redesigned. No validation, retry
restriction, privacy boundary, or mutation guard was removed.

## What changed

The staged change contains 38 paths and approximately 6,591 additions and 6,325
deletions. Most of that volume is code and tests moved out of oversized files.

| Area | Risk | Result |
| --- | --- | --- |
| EventSub | High | Capacity planning and protocol normalization moved into private responsibility modules; connection lifecycle and the public facade are unchanged. |
| Twitch HTTP | High | Typed response validation and normalization moved out of request execution; endpoint, retry, and mutation behavior are unchanged. |
| Configuration | Medium | Typed base/override composition moved out of persistence and migration code with the serialized schema unchanged. |
| `tm-app` orchestration | Medium | EventSub and PubSub now embed one crate-private runtime-effect context instead of duplicating five handles. |
| Test placement | Low | Large private unit suites moved under each crate's `tests/unit/` tree through `cfg(test)` path modules; no public test hooks were added. |
| Architecture policy | Low | A dependency-free PowerShell gate records the existing internal Cargo graph, domain isolation, and test-placement boundary. |
| Public documentation | Low | Crate responsibilities, invariants, checks, and release commands now describe the implemented architecture. |

## High-risk path analysis

### EventSub input and prediction validation

**Entry and trust boundary:** Untrusted WebSocket text enters
`parse_eventsub_message` in
`crates/tm-pubsub/src/eventsub/protocol.rs`. Notification bodies are decoded
into transport-neutral runtime events only after message-type, subscription,
condition, identifier, timestamp, and prediction-field validation.

**Invariants:**

- malformed or incomplete prediction payloads cannot become runtime events;
- broadcaster and user identifiers remain tied to the subscription condition;
- duplicate message IDs remain bounded by both capacity and age;
- deterministic subscription-cost planning cannot exceed Twitch's reported
  capacity;
- parsing and planning remain behind the existing `tm-pubsub` facade.

The parser, `validate_prediction_wire`, and `MessageDeduper` are direct moves
from the baseline module. The EventSub client still owns connection setup,
keepalive, reconnect, and subscription creation. All 41 `tm-pubsub` tests pass,
including the relocated protocol, deduplication, prediction, and planning
cases.

**Adversarial scenarios:** A malformed prediction omitting an outcome ID, an
oversized reconnect payload, repeated message IDs, and a subscription set whose
cost exceeds capacity follow the same reject, deduplicate, or planning paths as
the baseline. The extraction adds no alternate parser, fallback, or unchecked
deserialization path.

### Twitch response normalization and mutation safety

**Entry and trust boundary:** Authenticated Twitch HTTP/GQL responses are
decoded and validated in `crates/tm-twitch/src/responses.rs`; request execution,
classified read retry, and non-replayed mutations remain in
`crates/tm-twitch/src/client.rs`.

Moving response types and normalization does not broaden their visibility
outside the crate or weaken typed checks. Inventory campaign completion still
fails safe when IDs or progress are missing. Prediction and claim mutations
still cannot be retried after an uncertain response. All 41 `tm-twitch` tests
pass, including malformed-response and campaign inventory fixtures.

### Runtime ownership and orchestration

`EventSubTaskContext` and `PubSubTaskContext` now embed one
`RuntimeEffectContext`. The shared object contains the same runtime handle,
Twitch client, persistent user ID, observability handle, and health tracker that
were previously copied into both transport contexts.

The fields are exposed only within the `tm-app` crate. Transport credentials,
tracked-streamer state, subscription authorization, and fallback channels stay
in the narrower transport contexts. Event application still enters the bounded
single-writer actor, and only actor-approved effects reach the Twitch client.
All 88 `tm-app` tests pass.

**Adversarial scenario:** Simultaneous EventSub and PubSub observations still
serialize through the same runtime actor. Consolidating cloned handles does not
create shared mutable state, bypass the actor, or add a second effect executor.

### Architecture enforcement

`scripts/verify-architecture.ps1` obtains the workspace graph from locked Cargo
metadata and compares every internal crate against the current allowlist. It
also rejects async, HTTP, transport, tracing, or application dependencies in
`tm-domain`, and prevents substantial named test files from returning under
production `src` trees.

The check uses existing PowerShell and Cargo tooling, runs in CI, and adds no
dependency or production surface. A legitimate future dependency-boundary
change must deliberately update the checked policy rather than drifting
silently.

## Blast radius

- Public crate facades and serialized configuration remain unchanged.
- EventSub parser and planner callers remain inside `tm-pubsub`.
- Twitch response helpers remain crate-private and are called by the same
  request methods.
- Runtime-effect context access expands only from its defining module to its
  containing crate so the two transport contexts can embed it.
- Production behavior is exercised by the unchanged workspace, integration,
  contract, replay, and release-hygiene suites.

No new network call, external process, credential source, persistence format,
unsafe block, telemetry path, runtime dependency, or public API was introduced.

## Historical context

Git pickaxe and blame trace EventSub subscription planning and prediction wire
validation to `7b22ffe`, campaign inventory normalization to `22b8314`, and the
runtime-effect context to `db90940`. The reviewed change preserves those
hardening decisions. No removed line originated in a CVE fix, authorization
check, mutation-idempotency restriction, credential guard, or privacy fix.

Recent releases `#42`, `#48`, `#49`, and `#50` were also inspected for
architectural intent. The refactor follows their stable facades and transport
ownership instead of restoring an older implementation.

## Test and quality evidence

Local evidence for the staged diff:

- `cargo fmt --all -- --check`;
- strict workspace/all-target/all-feature Clippy with warnings denied;
- stricter production Clippy denying panic shortcuts;
- full workspace/all-target/all-feature tests;
- rustdoc with warnings denied;
- locked release build;
- documentation, architecture, release-hygiene, and Compose validation;
- five release-mode deterministic replay processes with 20 repetitions each.

All completed checks pass. The replay checksum and trace remain stable. The
dedicated build-integrity check and committed Linux CI/deep-quality jobs remain
release gates rather than being inferred from the working tree.

## Remaining release gates

- Run build integrity on the final committed tree.
- Pass Linux CI, dependency policy, coverage, fuzz, mutation, documentation,
  and reproducibility checks on the pushed revision.
- Build and attest the exact multi-architecture image from the merged revision.
- Canary and deploy that immutable ARM64 digest to the Pi.
- Establish a fresh non-backdated strict-health baseline and pass the complete
  24-hour and 72-hour live acceptance audits.
- Delete rebuildable local, CI, registry, and Pi build artifacts only after the
  72-hour gate; preserve required rollback evidence until then.

## Methodology and limitations

This was a focused security differential review of the baseline through the
staged tree. Every changed production path was inspected; high-risk input and
effect paths received history, caller/blast-radius, invariant, adversarial, and
test analysis. Relocated tests were compiled and executed in their new
locations. Documentation-only changes received a surface and consistency
review.

External dependency source was not re-audited because manifests and the lockfile
did not change. Confidence is high for source-equivalence and local behavior,
and remains medium for deployment behavior until the exact image completes the
fresh live soaks.
