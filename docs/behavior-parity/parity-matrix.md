# Behavior Parity And Release Limits

This is a behavior-level comparison against the adjacent Go implementation at
`91f00698314d`, not a claim that Twitch's undocumented contracts never change.
Rust fixture, integration, and deterministic parser-regression tests run in
CI. The dedicated-account `--canary` closes the remaining live read-contract
gap before each release.

| Go behavior | Rust status | Evidence / limit |
| --- | --- | --- |
| Device-code login and session persistence | Parity | Current and legacy cookie fixtures; private atomic writes and backup. |
| Explicit streamers, followers, exclusions, and priority lists | Parity | Config/runtime fixtures and orchestration tests. |
| Channel-points context, bonus chest, streaks, and minute watching | Extended parity | Typed context and `RewardList` fixtures, a private bounded warm cache for null-milestone restarts, deterministic longest/expiring streak priorities, resolved/unresolved short-restart carryover, a fixed 15-minute live budget, channel-rename recovery by stable ID, and opt-in single-worker VOD/clip recovery with live preemption. Playback acceptance is distinct from typed recovery confirmation. Read-only GQL requests use bounded header-aware retries; mutations remain single-attempt. |
| Drops and moments | Improved | Inventory, campaign, claim-status, and PubSub fixtures. Drop progress and claim mutations have independent global/per-streamer controls; a verified campaign can limit the watch set to one deterministic streamer so Twitch progress is not split. Legacy `claim_drops` configurations migrate without changing their prior effective behavior. |
| Predictions and betting strategies | Parity | Domain decision and runtime-effect tests, including PubSub pending-state updates followed by terminal viewer results. |
| Community goals and contributions | Parity | GQL/PubSub fixtures and contribution tests. |
| EventSub presence, PubSub viewer compatibility, IRC presence, and chat mentions | Improved | EventSub welcome/keepalive/reconnect/revocation/capacity tests, independently supervised PubSub `/v1` LISTEN/PING/PONG tests, transport-neutral runtime events, bounded dedupe, and IRC tests. EventSub predictions are selected only when the tracked channel ID matches the authenticated broadcaster and the validated token has a prediction read/manage scope; ordinary viewer discovery/confirmation remains on PubSub compatibility. |
| Discord notifications and anonymized logging | Parity | Event filtering, redaction, and payload tests. |
| Log persistence | Improved | Size rotation, bounded archives, and 30-day archive pruning. |
| Runtime supervision and health | Improved | Task-exit/panic supervision plus separate success/activity freshness and failure thresholds; active bounded recovery remains degraded without restarting the whole miner, silent tasks remain fatal, saved-session validation retries transient startup failures in-process, and batched presence polling records at most one task failure per cycle. |
| Docker amd64, arm64, arm/v7 | Supported | Per-platform digest and post-manifest smoke tests in release CI. |
| Automatic updater | Deliberately removed | Legacy `auto_update=true` is rejected; no dormant binary replacement code remains. |
| Config mutation | Improved | Versioned preview, atomic write, and rollback backup. |

## Configuration compatibility

All Go-era operational fields remain accepted unless they were unsafe or had no
working Rust implementation:

| Field group | Rust handling |
| --- | --- |
| Username, streamers, follower/game/watch selection | Preserved. |
| Logging, emojis, timestamps, console username, privacy, Discord | Preserved. |
| Drops, moments (`claim_moments` globally and per streamer), community goals, chat presence, `disable_at_in_nickname` | Preserved. Drop farming is now independently configurable with `farm_drops`; `watch_one_stream_when_drops_active` defaults to true. Both support per-streamer overrides. |
| Raid observation and auto-join | Preserved with compatibility risk | EventSub observes the raid lifecycle; PubSub compatibility supplies the legacy raid ID required by the typed single-attempt `JoinRaid` mutation. Repeated raid IDs are ignored. Live acceptance is still required before release. |
| Prediction and per-streamer override settings | Preserved. |
| `password` | Rejected when non-empty; device login does not need it. |
| `disable_ssl_cert_verification` | Rejected when true; TLS verification is mandatory. |
| `auto_update` | `false` is migrated away; `true` is rejected. |
| `config_schema_version` | Added by migration; future versions are rejected without write-back. |

## Contract evidence

The persisted-operation names/hashes that Rust actively uses are captured from
the Go source and checked against Rust builders in [protocol-inventory.md](../protocol-inventory.md).
Go contains a few unused operation definitions (`PlaybackAccessToken`,
`ModViewChannelQuery`, `DropCampaignDetails`, and `PersonalSections`); they are
not part of either miner's exercised runtime behavior and are intentionally not
copied into Rust.

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
