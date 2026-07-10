# Behavior Parity And Release Limits

This is a behavior-level comparison against the adjacent Go implementation at
`940c98409e58`, not a claim that Twitch's undocumented contracts never change.
Rust fixture, integration, and deterministic parser-regression tests run in
CI. The dedicated-account `--canary` closes the remaining live read-contract
gap before each release.

| Go behavior | Rust status | Evidence / limit |
| --- | --- | --- |
| Device-code login and session persistence | Parity | Current and legacy cookie fixtures; private atomic writes and backup. |
| Explicit streamers, followers, exclusions, and priority lists | Parity | Config/runtime fixtures and orchestration tests. |
| Channel-points context, bonus chest, streaks, and minute watching | Parity | GQL fixtures, mocked watch flow, and Spade watch-request recovery tests for 401/429/5xx responses. Direct GQL calls are intentionally not retried because mutations share the client path. |
| Drops and moments | Parity | Inventory, campaign, claim-status, and PubSub fixtures. |
| Predictions and betting strategies | Parity | Domain decision and runtime-effect tests. |
| Community goals and contributions | Parity | GQL/PubSub fixtures and contribution tests. |
| Raids, PubSub, IRC presence, and chat mentions | Parity | Topic, reconnect, IRC transport, and event fixtures. |
| Discord notifications and anonymized logging | Parity | Event filtering, redaction, and payload tests. |
| Log persistence | Improved | Size rotation, bounded archives, and 30-day archive pruning. |
| Runtime supervision and health | Improved | Task-exit/panic supervision plus task-specific freshness/failure thresholds. |
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
| Drops, raid, community goals, chat presence, `disable_at_in_nickname` | Preserved. |
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

Before publication, run all fixture tests and the read-only canary. A successful
canary proves only the listed read operations for that account at that time;
mutations remain fixture-verified to avoid claiming rewards or placing bets
during release validation.
