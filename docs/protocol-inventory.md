# Twitch Protocol Inventory

The Rust client keeps persisted-operation names and SHA-256 hashes in
`tm-twitch::PERSISTED_OPERATION_CONTRACTS`. A unit test verifies that every
builder uses an inventoried, unique contract. The comparison source is the Go
implementation at commit `940c98409e58`; the hashes below match the operations
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

The EventSub WebSocket path handles stream presence and prediction lifecycle
notifications. Raid notifications are accepted only as observations because
the current EventSub payload has no legacy `raidID` for the join mutation.
Viewer points, bonus claims, moments, community goals, and drop entitlements
remain GQL/polling-backed; Twitch currently supports `drop.entitlement.grant`
only through webhooks or conduits, not WebSockets.

## Typing policy

The runtime uses typed models for IDs, live state, stream metadata, followers,
channel-point context, inventory, drop campaigns, contributions, and mutation
status responses. Required identifiers, claim-safety fields, community-goal
financial fields, list containers, and contribution items fail closed when
missing or incompatible; optional per-edge Twitch data remains intentionally
skippable. Parsing errors retain only a redacted top-level shape and operation
context.

`ViewerDropsDashboard` deliberately retains unknown fields because Twitch
changes that experimental dashboard frequently and the miner only needs to
validate that the read completed. Prediction-result details are also kept as a
small dynamic value because the result schema varies by outcome. Bonus-claim
responses are also allowed to omit `status` when Twitch returns a balance-only
success envelope; an explicit non-empty error or an unknown status still fails
closed. The older raw JSON methods remain compatibility facades; runtime and
canary code use the explicit typed variants. Neither path logs or exposes the
retained payload.
