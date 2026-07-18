# Twitch Protocol Inventory

The Rust client keeps persisted-operation names and SHA-256 hashes in
`tm-twitch::PERSISTED_OPERATION_CONTRACTS`. A unit test verifies that every
builder uses an inventoried, unique contract. The comparison source is the Go
implementation at commit `91f00698314d`; the hashes below match the operations
actually used there.

When both repositories are available, run the baseline tests and the explicit
comparison gate from the Rust workspace:

```powershell
./scripts/verify-go-baseline.ps1 -GoRoot ../Twitch-Channel-Points-Miner
```

The gate runs `go test ./...`, compares every Go persisted-operation hash with
Rust, and permits only the four documented Go definitions that neither miner
exercises.

| Operation | Mode |
| --- | --- |
| `GetIDFromLogin` | Read-only |
| `ChannelFollows` | Read-only |
| `ChannelPointsContext` | Read-only |
| `WithIsStreamLiveQuery` | Read-only |
| `VideoPlayerStreamInfoOverlayChannel` | Read-only |
| `RewardList` | Read-only |
| `Inventory` | Read-only |
| `ViewerDropsDashboard` | Read-only |
| `DropsHighlightService_AvailableDrops` | Read-only |
| `UserPointsContribution` | Read-only |
| `ClaimCommunityPoints` | Mutation |
| `CommunityMomentCallout_Claim` | Mutation |
| `JoinRaid` | Mutation |
| `MakePrediction` | Mutation |
| `DropsPage_ClaimDropRewards` | Mutation |
| `ContributeCommunityPointsCommunityGoal` | Mutation |

Twitch can replace undocumented persisted-query contracts at any time. Before
each release, run the credential-safe canary on a dedicated account:

```sh
twitch-miner --data-dir /data --canary
```

The canary validates an existing session and performs only the read-only
operations listed above. It does not start workers, claim a reward, make a
prediction, join a raid, contribute points, mutate cookies, or send Discord
notifications. Record the source revision, image digest, date, and success or
failure class in the release notes; never record cookies, account IDs, raw
payloads, or request headers.

Mutation contracts are verified with sanitized fixtures and response-validation
tests. Read-only requests are bounded and header-aware; mutations are never
automatically replayed after an uncertain response. A release with a changed
operation hash must add its sanitized fixture, update this inventory, and pass
the canary before publication.

The preferred EventSub WebSocket path handles stream presence and observes
raids. Broadcaster prediction subscriptions are requested only when a tracked
channel ID exactly matches the authenticated user ID and the validated token
actually contains `channel:read:predictions` or
`channel:manage:predictions`; ordinary viewer tokens cannot authorize them for
arbitrary tracked channels. Other channels remain on PubSub compatibility.
EventSub creation and list responses use separate typed envelopes because only
the list response contains pagination. Both are capacity-planned; overflow or
failed presence capabilities use bounded GQL polling instead of silently
dropping channels. The WebSocket requests Twitch's supported 30-second
keepalive window and applies a five-second delivery grace before reconnecting,
avoiding an edge race at the advertised silence boundary.

The isolated PubSub compatibility path connects to
`wss://pubsub-edge.twitch.tv/v1`. It supplies viewer prediction discovery and
result events, immediate point/bonus events, moment IDs, raid IDs, and
community-goal changes. It is unofficial/deprecated, so LISTEN acknowledgement,
message time, reconnect count, and fixed failure class are exposed separately
from EventSub status. User topics alone receive the auth token, connections are
limited to 50 topics, and failures cannot stop EventSub, polling, IRC, or drops.
Both paths normalize into `tm-events`; mutation IDs, point-event state, and
prediction event IDs are boundedly deduplicated before effects are scheduled. GQL remains the typed
mutation/reconciliation path. Twitch currently supports
`drop.entitlement.grant` only through webhooks or conduits, not WebSockets.

## Typing policy

The runtime uses typed models for IDs, live state, stream metadata, followers,
channel-point context, inventory, drop campaigns, contributions, and mutation
status responses. Required identifiers, claim-safety fields, community-goal
financial fields, list containers, and contribution items fail closed when
missing or incompatible; optional per-edge Twitch data remains intentionally
skippable. PubSub prediction creation requires an ID, a recognized status,
valid timestamps/windows, and at least two valid outcomes. Incremental updates
require an ID and non-empty status but retain non-terminal states such as
`RESOLVE_PENDING` and `CANCEL_PENDING`; only explicit terminal states can
settle a bet. Both observed
`total_users`/`total_points` and `users`/`channel_points` counter names are
normalized; viewer results retain only a recognized result type and optional
nonnegative `points_won`. Parsing errors retain only a fixed protocol class and
operation context.

`ViewerDropsDashboard` deliberately retains unknown fields because Twitch
changes that experimental dashboard frequently and the miner only needs to
validate that the read completed. Bonus-claim responses are also allowed to
omit `status` when Twitch returns a balance-only
success envelope; an explicit non-empty error or an unknown status still fails
closed. `DropsHighlightService_AvailableDrops` treats a null channel or null
campaign list as an empty result, matching the Go reference, while every entry
in a present list still requires a non-empty campaign ID. Its typed result also
gates `DROPS` watch priority: unknown and empty results are not promoted, a
broadcast/game change invalidates the previous result, and later configured
priorities continue filling watcher capacity. The older raw JSON methods remain
compatibility facades; runtime and
canary code use the explicit typed variants. Neither path logs or exposes the
retained payload.
