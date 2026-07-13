# Differential review: TODO completion candidate

Review date: 2026-07-13

Base revision: `0b2eb1c2ff01533129aa0efdfe5b8d2badcf851b`

Candidate: the uncommitted working tree implementing the completion roadmap.
The final revision and immutable image digest must be added to the release record
after the external acceptance gates pass.

## Scope and blast radius

This is a large, cross-cutting candidate. It changes transport policy and
parsing, runtime idempotency and reporting, Twitch read contracts, health
reporting, log formatting/privacy, deployment validation, fixtures, CI parity,
and operator documentation. The highest-risk paths are:

1. authenticated PubSub LISTEN and EventSub subscription setup;
2. conversion of untrusted network payloads into runtime effects;
3. single-attempt reward, prediction, moment, raid, and goal mutations;
4. balance/history reconciliation and prediction settlement;
5. credential-safe logging, status, support bundles, and final reports;
6. digest-pinned candidate replacement and rollback.

The runtime remains a single-writer actor. EventSub, PubSub compatibility,
presence polling, IRC, context refresh, drops, and minute watching feed that
actor through typed messages. Runtime effects are executed outside the actor so
network waits do not block state updates.

## Reference and history review

- Recent Rust history was reviewed through the eight commits preceding the
  candidate. No earlier revision contains the complete dual-transport,
  reporting, health, and release-safety behavior in this candidate.
- Python behavior was checked at
  `a0cac46ca90dccc4156da6dae419a74348a4990a`.
- Go behavior and persisted operations were checked at
  `91f00698314dbbdd6c757d7b525458c82173e622`.
- PubSub remains an explicitly isolated compatibility path. EventSub/polling is
  the supported presence path; ordinary viewer prediction discovery still
  requires PubSub compatibility because EventSub prediction subscriptions
  require broadcaster authorization.
- The latest Go six-hour broadcast cache was not copied. Rust continues to read
  authoritative startup state and avoids adding another persisted identifier
  cache. This is an optimization difference, not a supported-feature gap.

## Security and correctness review

### Authentication and privacy

- OAuth material is sent only in the required HTTP headers or authenticated
  PubSub user-topic LISTEN frames. Channel topics are not given the auth token.
- Transport failures are logged as fixed classes; raw response bodies, headers,
  OAuth values, nonces, and topic identifiers are not emitted by production
  failure logs.
- Anonymized logging suppresses structured fields and uses stable streamer
  aliases/pseudo-balances. Review found and fixed two message-body leaks: raw IRC
  callback strings and the local final-report log path are now redacted.
- Support bundles contain counts and sanitized runtime status, not config,
  cookies, logs, webhook URLs, or response payloads.

### Network and parser boundaries

- PubSub is capped at 50 topics per connection, 10 connections, and 500 topics
  total. Capacity overflow fails closed and is health-reported.
- PubSub reconnect, PING/PONG timeout, RECONNECT, malformed frames, partial
  LISTEN failures, bad authentication, shutdown, and replay behavior have local
  coverage.
- EventSub setup reads current cost metadata before planning, caps a connection
  at 300 subscriptions, retains successful subscriptions after a partial
  failure, and reports per-streamer capability/source decisions.
- EventSub creation and list responses use their distinct documented envelopes;
  validated OAuth scopes, rather than account identity alone, control whether
  broadcaster prediction subscriptions are planned.
- EventSub notification parsing requires tracked broadcasters and validates
  lifecycle-specific prediction timestamps, statuses, outcome colors, counts,
  and resolved winners before constructing runtime events.
- Read-only Twitch requests have bounded retries and timeouts. Mutations remain
  single-attempt to avoid replay after an uncertain response.

### State and mutation safety

- EventSub message IDs and runtime mutation identifiers are bounded to prevent
  untrusted state growth.
- Prediction discovery retains the most recent 128 IDs after active state is
  removed, preventing bounded replays from scheduling a second bet.
- Claim, moment, and raid identifiers are recorded before their single-attempt
  effects. A timeout therefore does not trigger an automatic replay; an operator
  must reconcile uncertain mutation state instead.
- Point replay suppression is balance-aware. An immediate identical event is
  ignored only while the post-application balance is unchanged. Prediction
  stakes and authoritative balance reconciliation invalidate the replay key, so
  a legitimate later equal gain is accepted.
- Completed prediction report state and transport message IDs are bounded.
  Duplicate transport events do not produce duplicate settlement effects or
  notification lines.

### Canary, health, and deployment

- Canary mode uses existing credentials, fixed failure classes, bounded network
  time, and no farming mutations or Discord notifier. Review found and fixed a
  local-write defect: canary mode no longer creates its work directory or opens
  a file logger when `save_logs` is configured. EventSub subscription creation
  is temporary setup state attached to the canary WebSocket and is verified
  before the socket is closed.
- PubSub degradation remains visible to external `--health` without terminating
  EventSub, polling, IRC, drops, or minute watching. Mandatory task exit/panic or
  stale health still requests controlled shutdown.
- The guarded deployment helper accepts only immutable GHCR digests from the
  same repository, verifies both revisions/configs, requires the supplied
  rollback reference to match the running service, runs the candidate canary
  before replacement, and waits for revision, application health, Docker health,
  and zero restarts. Candidate failure triggers rollback and verifies rollback
  health.

## Findings fixed during review

| Severity | Finding | Resolution |
| --- | --- | --- |
| High | An immediate raw-signature point dedupe could suppress a legitimate equal gain after a prediction stake or balance reconciliation. | Dedupe now includes post-application balance and invalidates keys when balance moves; regression covers replay suppression and later acceptance. |
| High | `--canary` could open a configured log file despite the release command mounting `/data` read-only. | Canary initialization now enters an existing directory and forces console-only logging; regression verifies `save_logs` is disabled. |
| Medium | Anonymized logging could retain raw IRC callback text and reveal the local log path in the final report. | Callback messages and report paths are explicitly redacted; focused regression covers both helpers. |
| Medium | EventSub prediction parsing used a generic timestamp/status shape rather than Twitch's lifecycle-specific fields. | Begin/progress, lock, and end now validate their documented fields and result constraints with fixtures/tests. |
| Medium | Optional malformed stream `createdAt` could fail otherwise usable stream metadata. | The optional timestamp now degrades to `None`, matching the current Go behavior; required stream fields remain strict. |
| Medium | Immediate post-deploy health checks could reject a service still inside its declared startup period and did not bind rollback input to the running image reference. | Deployment validation now uses a bounded readiness loop and verifies the exact running rollback image before replacement. |
| Medium | The typed EventSub create response incorrectly required list-only `pagination`, so live successful `202` responses were classified as failures. | Create/list envelopes are separate; the documented no-pagination create shape is regression-tested and passed the ARM64 canary. |
| Medium | A null available-drops channel/list was treated as contract corruption even though Twitch and the Go reference use it for an empty result. | Null containers now normalize to an empty campaign set; IDs in a present list remain strict. |
| Medium | EventSub prediction planning inferred authorization from the authenticated account ID without checking the token's validated scopes. | OAuth validation scopes remain ephemeral in memory and prediction EventSub is planned only with a read/manage prediction scope. |

No unresolved local critical or high-severity finding is known at this stage.

## Validation evidence

Completed during the candidate review:

- focused point replay/idempotency regression;
- focused EventSub, PubSub, runtime, app, config, Twitch, contract, and
  integration tests;
- full workspace/all-target/all-feature tests on the frozen candidate;
- formatting and strict workspace Clippy with warnings denied;
- warning-free workspace rustdoc and a cached 1,160-advisory dependency audit;
- tracked-name and filename-only secret-pattern scans;
- guarded deployment helper validation and release-hygiene checks;
- optimized release build-integrity/revision check.
- an isolated ARM64 worktree image on the Pi reported the expected target and
  the frozen r4 source passed the 2026-07-13 `18:59:44Z` read-only Twitch canary
  after the live contract fixes; the preceding r3 source also passed twice
  consecutively;
  config, cookies, backups, production container identity, health, and restart
  count remained unchanged.

Go is unavailable on this Windows host; the pinned Go parity test remains a CI
gate. The local startup sample is explicitly non-comparative: five Windows x64
`--help` runs measured 8.0-15.8 ms with a 7,523,328-byte release binary. Pi and
equivalent Go/Rust workload measurements remain external acceptance work. Five
native Pi runs of the extracted static ARM64 worktree binary measured startup
min/median/max `5.242/5.368/6.865 ms`; the binary was 6,104,336 bytes with
SHA-256 `7dc07b89f620dea8c4a5507bab69c7e07b1efefa2405b785a26775a8056d5ec9`.

## External release blockers

The local ARM64 worktree candidate now establishes read-only Twitch contract
acceptance and Pi execution for its exact uncommitted bytes. It does not prove a
future published digest, multi-architecture provenance, candidate deployment,
rollback execution, controlled mutation acceptance, or soak stability. Do not
call the candidate complete or create the release commit until those remaining
roadmap gates have recorded evidence.
