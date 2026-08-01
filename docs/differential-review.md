# Differential Review

## Current review: protocol and accounting hardening

**Baseline:** `3a9c30c4f980a8baeff779404cae6f045afa6898`

**Candidate:** `next-release-protocol-hardening` working tree, 2026-08-01

| Severity | Open | Resolved during review |
| --- | ---: | ---: |
| Critical | 0 | 0 |
| High | 0 | 0 |
| Medium | 0 | 0 |
| Low | 0 | 1 |

**Overall source risk:** Low after review and local verification.

**Recommendation:** Conditional approval for PR/CI and the guarded image
pipeline. Do not merge or deploy until the exact clean commit passes pinned
dependency policy, coverage, mutation, Linux fuzzing, multiarchitecture image
smoke, canary, and review checks.

The reviewed implementation set contains 24 files and approximately 1,004
additions and 101 deletions, excluding this report. All changed production
functions have direct regression coverage. No authentication, endpoint-origin,
single-writer, mutation-uniqueness, campaign, or cookie-trust boundary was
weakened.

### Change and risk summary

| Area | Risk | Production blast radius | Review result |
| --- | --- | --- | --- |
| EventSub errors | Medium | One supervised connection loop | Concrete sanitized causes are retained; keepalive silence is no longer classified as rejected protocol input. |
| Context refresh | Medium | Context and pending-claim loops | All children drain; successful siblings apply; partial failure reaches task health. |
| Streak policy | Medium | One watch-selection predicate | Ten-minute budget is p90 plus bounded margin; restart, campaign, promotion, and rotation invariants remain. |
| Prediction stealth | High | One prediction decision boundary | One injected `1..=5` offset; balance, maximum, effective minimum, and top-predictor bounds hold. |
| HLS and Spade | High | One minute-watch preflight path | Semantic parsing retains final public-HTTPS/loopback validation; no fallback chain or cache was added. |
| Watch progress | Medium | One runtime-actor event reducer | Only bounded positive intervals add progress; stale/backward gaps re-anchor without credit or deletion. |
| Cookie migration | High | Operator trust boundary only | Documentation requires device login and explicitly forbids pickle inspection or conversion. |

### High-risk analysis

#### Twitch-supplied playback and telemetry URLs

**Attacker model:** a compromised or malformed Twitch document controls HLS
playlist lines or the `spade_url` JSON string, but does not control the miner's
compiled endpoints or local configuration.

The baseline selected the final non-comment playlist URI. The candidate parses
`EXT-X-STREAM-INF`, requires a positive bandwidth and an explicit video signal,
chooses the lowest-bandwidth playable variant, then requires an `EXTINF` media
segment. Relative URLs are resolved against their containing playlist.

The trust boundary remains after resolution: every selected playlist, segment,
and Spade URL must pass `validate_remote_endpoint`, which accepts public HTTPS
and test-only loopback HTTP. A document cannot use relative resolution,
whitespace, JSON escaping, audio-only renditions, or playlist reordering to
bypass the origin check. Rust's regex engine is linear-time, and JSON decoding
validates escape sequences before the endpoint is used.

**Adversarial result:** no new SSRF or credential-disclosure path was found.
Malformed primary shapes fail closed. Playback priming stays uncached because
request volume alone does not prove that reuse preserves credited rewards.

#### Prediction amount transfer

**Attacker model:** Twitch supplies outcome totals/top-predictor points while an
operator enables stealth mode and configures percentage/maximum limits.

The application chooses one randomized integer offset and injects it into the
pure decision function. The amount is still bounded by the non-negative
balance and configured maximum. Stealth applies only when the calculated amount
would meet or exceed the leading predictor, and it cannot lower a placeable bet
below the effective ten-point minimum.

Review found one pre-existing edge case where `balance < max_points < 10` could
raise the normalized amount above balance. The candidate now clamps that branch
to balance and has a regression test. No open value-transfer finding remains.

### State and liveness analysis

- EventSub logging computes the class once and includes the typed error display.
  Reconnect URLs and credentials are not part of the displayed variants.
- `KeepaliveTimeout` is a typed liveness error; protocol JSON, timestamp, and
  modelled-payload rejection remain distinct.
- Context refresh aggregates ordinary errors and panics only after every
  bounded child finishes. One child's failure cannot cancel or roll back a
  successful sibling update.
- Confirmed watch progress accepts at most 400 seconds: two selected slots,
  each with a 90-second request bound plus a 10-second interval, across two
  passes. Longer, zero, and backward deltas update only the timestamp anchor.
- The ten-minute streak budget comes from 28 sanitized observations over the
  available approximately 43-hour session (median four minutes, p90 seven),
  plus a three-minute margin. Detection time is not claimed as authoritative
  broadcast start.

### Test and tool evidence

Passed locally on the candidate working tree:

- formatting and diff hygiene;
- locked workspace tests for all targets and features;
- strict all-target/all-feature Clippy plus production no-panic shortcuts;
- warning-free workspace rustdoc;
- architecture, documentation, build-integrity, and release-hygiene scripts;
- isolated fuzz-workspace compilation;
- Go baseline tests and persisted-operation parity;
- local RustSec audit with no reported advisory;
- same-host semantic Rust/Go comparison with identical complete checksums.

The same-host parent/candidate comparison found no decision-throughput
regression after normalizing against Go: the Rust/Go ratio changed by -1.39%,
candidate startup improved 4.67%, and the stripped binary grew 6,144 bytes.

Local execution of the pinned fuzz binary stopped before fuzzing with Windows
`STATUS_DLL_NOT_FOUND`; compilation succeeded. `cargo-llvm-cov`,
`cargo-mutants`, and `cargo-deny` are not installed locally. Their pinned Linux
CI jobs are therefore mandatory release evidence rather than silently skipped
checks.

### Historical context and regression assessment

- The original fixed stealth decrement and Go-style 7/20-minute streak policy
  entered in `f8fc4cf`; the unexplained flat 15-minute policy entered in
  `be7ecd7`. Neither was a security fix being reverted.
- Playback priming and final-URI selection entered in `c8d5d2b`; the complete
  token/HLS/Spade sequence is retained, with only parsing made semantic.
- Context child-warning behavior traces to module decomposition in `8341456`;
  the new aggregation preserves its bounded sibling execution.
- No removed validation originated in a security/CVE commit, and no previously
  removed insecure fallback was reintroduced.

### Remaining release gates

1. Commit the reviewed tree and rerun clean-revision replay/performance/build
   integrity evidence.
2. Require every PR and deep-quality check, including dependency policy,
   coverage, mutation, and Linux fuzzing.
3. Publish and verify the exact multiarchitecture manifest through GitHub.
4. Run the exclusive canary, guarded ARM64 digest deployment, initial live
   health/points/campaign acceptance, artifact cleanup, and fresh soak.

## Historical review: previous release

### Executive summary

**Baseline:** `2389550662cfe0de61d2b6c02837262b3a003036`  
**Candidate:** `01c01a394ab5485f645df52ea005bfc49385549f`

| Severity | Count |
| --- | ---: |
| Critical | 0 |
| High | 0 |
| Medium | 0 |
| Low | 0 |

**Overall risk:** Medium until the exact image completes CI, canary, and live
acceptance; low at source level after local verification.

**Recommendation:** Approve for the guarded release pipeline. Do not merge or
deploy if the pinned Linux fuzz, mutation, coverage, dependency, image, or
canary gates fail.

Key metrics:

- 23 changed files, 807 additions, and 285 deletions across two commits.
- Every changed production behavior has focused regression coverage.
- One high-risk transport change has one production caller and removes an
  existing credential-exposure path.
- No validation, authentication, mutation-safety, or privacy regression found.

## What changed

| Area | Risk | Blast radius | Result |
| --- | --- | --- | --- |
| IRC connection | High | One production caller | Plaintext port 6667 is replaced by Rustls/WebPKI hostname and certificate verification on port 6697. |
| Watch accounting | Medium | One selection path and the runtime actor | Stable channel IDs reset progress only when a selected slot is released. |
| Streak selection | Medium | One selection path | One jump per broadcast and rotation window preserves campaign priority and eventual fair rotation. |
| Stream metadata | Medium | Startup and minute refresh writers | Exact game ID and tags are retained so the minute payload reuses the refreshed snapshot. |
| Drop claiming | Medium | Startup and periodic claim passes | Individual failures no longer prevent later claims; the task still returns one classified failure. |
| Prediction ties | Medium | Five strategies and the Smart fallback | Equal scores retain the first outcome, matching the Go and Python parents. |
| Client cleanup | Low | No callers | Six unreachable raw/alias methods are removed from non-published path-only crates. |
| Documentation | Low | Operators and maintainers | Protocol, parity, security, and release notes now match implementation. |

The commit sequence is:

1. `e3e4378` — watch accounting, bounded streak promotion, snapshot reuse,
   Drop-batch continuation, prediction parity, and dead client cleanup.
2. `01c01a3` — certificate-verified IRC TLS and shared release documentation.

## High-risk transport analysis

### IRC credential path

`tm-app::chat` is the only production caller of
`tm_irc::ChatClient::connect_and_run`. The baseline opened a plain TCP stream to
port 6667 and then passed it to `run_stream`, which writes `PASS oauth:...`.
History traces that behavior to the initial parity implementation rather than
to a prior security requirement.

The candidate still opens TCP first, but it must complete a
`TlsConnector::connect` using the owned Twitch server name and the WebPKI root
store before `run_stream` can write registration data. Certificate or hostname
failure returns an I/O error to the existing supervised reconnect loop. There
is no custom verifier, plaintext retry, certificate bypass, or alternate token
write path.

**Attacker model:** an on-path network attacker can delay, reset, redirect, or
modify traffic, but does not control a WebPKI-trusted certificate for
`irc.chat.twitch.tv`.

**Adversarial result:** redirection to a server without a valid Twitch
certificate fails before OAuth registration. A forged certificate also fails.
Reset and timeout behavior remains a recoverable connection failure. A
compromised public root remains an external PKI trust limitation shared with
the miner's other verified HTTPS and WebSocket transports.

The implementation compiled under strict no-panic/no-unwrap Clippy and
completed a credential-free certificate-verified TLS 1.2 handshake to Twitch
port 6697 with verification code zero. Live authenticated IRC remains part of
the guarded operational observation rather than a unit test.

## State and algorithm analysis

### Slot-release accounting

The minute watcher compares the previous and current selected channel-ID sets
after metadata refresh and selection. Each released ID is sent through the
existing bounded single-writer actor before the next watch pass. Renames cannot
reset the wrong channel because identity is keyed by stable channel ID. A
temporary snapshot failure does not fabricate a selection transition.

The actor clears only `minute_watched` and `last_minute_update`; other stream,
campaign, streak, points, and prediction state is unchanged. Tests exercise the
set difference and prove that resetting one channel leaves another channel's
watch progress intact.

### Bounded streak promotion

The queue remains the authority for fair rotation. A streak candidate outside
the active rotating slots can move to the front at most once for its broadcast,
and global promotion is limited to once per 15-minute rotation window.
Campaign pinning is computed first and retains its reserved slot. Tests prove
immediate bounded promotion, suppression of repeated jumps, later promotion of
another candidate, and eventual service of ordinary channels.

The 30-minute restart condition uses the last observed offline transition.
Twitch's current Watch Streak help documents at least a 30-minute break between
streams; the implementation does not infer an end time across a process
restart.

### Snapshot reuse

Selection metadata refresh remains the single source of live broadcast ID,
title, game name, game ID, viewers, tags, and creation time. The minute sender
requires a non-empty refreshed broadcast ID and builds the same typed payload
from that snapshot. Rename/offline recovery moved to the refresh boundary,
where the authoritative request now occurs.

The focused mock now observes five Twitch requests and explicitly rejects the
previous extra `VideoPlayerStreamInfoOverlayChannel` operation. Before
deployment, the current image recorded 48 watch-request/minute-watch failures
over 72 hours; that measured baseline will be compared with the candidate
without attributing unrelated failures or repeating the earlier projected
three-times-volume claim.

### Mutations and prediction parity

Drop claims remain single-attempt mutations. A failed claim is collected and
later claimable drops are attempted once; successful claims alone update health
claim counters and notifications. The caller still receives one
`inventory-or-claim` failure after the batch.

Prediction selection updates the stored maximum only on a strictly greater
score/key, so equality retains index zero. Unit coverage spans all affected
strategies, while the shared JSON vector is executed by both Rust and the
pinned Go baseline.

## Test coverage and quality evidence

Completed on the clean candidate revision:

- format, strict all-target/all-feature Clippy, and production
  no-panic/no-unwrap Clippy;
- all locked workspace tests and warning-free rustdoc;
- architecture, documentation, release-hygiene, and pinned Go parity gates;
- RustSec audit with no vulnerability finding and duplicate-dependency review;
- deterministic byte-identical release builds;
- clean same-host Go/Rust comparison with identical decisions, operation
  checksum, and all-decision semantic checksum;
- sanitized mixed-runtime replay;
- credential-free verified TLS handshake to the production IRC endpoint.

Windows built both fuzz targets but could not execute the sanitizer runtime
(`STATUS_DLL_NOT_FOUND`). The exact pinned Linux ASan fuzz jobs, branch
coverage, and mutation shards are therefore blocking PR gates, not inferred
passes.

## Historical and regression analysis

Git history does not show a removed security check being reintroduced. Port
6667 dates to the original parity implementation. Snapshot refresh and watch
rotation were introduced as mining features and retain their existing
fail-closed metadata and fair-queue boundaries. Removed Twitch client methods
have no callers in production or tests, and every crate is non-published with
path-only workspace dependencies.

No new public configuration option, schema version, persisted secret,
telemetry, unsafe block, authentication fallback, automatic replay of uncertain
mutations, or dependency direction is introduced.

## Remaining release gates

- Pass exact-head CI and manually dispatched Deep Quality on Linux.
- Resolve every review thread and merge only the accepted head.
- Verify the exact post-merge three-platform manifest, embedded revision,
  smoke tests, SBOM, and provenance.
- Run the exclusive Pi canary and guarded immutable deployment with preserved
  rollback material.
- Establish a fresh post-SIGTERM strict-health baseline and complete the
  non-backdated 72-hour acceptance window.
- Compare candidate watch-request warnings and point/campaign behavior against
  the measured old-image baseline.

## Methodology and confidence

This was a deep review because the scope was under 20 production/test modules
plus documentation. Every changed production hunk and its baseline was read;
callers, state flows, removed methods, history, invariants, trust boundaries,
tests, and adversarial transport scenarios were traced. All high-risk code and
one-hop dependencies were reviewed.

Confidence is high for source behavior and local deterministic evidence.
Deployment confidence remains conditional on the exact Linux and live-image
gates above.
