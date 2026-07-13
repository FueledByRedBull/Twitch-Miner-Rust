# Changelog

## Unreleased

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
- Restores Python-style timestamp/level/operation log lines and adds a bounded,
  privacy-aware shutdown report with session metadata, completed prediction
  details, outcomes/results, and per-streamer point history.
- Adds task-aware runtime health checks, supervised task exit/panic handling,
  bounded reconnect backoff, and a privacy-safe support bundle.
- Keeps read-only canary mode free of directory/log writes and redacts IRC
  callback text plus local log paths when anonymized logging is enabled.
- Treats Twitch's null available-drops channel/list as an empty result, matching
  the Go miner while retaining strict campaign IDs for present entries.
- Removes the dormant automatic updater and migrates/rejects legacy
  `auto_update` configuration safely.
- Adds atomic, backed-up config and cookie migration with a `--check-config`
  preview command.
- Adds build provenance, release-digest smoke tests, SBOM/provenance, pinned
  Actions, dependency/license/secret checks, coverage, and bounded parser
  fuzz-regression tests.
- Adds the read-only Twitch canary, protocol inventory, digest-pinned
  deployment/rollback instructions, migration guide, issue template, and
  contributor review checklist.

Release notes must identify the source revision, published image digest,
supported platforms, configuration migration impact, and rollback digest.
