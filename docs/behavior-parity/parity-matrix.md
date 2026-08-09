# Behavior Parity And Release Limits

This is a behavior-level comparison against an explicit clean Go checkout at
`91f00698314d`, not a claim that Twitch's undocumented contracts never change.
Rust fixture, integration, and deterministic parser-regression tests run in
CI. The dedicated-account `--canary` closes the remaining live read-contract
gap before each release.

| Go behavior | Rust status | Evidence / limit |
| --- | --- | --- |
| Device-code login and session persistence | Parity | Current and legacy cookie fixtures; private atomic writes and backup. |
| Explicit streamers, followers, exclusions, and priority lists | Parity | Config/runtime fixtures and orchestration tests. |
| Channel-points context, bonus chest, streaks, and minute watching | Extended parity | Typed context and `RewardList` fixtures, a private bounded warm cache for null-milestone restarts, deterministic longest/expiring streak priorities, resolved/unresolved short-restart carryover, Twitch's documented 30-minute inter-broadcast eligibility rule, an evidence-based ten-minute live budget, one bounded streak jump per broadcast that only a different broadcast can release, a fair-rotation ceiling of two rotation windows so promotions cannot defer rotation indefinitely, watch-progress reset when a channel loses its credited slot, and a two-pass scheduler-envelope cap that prevents stale or backward gaps from inflating confirmed progress. Snapshot reuse for minute-watched payloads retains one bounded inline refresh when the snapshot is missing or older than five minutes; semantic HLS playback priming remains uncached until credited-reward A/B evidence supports reuse. Partial context refreshes retain successful siblings while reporting one task-health failure. Playback acceptance is distinct from typed recovery confirmation. Read-only GQL requests use bounded header-aware retries; mutations remain single-attempt. |
| Drops and moments | Improved | Inventory, campaign, claim-status, and PubSub fixtures. Drop progress and claim mutations have independent global/per-streamer controls; a verified campaign can limit the watch set to one deterministic streamer so Twitch progress is not split. Inventory and per-channel campaign IDs are matched so a fully claimed campaign releases its pin, while new/incomplete/unknown campaigns remain eligible. One failed drop claim is reported after the remaining claimable batch has been attempted. The completed live release evidence contains 14 Drop progress/claim pairs. That evidence validates the live claim path; it does not claim an exact campaign pin/unpin telemetry trace, which remains covered by the typed inventory/campaign fixtures. Legacy `claim_drops` configurations migrate without changing their prior effective behavior. |
| Predictions and betting strategies | Parity | Domain decision and runtime-effect tests, including an explicit first-outcome tie contract shared with Go/Python, deterministic coverage of the application-injected `1..=5` stealth amount offset and all amount bounds, and PubSub pending-state updates followed by terminal viewer results. |
| Community goals and contributions | Parity | GQL/PubSub fixtures and contribution tests. |
| EventSub presence, PubSub viewer compatibility, IRC presence, and chat mentions | Improved | EventSub welcome/keepalive/reconnect/revocation/capacity tests, independently supervised PubSub `/v1` LISTEN/PING/PONG tests, transport-neutral runtime events, bounded dedupe, and verified Rustls/WebPKI IRC on port 6697. A missed EventSub keepalive has its own health class and reconnect warnings retain the sanitized concrete cause. Unmodelled EventSub message and subscription types are ignored instead of dropping the socket, while payloads for modelled types still fail closed, and a session inherited through a reconnect re-derives its verified active count from Twitch. EventSub predictions are selected only when the tracked channel ID matches the authenticated broadcaster and the validated token has a prediction read/manage scope; ordinary viewer discovery/confirmation remains on PubSub compatibility. |
| Discord notifications and anonymized logging | Parity | Event filtering, redaction, and payload tests. Discord is the sole built-in outbound notifier. Generic notifier backends and analytics exporters are out of scope unless a concrete operator requirement justifies them. |
| Log persistence | Improved | Size rotation, bounded archives, and 30-day archive pruning. |
| Runtime supervision and health | Improved | Task-exit/panic supervision plus separate success/activity freshness and failure thresholds; active bounded recovery remains degraded without restarting the whole miner, silent tasks remain fatal, saved-session validation retries transient startup failures in-process, and batched presence polling records at most one task failure per cycle. |
| Docker amd64, arm64, arm/v7 | Supported | Per-platform digest and post-manifest smoke tests in release CI. |
| Automatic updater | Deliberately removed | Legacy `auto_update=true` is rejected; no dormant binary replacement code remains. |
| Config mutation | Improved | Versioned preview, atomic write, and rollback backup. |

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
| Drops, moments (`claim_moments` globally and per streamer), community goals, chat presence, `disable_at_in_nickname` | Improved. Drop farming is independently configurable with `farm_drops`. The highest-ranked eligible campaign preempts immediately and stays pinned until its typed inventory is fully claimed, it becomes ineligible, or it goes offline. Completed inventory IDs cannot retain a stale pin. `watch_one_stream_when_drops_active=true` retains Twitch's strict one-channel Drop mode; when false, the other watch slot rotates fairly through non-campaign channels, and both slots resume 15-minute fair rotation when no campaign is active. Both settings support per-streamer overrides. |
| Raid observation and auto-join | Preserved with compatibility boundary | EventSub observes the raid lifecycle; PubSub compatibility supplies the legacy raid ID required by the typed single-attempt `JoinRaid` mutation. Repeated raid IDs are ignored. Live release evidence records 20 successful post-mutation `update_raid` logs; 19 had a matching RAID reward within 15 minutes. The remaining mutation had no reward observed in that window and is retained as an observation, not counted as a mutation failure. |
| Prediction and per-streamer override settings | Preserved. |
| `password` | Rejected when non-empty; device login does not need it. |
| `disable_ssl_cert_verification` | Rejected when true; TLS verification is mandatory. |
| `auto_update` | `false` is migrated away; `true` is rejected. |
| `watch_streak_warm_start_cache` | The Go boolean is migrated away; Rust always manages its bounded streak cache internally. |
| `betting.make_predictions` wrapper | Migrated to `betting(make_predictions)`; matching duplicates are removed, while conflicts or malformed wrappers are rejected without write-back. |
| `watch_streams` marker | Legacy `true` is migrated away because running the miner means watching; `false` or non-boolean values are rejected rather than changing operator intent. |
| Unknown configuration keys | Rejected with an exact root/nested JSON path after recognized legacy migrations; dynamic streamer-login map keys remain valid. |
| `config_schema_version` | Added by migration; future versions are rejected without write-back. |

## Contract evidence

The persisted-operation names/hashes that Rust actively uses are captured from
the Go source and checked against Rust builders in [protocol-inventory.md](../protocol-inventory.md).
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
