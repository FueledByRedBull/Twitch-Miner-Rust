# Behavior parity

This compares the Rust miner with the Go port it is checked against, pinned at
commit `91f00698314d`. It is a behavior-level comparison, not a claim that
Twitch's undocumented contracts never change. Parity is established by running
shared test vectors against both implementations; Rust fixture, integration,
and deterministic parser-regression tests run in CI, and the dedicated-account
`--canary` closes the remaining live read-contract gap before each release (see
the [protocol inventory](protocol-inventory.md#release-canary-and-hash-probe)).

The external Go implementation is fetched separately and is not vendored into
this repository or its offline source bundle. Project-authored compatibility
harnesses under `tests/parity/go/` are tracked here; no Go source is copied into
published container images.

| Go behavior | Rust status | Evidence / limit |
| --- | --- | --- |
| Device-code login and session persistence | Parity | Current and legacy cookie fixtures; private atomic writes and backup. |
| Explicit streamers, followers, exclusions, and priority lists | Parity | Config/runtime fixtures and orchestration tests. |
| Channel-points context, bonus chest, streaks, and minute watching | Extended parity | Typed context, credit eligibility, fair rotation, bounded transient HLS retries, uncached playback priming, and broadcast-bound streak recovery are fixture-tested. Exact contracts and limits are normative in the [protocol inventory](protocol-inventory.md). |
| Drops and moments | Improved | Drop progress, campaign selection, and claims have independent controls and typed fixture coverage. Live evidence includes 14 progress/claim pairs; it does not claim exact campaign pin/unpin telemetry. |
| Predictions and betting strategies | Parity | Domain decision and runtime-effect tests, including an explicit first-outcome tie contract shared with Go/Python, deterministic coverage of the application-injected `1..=5` stealth amount offset, Twitch's documented `10`-to-`250000` per-viewer stake bounds, a typed rejection when `makePrediction.error` is present (as Python and Java read it; the fixture is synthetic), and PubSub pending-state updates followed by terminal viewer results. |
| Community goals and contributions | Parity | GQL/PubSub fixtures and contribution tests. |
| EventSub presence, PubSub viewer compatibility, IRC presence, and chat mentions | Improved | Typed, independently supervised transports with bounded reconnect, handoff, dedupe, and polling fallback. Like Go and Python, PubSub also subscribes each channel's `video-playback-by-id` presence topic; it is listed last, so the 500-topic limit drops it first. EventSub observes the raid lifecycle while PubSub supplies the legacy raid ID; live evidence records 20 successful raid mutations and 19 matching rewards within 15 minutes, and the unmatched observation is not called a mutation failure. This validates the hybrid boundary rather than an EventSub-only viewer contract. |
| Discord notifications and anonymized logging | Parity | Event filtering, redaction, and payload tests. Discord is the sole built-in notifier; see the [architecture](architecture.md). |
| Log persistence | Improved | Size rotation, bounded archives, and 30-day archive pruning. |
| Runtime supervision and health | Improved | Task-exit/panic supervision, separate activity/success freshness, and bounded recovery are status-tested. |
| Docker amd64 and arm64 | Supported | One published manifest is verified for both child platforms, attestations, embedded revision, and smoke behavior. ARMv7 is not supported. |
| Automatic updater | Deliberately removed | Legacy `auto_update=true` is rejected; no dormant binary replacement code remains. |
| Config mutation | Improved | Versioned preview, atomic write, and rollback backup. How each Go-era field is handled is in the [migration guide](migration.md#go-era-settings). |

Configuration validation is a separate robustness property, not a parity
reclassification: unsupported enum-like values are rejected with their exact
JSON paths before runtime or write-back (see the
[configuration reference](configuration.md)), while currently supported aliases
and empty-list defaults remain compatible. This does not change prediction
strategy or filter behavior, which remains `Parity` above.

## Known differences

Rust's transition diagnostics report `stream_up_at`, the time this miner
observed the current broadcast online, rather than Go's Twitch-sourced
`createdAt`. Displayed timestamps use the configured timezone, including its
daylight-saving offset, matching log headers. The same online/offline message is
sent to Discord when that notifier is enabled; privacy anonymization suppresses
exact streak timestamps before either destination.

Go defines but never issues five operations: `PlaybackAccessToken`,
`ModViewChannelQuery`, `ViewerDropsDashboard`, `DropCampaignDetails`, and
`PersonalSections`. Rust uses the first while mining and the third only in the
release canary, both as typed read-only contracts. The remaining three are not part of either miner's exercised runtime
behavior and are intentionally not copied into Rust.

Streak prioritization, watch rotation, and campaign selection follow Twitch's
current platform rules rather than parent-miner lineage; they are specified in
the [protocol inventory](protocol-inventory.md#watch-selection).

## Go baseline check

The normalized vectors in `tests/parity/vectors.json` cover common streamer
settings, prediction decisions and settlements, point-history updates, watch
selection, and a legacy PubSub point event. Rust runs them through its contract
tests. `scripts/verify-go-baseline.ps1` runs the same vectors against a checkout
of [0x8fv/Twitch-Channel-Points-Miner](https://github.com/0x8fv/Twitch-Channel-Points-Miner)
at commit `91f00698314dbbdd6c757d7b525458c82173e622` (CI clones it into
`Twitch-Channel-Points-Miner/`):

```powershell
./scripts/verify-go-baseline.ps1 -GoRoot <path-to-go-checkout>
```

The script temporarily copies the project-authored Go harness into the pinned
checkout's packages so it can exercise internal behavior, runs `go test ./...`,
and removes the generated files before returning. It fails if either
implementation diverges, and it reads no credentials or live Twitch data. This
is a behavioral test boundary, not a claim about legal independence or license
compatibility.

The gate also compares every Go persisted-operation hash with Rust. It accounts
for the five operations Go never issues and requires exactly two documented
hash differences: Rust carries Twitch's current `PlaybackAccessToken` hash while
the Go baseline retains the retired hash for an operation it does not issue, and
Rust's `Inventory` contract includes typed `requiredSubs` data so
subscription-only Drop campaigns cannot occupy a watch slot. The pinned Go
`TestStreamWatchProgress` uses an unstable exact two-minute boundary, so the gate
skips only that assertion and injects a deterministic equivalent covering
continuous progress at 90 seconds and reset behavior after 121 seconds.

The pin is held deliberately rather than tracked. As of 2026-08-20 the reviewed
upstream head `62eb343` still fails `go test` on a malformed nested declaration
in `TwitchChannelPointsMiner/miner_test.go`, and its history has diverged from
the pin, so the baseline cannot be advanced mechanically. An informational
comparison against that head shows the persisted-operation surface changed only
by adding `ClipsCards__User` and `FilterableVideoTower_Videos`, with no removals
and no hash rotations; both hashes already match this project's
implementations. Advancing to the first green upstream revision would therefore
remove the two documented Rust-only exceptions rather than add new ones. Broken
upstream tests are never made an enforced gate here.
