# Twitch Miner differential review: recovery and fair watch rotation

Review date: 2026-07-20

Original recovery base: `95a5de03fd43cd3d37a68ed6b9bd0e6c24b9a117`

Current follow-up base: `5db10101845b98eae161051df2d0e82b1c2fec33`

Candidate: the `fix/fair-all-channel-rotation` working tree. This cumulative
report covers startup/network recovery, the narrow read-only service-error
retry, and the live-rate correction prompted by successful-but-uncredited direct
Spade watch requests. Final deployment evidence belongs in the release record
after the candidate is merged and published by CI.

## Executive summary

| Severity | Unresolved findings |
| --- | ---: |
| Critical | 0 |
| High | 0 |
| Medium | 0 |
| Low | 0 |

**Overall risk:** Medium. Authentication, process supervision, and external
watch scheduling are high-risk paths, but each correction is narrow, preserves
fail-closed validation, and adds focused regression coverage.

**Recommendation:** Approve after the full workspace, documentation, release,
dependency, and build-integrity gates pass. Deploy only the exact CI-published
digest and start a new acceptance clock; the failed clock must not be backdated.

Key metrics:

- Current follow-up includes the scheduler plus the typed playback/HLS client,
  canary, protocol inventory, tests, and operator documentation.
- 80 Rust files in the medium-sized repository; focused review covered every
  changed file, the runtime selector, external minute-watch path, and the single
  production caller.
- 0 changed high-risk functions without focused tests.
- 0 removed authorization, TLS, contract-validation, or mutation-idempotency
  checks.

## What changed

The failed candidate remained available during shared network loss, but the
process exited repeatedly while validating its saved Twitch session and after
five EventSub failures. The correction separates an active recovery loop from a
silent task while retaining a strict external health signal.

| File | Added | Removed | Risk | Blast radius |
| --- | ---: | ---: | --- | --- |
| `crates/tm-app/src/bootstrap.rs` | 152 | 11 | High | Low: one production entry point and tests |
| `crates/tm-app/src/status.rs` | 24 | 18 | High | Medium: all ten background health entries |
| `crates/tm-app/src/shutdown.rs` | 1 | 1 | Medium | Low: startup retry and runtime shutdown |
| `crates/tm-app/src/app_tests.rs` | 80 | 4 | Low | Test-only |
| Changelog and operator documents | 20 | 6 | Low | Operator-visible behavior |

The saved-session path now retries only timeouts, connection/request failures,
HTTP 429, and HTTP 5xx responses with a 5-second to 5-minute capped exponential
backoff. HTTP 400/401/403 and login mismatch still enter reauthorization;
malformed or incomplete successful responses still stop startup. Retry logs use
fixed classes and do not include the raw error, request, or credential.

Task status schema 5 records both last success and last activity. External
`--health` continues to use last success and the consecutive-failure threshold.
Internal supervision uses last activity, so a task that is actively retrying
stays in the same process, while a silent task still triggers controlled
termination. Unexpected exits and panics remain independently supervised.

## Critical findings

No unresolved critical or high-severity finding was identified.

The live incident itself exposed two high-impact defects in the base revision:

1. A transient saved-session validation failure exited the process, allowing
   the container restart policy to create a restart storm.
2. Internal supervision treated five reported EventSub failures as proof that
   the retrying task was dead and terminated the whole miner.

Both root causes are directly corrected by this candidate rather than hidden by
loosening Docker health or restart configuration.

## Test coverage analysis

| Changed behavior | Coverage | Result |
| --- | --- | --- |
| Connection closes before a saved-session validation response, then recovers | `saved_session_validation_recovers_without_device_login_after_transient_failure` | Same saved token is retained, two validation requests occur, and device login is not entered |
| Retry classification and maximum delay | `saved_session_retry_policy_is_transient_and_bounded` | 429/5xx are transient, contract failure is not, and delay caps at 300 seconds |
| Degraded retry loop versus stale activity | `stale_or_repeatedly_failing_tasks_fail_health` | External health fails, active supervision passes, inactive supervision fails |
| Status publication while degraded | `reporter_publishes_degraded_task_state_before_returning_an_error` | Degraded status is written before external health returns an error |
| Exited and panicked tasks | Existing `tasks` tests | Mandatory task exits/panics remain detected; PubSub remains isolated until its activity deadline |

The focused tests cover all new decision branches except OS signal delivery
itself. Signal handling is shared with the already exercised normal shutdown
path and will be checked again during digest-pinned deployment.

## Blast-radius analysis

| Function or data contract | Direct callers/consumers | Risk | Review conclusion |
| --- | ---: | --- | --- |
| `load_or_login_session_with_auth_client_and_retry` | 2 call sites plus its regression | High | Only saved-session validation is retried; device flow and post-login validation are unchanged |
| `saved_session_retry_class` | 1 production caller plus unit test | High | Allowlist is narrow; auth rejection and response-contract errors cannot loop |
| `HealthTracker::success` / `failure` | 43 call sites | High | Existing success/failure semantics remain externally visible; activity is updated on either outcome |
| `validate_common_status` | 2 validators | High | External and internal policy are explicit rather than task-name exceptions |
| `wait_for_shutdown_signal` | 2 production callers | Medium | Visibility changed only within the crate; behavior is unchanged |
| Status schema 5 | CLI status, health, support bundle, Docker health | Medium | Schema bump makes stale schema-4 files fail closed until the new runtime publishes schema 5 |

All task retry/cadence bounds were compared with their activity deadlines.
EventSub and PubSub cap reconnect waits at five minutes against an eight-minute
deadline. Presence, context, pending-claim, minute, drop, chat, streak-cache,
and streak-recovery loops report on cadences below their configured inactivity
deadlines. A hung network call remains bounded by the HTTP/WebSocket clients or
eventually appears inactive to supervision.

## Adversarial analysis

Attacker model: an unauthenticated network peer, local network failure, or
upstream Twitch failure that can delay, reset, rate-limit, or return malformed
authentication/transport responses. The attacker has no filesystem or process
control.

Scenarios reviewed:

- **Persistent connection failure:** the miner remains in one process and backs
  off to five minutes. Docker health is unhealthy and logs expose only a fixed
  class. No device code is requested, token is not deleted, and no mutation is
  issued.
- **Forged auth rejection:** HTTP 400/401/403 does not enter the transient loop;
  the established reauthorization path is used. This does not weaken the
  existing identity and scope checks.
- **Malformed success response:** missing user ID, login mismatch, decode, or
  body errors are not transient. Startup fails closed rather than accepting an
  unvalidated identity.
- **Active-failure denial of service:** repeated task failures can keep the
  process alive, but external health stays failed and every retry updates
  activity. If reporting stops beyond the task deadline, supervision exits the
  process. Mutation requests are not part of these retry loops.
- **Shutdown during startup outage:** the backoff wait selects on the same
  CTRL-C/SIGTERM handling used by the running service, so an operator is not
  forced to wait for the five-minute cap.

No reviewed scenario bypasses authentication, reveals credentials, duplicates
a points mutation, or converts an invalid Twitch contract into accepted state.

## Historical context

- Saved-session loading dates to `e32e8ae`; strict non-auth failure propagation
  was added in `23d17be` to prevent transient errors from being mistaken for
  invalid credentials. This candidate preserves that distinction and changes
  only the recovery location from container restarts to bounded in-process
  retries.
- Task freshness/failure supervision was introduced in `ccdfd40`; the PubSub
  exception was added in `7b22ffe`. This candidate generalizes the correct
  principle to every active retry loop while restoring supervision of silent
  PubSub activity.
- Git pickaxe/blame found no removed security fix, reintroduced unsafe code, or
  prior credential-validation bypass in these hunks.

## Recommendations

Immediate merge gates:

- Run formatting, full workspace tests, strict Clippy, warning-free rustdoc,
  dependency audit, documentation verification, release hygiene, and optimized
  build-integrity checks.
- Inspect the final diff for unintended compatibility, fallback, placeholder,
  or generated-artifact changes.

Production gates:

- Merge through a reviewed PR and wait for the exact commit's CI and
  multi-architecture manifest.
- Canary and deploy the immutable digest with the retained prior digest as
  rollback.
- Verify exact revision, zero unplanned restarts, schema-5 status, all task and
  capability states, EventSub/PubSub recovery, sanitized warnings/errors,
  reward/action parity, mutation uniqueness, and points acquisition.
- Start fresh 24-hour and 72-hour clocks after the final intentional restart.

## Analysis methodology

**Strategy:** Focused analysis for a medium repository.

The review read all changed source and documentation, the base versions of the
high-risk regions, direct callers and health reporters, retry-loop cadences,
status consumers, existing and new tests, and relevant git history/blame. It
applied auth trust-boundary review, denial-of-service and malformed-response
modeling, validation-removal checks, blast-radius counting, and test-gap
analysis.

Limitations: the review does not claim Twitch availability or long-duration
runtime stability from local fixtures. Those are explicit digest-pinned Pi soak
gates. Confidence is high for the changed code path and medium for external
availability behavior until the replacement soak passes.

## Appendix: invariants retained

1. A saved token is trusted only after Twitch validates login, user ID, and
   scopes.
2. Definitive authentication rejection never masquerades as a transient outage.
3. Response-contract failures fail closed.
4. Logs and status do not contain tokens, cookies, request headers, raw account
   payloads, or channel/user identifiers.
5. Retriable reads may repeat; points-changing mutations remain single-attempt.
6. Operational health reports prolonged failure even when the process remains
   available to recover.
7. Silent, exited, or panicked mandatory tasks still cause controlled process
   termination.

## Canary follow-up

The first published network-recovery candidate and its rollback both failed the
same exclusive canary stage while Twitch intermittently returned HTTP 200 with
one GraphQL error at the `user.videos` path. Sanitized repeated probes confirmed
that the error message is exactly the fixed `service error` value and that the
next responses contain seven valid, fully typed archived-video nodes. The final
candidate therefore retries only read-only envelopes whose non-empty error list
consists entirely of that fixed service error. Unknown or mixed GraphQL errors,
response-shape failures, and all mutations remain fail-closed/single-attempt.

## All-channel watch follow-up

Live rate evidence first found six online channels but one five-minute WATCH
reward stream. The configured campaign single-watcher policy had reduced the
historical two-slot selector to one. Disabling that policy and sending minute-
watched heartbeats to all five channels in a later fresh session produced zero
WATCH rewards over 15.5 minutes despite continuous HTTP 204 responses and zero
task failures. The protected historical two-slot image was restored as a control
and also produced zero rewards over 19.2 minutes. That disproves concurrency as
the sole cause and isolates the shared direct-Spade path, which omitted the
Python parent's playback-token/HLS media preflight.

The corrected follow-up restores that algorithmic stage using a typed read-only
`PlaybackAccessToken`, bounded HLS master/media reads, and a media-segment HEAD
request before Spade. It also keeps every eligible online channel in the
deterministic priority set, but fairly rotates that set through Twitch's two
creditable slots every 15 minutes. Fifteen-minute dwell preserves three five-
minute WATCH opportunities, the bonus opportunity, and streak eligibility before
rotation. Offline, suspended, and game-excluded channels are removed immediately; newly eligible
channels join the queue; the explicit single-watcher campaign policy remains
available. Focused tests cover dwell boundaries, wraparound fairness, immediate
ineligible removal/refill, and one/two-channel operation.

The differential blast radius is limited to minute-watcher target selection:
authentication, EventSub/PubSub routing, point mutations, and retry policy are
unchanged. The existing sequential loop and five-second minimum request spacing
remain in force; the active count is now at most two, so the request interval is
the established ten seconds per slot.

Adversarial review covered Twitch-driven online/offline flapping, reordered and
duplicate eligible input, odd-sized queues, newly eligible channels, empty
queues, process restarts, and backward wall-clock adjustment. Active-channel
removal refills immediately without resetting the rotation deadline, preventing
a flapping channel from starving the rest of the queue. Queue construction
deduplicates input, odd-sized wraparound remains balanced, and process restarts
reset only in-memory scheduling without changing points or persisted state. A
large backward host-clock jump can delay one rotation; this is an operational
clock-integrity limitation rather than an attacker-controlled Twitch input and
is covered by the zero-restart/host-health soak gate.

The playback token crosses a high-risk external boundary. Its signature/value
remain private typed fields, query parameters are never logged, request failures
are reduced to fixed context plus failure class, and malformed or empty token/
playlist fields fail closed. HLS URLs require HTTPS public-domain syntax in
production; HTTP is accepted only for loopback fixtures. No cookies or
authorization headers are sent to HLS/CDN requests. The shared HTTP client still
follows provider-directed redirects, so exact-image canary and sanitized failure-
log review remain mandatory.

The scheduling blast radius is one production caller; playback adds one typed
read-only contract and three bounded media reads to that caller. All new
scheduling, protocol, parsing, and tokenized-error-redaction branches have
focused tests. Review found no unresolved critical, high, medium, or low issue.
Recommendation remains conditional on the full validation ladder, exact-image
canary, and live two-slot WATCH/CLAIM acquisition plus full-queue rotation
evidence.
