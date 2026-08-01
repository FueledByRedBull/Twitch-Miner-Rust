# Changelog

## Unreleased

- Logs the sanitized concrete `EventSub` failure beside its stable error class
  and classifies a missed keepalive deadline as `keepalive-timeout` instead of
  conflating normal liveness recovery with a rejected protocol payload.
- Reports a partially failed context-refresh cycle to task health only after
  all bounded child refreshes finish. Successful sibling updates remain
  applied, and the successful-refresh counter advances only for an entirely
  successful cycle.
- Uses a ten-minute live streak-priority budget derived from the current
  production sample's seven-minute p90 plus a bounded three-minute margin.
  The documented 30-minute inter-broadcast rule, one promotion per broadcast,
  campaign pinning, slot release, and fair-rotation ceiling remain unchanged.
- Restores prediction stealth amount variation as one application-selected
  integer offset in `1..=5`. The pure decision path accepts the offset as an
  input so tests remain deterministic, and minimum, balance, maximum, and
  top-predictor bounds remain enforced. This is amount variation, not an
  anti-detection guarantee.
- Selects the lowest-bandwidth explicitly playable HLS video variant and a
  valid media segment from playlist structure instead of taking the final URI.
  Relative URL resolution and public-HTTPS endpoint validation remain
  mandatory.
- Keeps playback priming uncached after measuring the roughly 20-second
  selected-channel cadence and pinning the four-request preflight contract.
  No credited WATCH/WATCH_STREAK A/B evidence currently proves that reuse is
  safe.
- Decodes the primary `spade_url` as a JSON string while accepting harmless
  whitespace and escaping. Missing or malformed primary shapes fail clearly;
  no alternate extraction chain is added.
- Stops stale, zero, or backward wall-clock gaps from changing confirmed watch
  progress. The accepted interval is bounded by two worst-case two-slot
  scheduler passes; longer gaps move the anchor without crediting or erasing
  points already confirmed.
- Documents that Python pickle cookie jars are never inspected or converted.
  Operators migrate safely through Rust device login and preserve the old
  secret file as a private backup outside the active data mount.
- Holds the spade endpoint to the same origin rules as playback. The URL is
  extracted from an unconstrained field inside Twitch's settings script and the
  minute-watched payload carries account identifiers, so it must now parse as a
  public HTTPS origin (or loopback HTTP for tests) before any request is sent.
- Ignores `EventSub` message types and subscription types this build does not
  model, rather than treating them as protocol violations. An additive Twitch
  change previously dropped the socket, and because each reconnect re-derives
  capacity from the still-listed subscriptions of the dropped session, it could
  degrade into progressively smaller subscription sets. Payloads for
  subscription types the miner does act on still fail closed.
- Re-derives the active `EventSub` subscription count from Twitch after a
  session reconnect instead of reporting the previous session's numbers, and
  marks the resulting report verified.
- Decodes each `EventSub` frame once through a shared path, removing the
  duplicated text/binary handling in the welcome and listen loops, and parses
  each notification once instead of validating it twice.
- Classifies watch-selection metadata refresh failures as `metadata-refresh`
  on the `minute` task instead of leaving them as unclassified warnings, so a
  failing stream-info read stays visible to health and warning classification
  after the refresh moved out of the per-tick send path.
- Bounds a streak promotion to the broadcast that earned it. The record is
  released only by a different broadcast, never by a channel dropping out of
  the eligible set for a pass, and fair rotation reclaims the pass once
  promotions have deferred it for two rotation windows.
- Refreshes stream metadata inline once when the batched refresh has not landed
  or is older than five minutes, so a single failed refresh no longer fails
  every watch tick for that channel and no watch event is sent against
  metadata that may no longer describe the broadcast. Presence changes are
  detected by the batched refresh and the event transports rather than by every
  watch tick.
- Drops the unread `Stream.tags` field and builds the minute-watched payload
  directly from the runtime stream record.
- Resets credited watch progress through the runtime actor when a channel loses
  a watch slot, and gives a newly eligible streak channel at most one bounded
  promotion per broadcast and 15-minute rotation window. Fair rotation and
  Drop-campaign pinning remain intact.
- Enforces Twitch's documented 30-minute break before prioritizing a restarted
  stream for streak recovery, and reuses the refreshed stream snapshot for
  minute-watch payloads instead of issuing a second stream-info request on
  every watch tick.
- Makes prediction ties select the first outcome, matching the Go and Python
  parents, and continues a Drop claim batch after an individual mutation
  fails while retaining one aggregate task failure.
- Encrypts IRC authentication and chat traffic with certificate-verified TLS
  on port 6697. There is no plaintext or certificate-bypass fallback.
- Removes unused raw Twitch client facades and the empty updater directory,
  and corrects the persisted-operation and parity documentation.
- Decomposes EventSub connection handling, deterministic subscription planning,
  and protocol normalization; separates Twitch typed-response validation from
  HTTP execution and config setting composition from persistence/migration.
  Large private unit suites now live outside production source trees without
  exposing test hooks.
- Documents crate and public-interface invariants, shares one cohesive runtime
  effect context between EventSub and PubSub orchestration, and adds a
  dependency-free CI architecture gate for crate directions, transport-free
  domain logic, and production/test source separation. Consolidates duplicate
  container deployment guidance into the maintained operator and container
  guides.
- Matches channel-level Drop campaign IDs to typed inventory progress so a fully
  claimed campaign releases its watch pin instead of suppressing Twitch's spare
  points slot. New, incomplete, and temporarily unknown campaigns remain
  eligible.
- Replaces floating-point bet sizing with exact integer arithmetic, removes
  broad cast lint suppressions, and preserves integer prediction ordering above
  `f64` precision. Boundary tests cover extreme balances, percentages, and
  progress formatting.
- Adds reproducible Go/Rust and actor queue-capacity measurements. The measured
  queue differences are noise-scale, so the bounded capacity remains 64; the
  two-layer `scratch` runtime image is already near-minimal.
- Removes foreign console/avatar branding and stale internal review dumps while
  preserving license attribution. GitHub Actions now use Node 24-capable pins,
  secret scanning uses a checksum-verified native Gitleaks binary, and
  multiarch digest collection uses the native authenticated artifact API.
- Runs the guarded release canary only after a normal stop of the rollback
  service, preventing a fully utilized EventSub cost budget from rejecting the
  second session. Canary and deployment failures restore and verify rollback.
- Mines every eligible live channel through fair 15-minute turns in Twitch's two
  creditable watch slots. This preserves the platform's normal points/bonus rate
  while avoiding permanent priority starvation. An eligible Drop campaign now
  preempts immediately and pins its highest-ranked channel until completion or
  ineligibility. With `watch_one_stream_when_drops_active=false`, the second slot
  continues fair non-campaign rotation; when enabled, the existing explicit
  one-channel Drop policy remains unchanged.
- Restores the Python parent's live-playback preflight before minute-watch
  heartbeats: a typed read-only playback token, lowest-quality HLS playlist, and
  media-segment HEAD request. Direct Spade requests remain fail-closed when the
  playback chain is unavailable instead of reporting uncredited success.
- Retries read-only GQL requests when Twitch returns the observed HTTP-200
  envelope containing only fixed `service error` entries. Unknown or mixed GQL
  errors still fail closed, and mutations remain single-attempt.
- Keeps saved-session validation inside the process across transient network,
  rate-limit, and Twitch server failures with capped interruptible backoff, and
  separates task activity from task success so active recovery is reported as
  degraded without creating a container restart storm. Definitive auth
  rejection still enters device reauthorization, contract failures remain
  fail-closed, and silent or exited tasks remain fatal to supervision.
- Adds a versioned, bounded private streak cache, deterministic longest/expiring
  streak priorities, and opt-in 23.5-hour offline VOD/clip streak recovery with
  exact broadcast matching, live preemption, sanitized progress, and typed
  milestone confirmation. Archived-video edges with an unavailable null node
  are ignored individually, while every present node remains strictly typed.
- Adds periodic typed drop-progress console lines without additional inventory
  requests or raw campaign/drop identifiers.

- Refreshes the pinned GitHub Actions used for checkout, Rust, Go, artifacts,
  metadata, security scanning, and multi-architecture builds, and updates
  `regex`, `chrono`, `tokio`, `clap`, and `tokio-tungstenite`. These
  maintenance updates do not change the configuration schema.
- Recovers a tracked channel rename during a running session by resolving the
  current login from its stable channel ID, preserving the session's initial
  balance, retrying the watch request with the new identity, and temporarily
  releasing a slot when identity recovery cannot be completed safely.
- Replaces blanket short-restart streak suppression with explicit resolved
  state carryover across repeated sub-30-minute stream segments, uses a measured
  15-minute streak budget, and removes the unnecessary 30-second online delay.
- Separates drop farming from reward claiming, migrates legacy `claim_drops`
  choices without changing their effective behavior, optionally limits the
  watch set to one verified active campaign, and removes same-game watcher
  diversification that unnecessarily reduced channel-point throughput.
- Restores the independently supervised PubSub `/v1` compatibility transport
  for viewer prediction discovery/results, immediate points and bonus events,
  moments, raid IDs, and community goals while retaining EventSub as the
  preferred presence source.
- Adds per-streamer EventSub capacity/source reporting, bounded presence polling
  for overflow or outages, typed subscription create/list diagnostics, and
  per-topic-class PubSub LISTEN/message/reconnect health. Broadcaster EventSub
  predictions are selected only for the authenticated user's own channel when
  the validated token has a prediction read/manage scope; planning accounts for
  Twitch's current returned cost before creating any subscription. Create and
  list responses use their distinct documented pagination shapes.
- Adds bounded cross-transport mutation/prediction deduplication, a read-only
  dual-transport canary handshake, 2026 `stream.offline` fixture coverage, and
  normalized source-policy/capacity/batching parity vectors.
- Updates the pinned Go baseline to `91f00698314d`, adds its global
  `claim_moments` setting, and uses the same typed read-only `RewardList`
  contract to reconcile a completed watch streak on a still-live stream.
- Retains the 128 most recent prediction discovery IDs after active state is
  removed so bounded replays cannot schedule another mutation attempt.
- Validates viewer prediction IDs and result types, supports both observed
  prediction outcome counter shapes, and lets a late viewer result refine the
  final report without emitting a duplicate settlement notification.
- Keeps PubSub prediction updates connected through Twitch's intermediate
  `RESOLVE_PENDING` and `CANCEL_PENDING` states while restricting financial
  settlement to explicit terminal states.
- Routes periodic bonus discovery through the runtime's bounded claim-ID
  deduplication, preventing a context refresh and PubSub from submitting the
  same claim twice, and avoids replacing a fresh reward balance with a stale
  post-claim context response.
- Restores Python-style timestamp/level/operation log lines and adds a bounded,
  privacy-aware shutdown report with session metadata, completed prediction
  details, outcomes/results, and per-streamer point history.
- Adds task-aware runtime health checks, supervised task exit/panic handling,
  bounded reconnect backoff, and a privacy-safe support bundle.
- Counts fallback-presence health once per polling cycle rather than once per
  streamer, and starts device reauthorization only after a definitive saved-
  session rejection instead of after transient network/server failures.
- Keeps read-only canary mode free of directory/log writes and redacts IRC
  callback text plus local log paths when anonymized logging is enabled.
- Treats Twitch's null available-drops channel/list as an empty result, matching
  the Go miner while retaining strict campaign IDs for present entries.
- Restores the Python parent's campaign-aware `DROPS` watch priority: only a
  live, drop-farming-enabled channel with a validated active campaign is promoted,
  campaign state is invalidated on broadcast/game changes, and later configured
  priorities safely fill unused watch slots.
- Removes the dormant automatic updater and migrates/rejects legacy
  `auto_update` configuration safely.
- Adds atomic, backed-up config and cookie migration with a `--check-config`
  preview command.
- Adds build provenance, release-digest smoke tests, SBOM/provenance, pinned
  Actions, dependency/license/secret checks, coverage, and bounded parser
  fuzz-regression tests.
- Makes signed `v*` tags promote the already-built and canaried commit-SHA
  manifest instead of rebuilding it with fresh attestations. Release promotion
  verifies all platform revisions and attestations and fails unless the release
  tag resolves to the exact tested digest.
- Makes guarded deployment reject a healthy-looking stale status file from a
  prior same-revision session; acceptance now requires the current container's
  fresh session heartbeat plus complete task, EventSub, and PubSub recovery.
- Adds the read-only Twitch canary, protocol inventory, digest-pinned
  deployment/rollback instructions, migration guide, issue template, and
  contributor review checklist.

Release notes must identify the source revision, published image digest,
supported platforms, configuration migration impact, and rollback digest.
