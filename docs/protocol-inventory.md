# Twitch Protocol Inventory

The Rust client keeps persisted-operation names and SHA-256 hashes in
`tm-twitch::PERSISTED_OPERATION_CONTRACTS`. A unit test verifies that every
builder uses an inventoried, unique contract. The comparison source is the Go
implementation at commit `91f00698314d`; the baseline gate permits only its three
unimplemented definitions plus the two Go-unused definitions that Rust exercises
as typed playback and Drops-dashboard reads.

When both repositories are available, run the baseline tests and the explicit
comparison gate from the Rust workspace:

```powershell
./scripts/verify-go-baseline.ps1 -GoRoot ../Twitch-Channel-Points-Miner
```

The gate runs `go test ./...`, compares every Go persisted-operation hash with
Rust, and accounts for five Go definitions that Go never issues:
`PlaybackAccessToken`, `ModViewChannelQuery`, `ViewerDropsDashboard`,
`DropCampaignDetails`, and `PersonalSections`. Rust actively exercises
`PlaybackAccessToken` and `ViewerDropsDashboard`; the remaining three are not
part of either miner's runtime. The gate also requires the one documented hash
mismatch: Rust carries Twitch's current `PlaybackAccessToken` hash while the Go
baseline retains the retired hash for an operation it does not issue.

| Operation | Mode |
| --- | --- |
| `GetIDFromLogin` | Read-only |
| `ChannelFollows` | Read-only |
| `ChannelPointsContext` | Read-only |
| `WithIsStreamLiveQuery` | Read-only |
| `VideoPlayerStreamInfoOverlayChannel` | Read-only |
| `PlaybackAccessToken` | Read-only |
| `RewardList` | Read-only |
| `FilterableVideoTower_Videos` | Read-only |
| `ClipsCards__User` | Read-only |
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
operations listed above. When its target is live it also resolves the HLS master
and media playlists and performs one media-segment HEAD request. It does not
start workers, claim a reward, make a
prediction, join a raid, contribute points, mutate cookies, or send Discord
notifications. Record the source revision, image digest, date, and success or
failure class in the release notes; never record cookies, account IDs, raw
payloads, or request headers.

Every request target that Twitch supplies inside a document rather than one the
miner compiles in is checked before use: the settings script, playback master
playlist, selected media playlist, newest complete media segment, and the spade
endpoint. The
remote client disables redirects, requires HTTPS for public origins, and allows
HTTP loopback only when an endpoint-override constructor explicitly injects a
loopback HTTP base URL for local tests. The app's production construction path
does not enable that allowance. Its resolver
validates every address in each DNS answer (including later connection
resolutions), rejects loopback, link-local, private, unspecified, multicast,
documentation, reserved, and other non-public IPv4/IPv6 ranges, then returns
that validated address set directly to the connector. This prevents a mixed
answer or DNS rebinding from turning a public hostname into a private request;
relative playlist URLs are checked again after resolution. This matters most
for spade, whose minute-watched payload carries the channel, broadcast, and
account identifiers. The policy follows the IANA
[IPv4](https://www.iana.org/assignments/iana-ipv4-special-registry/iana-ipv4-special-registry.xhtml)
and [IPv6](https://www.iana.org/assignments/iana-ipv6-special-registry/iana-ipv6-special-registry.xhtml)
special-purpose registries; protocol-specific tunnel/anycast blocks are denied
because the remote-endpoint contract admits only ordinary public CDN addresses.

`CLIENT_ID` is the browser identity required consistently by device auth, GQL,
and EventSub; it cannot be discovered safely at runtime. `Client-Version` is
not pinned to a compiled fallback: the client extracts the current build ID
from Twitch's homepage before the first GQL request and refreshes the cached
value every ten hours. Discovery or rejection failures remain explicit and are
covered by the canary; the miner does not guess alternate identities or
credentials.

Offline streak recovery uses the two typed read-only operations above. Archived
videos require an ID, duration, and optional broadcast identifier; the scheduler
accepts a VOD only when its broadcast identifier exactly matches the missed live
broadcast and it is at least five minutes long. Clip nodes require an ID, slug,
URL, and finite positive duration. Playback submission is opt-in, single-worker,
bounded to 23.5 hours, and preempted by live state. An HTTP 204 is recorded only
as accepted playback progress; a newer typed `RewardList` milestone is required
before runtime state reports recovery.

Mutation contracts are verified with sanitized fixtures and response-validation
tests. Read-only requests are bounded and header-aware; mutations are never
automatically replayed after an uncertain response. A release with a changed
operation hash must add its sanitized fixture, update this inventory, and pass
the canary before publication.

The watch-selection metadata refresh has one bounded raw read-only GraphQL
query, `ResolveLoginById`, used only after a selected channel is still live by
stable numeric ID but its login-based stream-info lookup reports a missing
user. The typed response must return the same requested ID and a non-empty
normalized login before runtime identity is changed. A successful change is
retried once; an unresolved mismatch releases the watch slot for five minutes
instead of allowing it to consume half of the watch capacity indefinitely.
Minute-watched sends reuse the refreshed runtime snapshot, including broadcast
and game metadata, rather than issuing a second stream-info request on every
tick. A send performs one inline stream-info refresh only when that snapshot is
absent, has no broadcast, or is older than five minutes, so a stalled batched
refresh neither fails every tick for the channel nor lets a watch event be sent
against metadata that may no longer describe the broadcast. Because the batched
refresh owns presence detection, an offline channel can be selected for up to
one refresh interval before the event transports or the next refresh clear it.
Refresh failures are reported as one `metadata-refresh` task failure per cycle.
No response payload, token, or cookie is logged.

Playback priming deliberately remains uncached. The scheduler gives each
selected channel a nominal 20-second interval; with the normal two slots it
serializes the sends 10 seconds apart. Because the interval sleep begins after
each request finishes, a channel is revisited after both selected requests plus
the two nominal sleeps (about 20 seconds when requests are short).
Every successful tick therefore performs one `PlaybackAccessToken` GQL request,
one master-playlist GET, one selected media-playlist GET, and one media-segment
HEAD before the spade POST. A regression test calls the primer twice and proves
that all four requests are repeated for each call. Production failure counts
are recorded with each release's acceptance evidence. Successful per-stage
requests are not logged, so their volume comes from the deterministic scheduler
and request path rather than invented telemetry. No credited WATCH/WATCH_STREAK
A/B or canary evidence currently proves that reusing a playback session
preserves credit. Adding a broadcast-bound cache on request volume alone would
therefore weaken a working private protocol contract without evidence, and is
rejected.

The playback-token request includes both the audited persisted hash and its full
read-only query. Twitch accepts that combined shape, so a future persisted-hash
retirement does not stop WATCH while raw queries remain supported; a schema
change or raw-query rejection still fails strict health and requires a release.

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
avoiding an edge race at the advertised silence boundary. A connected peer has
15 seconds to send the first Welcome frame, and the complete connect, Welcome,
and subscription-setup attempt has a four-minute outer budget. Retry attempts
refresh supervision activity without being recorded as successes; the existing
eight-minute silent-task policy therefore still restarts a genuinely stuck
task while bounded recovery remains in process.

Message and subscription types this build does not model are ignored rather
than treated as protocol violations, so an additive Twitch change cannot force a
reconnect loop that shrinks the subscription set on each cycle; a payload for a
subscription type the miner does act on still fails closed. A session inherited
through a reconnect keeps its subscriptions, and its active count is re-derived
from Twitch for the new session ID rather than carried over from the previous
session's report.

The isolated PubSub compatibility path connects to
`wss://pubsub-edge.twitch.tv/v1`. It supplies viewer prediction discovery and
result events, immediate point/bonus events, moment IDs, raid IDs, and
community-goal changes. It is unofficial/deprecated, so LISTEN acknowledgement,
message time, reconnect count, and fixed failure class are exposed separately
from EventSub status. User topics alone receive the auth token, connections are
limited to 50 topics, and failures cannot stop EventSub, polling, IRC, or drops.
Both paths normalize into `tm-domain::MinerEvent`; mutation IDs, point-event state, and
prediction event IDs are boundedly deduplicated before effects are scheduled. GQL remains the typed
mutation/reconciliation path. Twitch currently supports
`drop.entitlement.grant` only through webhooks or conduits, not WebSockets.
Optional IRC chat presence connects only to `irc.chat.twitch.tv:6697` through
Rustls with WebPKI roots; the OAuth token is never sent over plaintext IRC.

The completed live release evidence validates this hybrid boundary rather than
claiming an EventSub-only viewer contract: it records 20 successful
post-mutation `update_raid` logs, with 19 matching RAID rewards observed within
15 minutes, and 14 live Drop progress/claim pairs. The Drop evidence validates
the claim path but does not assert exact campaign pin/unpin telemetry. Discord
is the sole built-in outbound notifier; generic notifier backends and analytics
exporters remain out of scope absent a concrete operator requirement.

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
gates `DROPS` watch priority when `farm_drops` is enabled. One typed inventory
snapshot per selection refresh retains campaign IDs and marks an ID complete
only when its non-empty drop list is explicitly fully claimed. Per-channel
available IDs are filtered against that completed set. New, incomplete,
missing-progress, and transient-failure cases remain eligible; completed
campaigns release their pin so the spare points slot is not suppressed. Unknown
and empty available-campaign results are not promoted, a broadcast/game change
invalidates the previous result, and later configured priorities continue
filling watcher capacity. The older raw JSON methods remain
compatibility facades; runtime and
canary code use the explicit typed variants. Neither path logs or exposes the
retained payload.
