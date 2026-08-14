# Behavior Parity And Release Limits

This is a behavior-level comparison against an explicit clean Go checkout at
`91f00698314d`, not a claim that Twitch's undocumented contracts never change.
The pinned checkout is a deterministic comparison baseline only: parity is
established by running shared test vectors against both implementations, and no
Go source is copied into or redistributed with this project. Rust fixture,
integration, and deterministic parser-regression tests run in CI. The
dedicated-account `--canary` closes the remaining live read-contract gap before
each release.

| Go behavior | Rust status | Evidence / limit |
| --- | --- | --- |
| Device-code login and session persistence | Parity | Current and legacy cookie fixtures; private atomic writes and backup. |
| Explicit streamers, followers, exclusions, and priority lists | Parity | Config/runtime fixtures and orchestration tests. |
| Channel-points context, bonus chest, streaks, and minute watching | Extended parity | Typed context, credit eligibility, fair rotation, uncached playback priming, and confirmed streak recovery are fixture-tested. Exact contracts and limits are normative in the [protocol inventory](../protocol-inventory.md). |
| Drops and moments | Improved | Drop progress, campaign selection, and claims have independent controls and typed fixture coverage. Live evidence includes 14 progress/claim pairs; it does not claim exact campaign pin/unpin telemetry. |
| Predictions and betting strategies | Parity | Domain decision and runtime-effect tests, including an explicit first-outcome tie contract shared with Go/Python, deterministic coverage of the application-injected `1..=5` stealth amount offset, Twitch's documented `10`-to-`250000` per-viewer stake bounds, and PubSub pending-state updates followed by terminal viewer results. |
| Community goals and contributions | Parity | GQL/PubSub fixtures and contribution tests. |
| EventSub presence, PubSub viewer compatibility, IRC presence, and chat mentions | Improved | Typed, independently supervised transports with bounded reconnect, handoff, dedupe, and polling fallback. Endpoint, authorization, and timeout policy is defined in the [protocol inventory](../protocol-inventory.md). |
| Discord notifications and anonymized logging | Parity | Event filtering, redaction, and payload tests. Discord is the sole built-in outbound notifier. Generic notifier backends and analytics exporters are out of scope unless a concrete operator requirement justifies them. |
| Log persistence | Improved | Size rotation, bounded archives, and 30-day archive pruning. |
| Runtime supervision and health | Improved | Task-exit/panic supervision, separate activity/success freshness, and bounded recovery are status-tested. |
| Docker amd64, arm64, arm/v7 | Supported | One published manifest is verified for all three child platforms, attestations, embedded revision, and smoke behavior. |
| Automatic updater | Deliberately removed | Legacy `auto_update=true` is rejected; no dormant binary replacement code remains. |
| Config mutation | Improved | Versioned preview, atomic write, and rollback backup. |

Configuration validation is a separate robustness property, not a parity
reclassification. Unsupported enum-like values in global or per-streamer
strategy, delay mode, filter condition, chat presence, and watch-priority
settings are rejected with their exact JSON paths before runtime or write-back;
currently supported aliases and empty-list defaults remain compatible. This
hardening does not change prediction strategy or filter behavior, which remains
`Parity` above.

Twitch's current [Watch Streaks requirements](https://help.twitch.tv/s/article/recover-watch-streaks)
state that at least 30 minutes must pass between the end of one stream and the
start of the next. That platform rule, rather than parent-miner lineage, is the
contract used by streak prioritization.

## Configuration compatibility

All Go-era operational fields remain accepted unless they were unsafe or had no
working Rust implementation:

| Field group | Rust handling |
| --- | --- |
| Username, streamers, follower/game/watch selection | Preserved. |
| Logging, emojis, timestamps, console username, privacy, Discord | Preserved. |
| Drops, moments (`claim_moments` globally and per streamer), community goals, chat presence, `disable_at_in_nickname` | Improved; global and per-streamer controls are preserved. See the protocol inventory for campaign and mutation rules. |
| Raid observation and auto-join | Preserved with compatibility boundary. EventSub observes lifecycle; PubSub supplies the legacy raid ID. Live evidence records 20 successful mutations and 19 matching rewards within 15 minutes; the unmatched observation is not called a mutation failure. |
| Prediction and per-streamer override settings | Preserved. |
| `password` | Empty values are migrated away; non-empty or malformed values are rejected. |
| `disable_ssl_cert_verification` | Legacy `false` is migrated away; `true` or malformed values are rejected and TLS remains mandatory. |
| `watch_queue_logging` | Migrated away because production never consumed it. |
| `auto_update` | `false` is migrated away; `true` is rejected. |
| `watch_streak_warm_start_cache` | The Go boolean is migrated away; Rust always manages its bounded streak cache internally. |
| `betting.make_predictions` wrapper | Migrated to `betting(make_predictions)`; matching duplicates are removed, while conflicts or malformed wrappers are rejected without write-back. |
| `watch_streams` marker | Legacy `true` is migrated away because running the miner means watching; `false` or non-boolean values are rejected rather than changing operator intent. |
| Unknown configuration keys | Rejected with an exact root/nested JSON path after recognized legacy migrations; dynamic streamer-login map keys remain valid. |
| `config_schema_version` | Added by migration; future versions are rejected without write-back. |

## Contract evidence

The authoritative operation and transport contracts are in the
[protocol inventory](../protocol-inventory.md).
Go defines but never issues five operations: `PlaybackAccessToken`,
`ModViewChannelQuery`, `ViewerDropsDashboard`, `DropCampaignDetails`, and
`PersonalSections`. Rust exercises the first and third as typed read-only
contracts. The remaining three are not part of either miner's exercised runtime
behavior and are intentionally not copied into Rust.

The normalized cross-process vectors in `tests/parity/vectors.json` are run by
the Rust contract tests and by the pinned Go baseline through
`scripts/verify-go-baseline.ps1`. The Go harness is copied only for the duration
of that command and is removed afterward. The pinned Go baseline's
`TestStreamWatchProgress` uses an unstable exact two-minute boundary, so the
gate skips only that assertion and injects a deterministic equivalent covering
continuous progress at 90 seconds and reset behavior after 121 seconds.

Before publication, run all fixture tests and the read-only canary. A successful
canary proves only the listed read operations for that account at that time;
mutations remain fixture-verified to avoid claiming rewards or placing bets
during release validation.

The read-only canary also requires EventSub setup/list verification and a
PubSub LISTEN acknowledgement for every configured compatibility topic. It
never applies received transport events to runtime state.
