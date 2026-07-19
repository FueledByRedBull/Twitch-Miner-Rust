# Changelog

## Unreleased

- Adds a versioned, bounded private streak cache, deterministic longest/expiring
  streak priorities, and opt-in 23.5-hour offline VOD/clip streak recovery with
  exact broadcast matching, live preemption, sanitized progress, and typed
  milestone confirmation.
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
