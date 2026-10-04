# Twitch protocol inventory

This is the normative reference for what the miner sends to Twitch and how it
reacts: persisted operations, playback submission, watch and campaign
selection, transports, retries, mutation safety, and typing policy. Other
documents summarize or compare these contracts and link here rather than
redefining them.

## Persisted operations

The Rust client keeps persisted-operation names and SHA-256 hashes in
`tm-twitch::PERSISTED_OPERATION_CONTRACTS`. A unit test verifies that every
builder uses an inventoried, unique contract. The comparison with the pinned Go
baseline, including its documented hash differences, is in the
[parity matrix](parity-matrix.md#go-baseline-check).

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

The watch-selection metadata refresh also uses one bounded raw read-only
GraphQL query, `ResolveLoginById`, described under
[Watching and playback](#watching-and-playback).

## Release canary and hash probe

Twitch can replace undocumented persisted-query contracts at any time. Before
each release, run the credential-safe canary on a dedicated account:

```sh
twitch-miner --data-dir /data --canary
```

The canary validates an existing session and performs only the read-only
operations listed above. When its target is live it also resolves the HLS master
and media playlists and performs one media-segment HEAD request. It also
requires EventSub setup and list verification and a PubSub LISTEN
acknowledgement for every configured compatibility topic, and it never applies
received transport events to runtime state. It does not start workers, claim a
reward, make a prediction, join a raid, contribute points, mutate cookies, or
send Discord notifications. A successful canary proves only the listed read
operations for that account at that time; mutations remain fixture-verified so
release validation never claims rewards or places bets. Record the source
revision, image digest, date, and success or failure class in the release notes;
never record cookies, account IDs, raw payloads, or request headers.

For a credential-free, on-demand hash check, probe the inventoried hashes
without starting the miner:

```sh
cargo run -p tm-twitch --example apq_probe --locked
cargo run -p tm-twitch --example apq_probe --locked -- --operation PlaybackAccessToken
```

The probe sends only the public Client-ID, operation name, and persisted-query
hash; it sends no authorization, cookies, variables, query text, or mutation
input. Mutation rows are labeled `MUTATION-HASH`: the probe provides neither
authorization nor mutation input and is not an account-action request.
`REGISTERED` means HTTP 200 with a valid JSON response that is not a persisted
query error. `BROKEN` is reserved for an explicit `PersistedQueryNotFound`.
Transport failures, non-200 responses, invalid JSON, and
`PersistedQueryNotSupported` are `INCONCLUSIVE`. This is diagnostic evidence,
not a release gate or automatic hash-refresh mechanism; `REGISTERED` does not
prove that an operation would execute with credentials or valid variables.
Authenticated decoding reports exact `PersistedQueryNotFound` responses as the
fixed `persisted-query-not-found` class and retains only the operation name.

## Remote endpoints and client identity

Every request target that Twitch supplies inside a document rather than one the
miner compiles in is checked before use: the settings script, playback master
playlist, selected media playlist, newest complete media segment, and the spade
endpoint. The remote client disables redirects, requires HTTPS for public
origins, and allows HTTP loopback only when an endpoint-override constructor
explicitly injects a loopback HTTP base URL for local tests. The app's
production construction path does not enable that allowance. Its resolver
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
Requests to these Twitch-supplied URLs bypass system proxies so that redirect
and DNS-address validation cannot be bypassed.

`CLIENT_ID` is the browser identity required consistently by device auth, GQL,
and EventSub; it cannot be discovered safely at runtime. `Client-Version` is
not pinned to a compiled fallback: the client extracts the current build ID
from Twitch's homepage before the first GQL request and refreshes the cached
value every ten hours. Discovery or rejection failures remain explicit and are
covered by the canary; the miner does not guess alternate identities or
credentials.

The device ID and per-process `Client-Session-Id` are random hexadecimal values
seeded from the operating system, so they differ across processes and installs.

The miner sends no `Client-Integrity` header. Tokens minted outside a real
browser are flagged as bots, so it does not try to obtain one. A GraphQL error
reporting a failed integrity check is classified as `integrity-required`
instead of a generic error, so status and logs show why Twitch refused.

## Watching and playback

Playback priming deliberately remains uncached. The scheduler gives each
selected channel a nominal 20-second interval; with the normal two slots it
serializes attempts at nominal 10-second intervals. Snapshot and request time
consume that interval, and only the remaining time is slept. An overrun starts
the next attempt without replaying missed ticks; it does not queue catch-up
requests. Watch-selection work can still add time between passes.
Every tick performs one `PlaybackAccessToken` GQL request, one master-playlist
GET, one selected media-playlist GET, and one newest-complete-segment HEAD before
the spade POST. Local request-count savings did not establish credited
WATCH/WATCH_STREAK equivalence, so the experimental broadcast cache was removed.
The three HLS reads reuse the standard three-attempt read policy for connection
resets, timeouts, HTTP 429, and HTTP 5xx; other HTTP 4xx responses fail without
retry. Failures retain only the fixed playback stage and sanitized failure class.

The playback-token request sends the audited persisted hash alone. Only an exact
`PersistedQueryNotFound` retries once with the audited full read-only query;
other failures do not change the request shape. A hash rotation can therefore
self-heal while a schema change or raw-query rejection still fails strict health
and requires a release.

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

## Watch selection

At most two Twitch-creditable slots run at once.

`ChannelPointsContext` also reads Twitch's optional
`communityPointsSettings.isEnabled` marker. An explicit `false` makes the
channel ineligible for credited watch slots and point-dependent actions while
presence monitoring continues. Missing remains unknown/compatible because the
private response does not always include the marker; explicit `null` has the
same unknown meaning, while any other non-boolean value is a typed protocol
error. A verified active Drop campaign remains watchable on a channel with
points disabled, because Drops are independent of channel points, without
advancing local point or streak progress.

Twitch's current [Watch Streaks requirements](https://help.twitch.tv/s/article/recover-watch-streaks)
state that at least 30 minutes must pass between the end of one stream and the
start of the next. That platform rule, rather than parent-miner lineage, is the
contract used by streak prioritization. `STREAK`, `LONGEST_STREAK`, and
`EXPIRING_STREAK` stay inside an evidence-based ten-minute live-streak budget,
which is internal policy rather than a setting.

When watch selection changes, newly selected channels receive requests before
retained channels. That dispatch order persists on subsequent passes while
logical health-slot attribution remains tied to selection order. The two-slot
limit and request interval are unchanged. Moving a new channel first delays the
retained channel by one stagger at handover; faster request dispatch does not
establish faster server credit or higher earnings.

Fair rotations and streak promotions share a 15-minute promotion cooldown.
When two ordinary watch slots are available, fair rotation replaces one per
turn so an established slot remains selected while the replacement starts.
The campaign pin preserves configured game priority, then ranks known earnable
rewards by earliest expiry, with configured channel order breaking
equal-deadline ties independently of progress refreshes and temporary streak
rank. Channel availability must confirm the campaign; inventory alone cannot
authorize a channel. Completed, subscription-only, prerequisite-blocked, future
and infeasible rewards do not receive deadline priority. Missing planning
metadata retains the existing campaign/channel-order behavior without inventing
eligibility or deadlines. Targets older than ten minutes lose deadline priority.
This ranks rewards on tracked channels; it does not discover new channels or
guarantee optimal global campaign coverage. Releasing a channel resets its
watched minutes; that reset must not send the campaign pin immediately back to
it. A streak candidate arriving just after a fair rotation waits until the next
turn instead of displacing a channel that has only just started watching.
Startup promotions, campaign preemption, unavailable-channel replacement and
watchdog recovery remain immediate. Watchdog and unavailable-channel
replacements start a fresh turn so the outgoing channel's rotation deadline
cannot immediately displace the replacement. Campaign changes retain the
existing fairness clock. When current-visit reward measurement is healthy and
the outgoing channel's last WATCH or WATCH_STREAK credit is 210–299 seconds
old, fair rotation may wait for the next credit from that channel, capped at
120 seconds. Missing measurement, campaign changes and eligible streak
promotions bypass this wait. This is a bounded cadence heuristic, not a
prediction of Twitch's next award. The 30-minute fairness ceiling still limits
streak deferrals. This bounds voluntary switching; it does not guarantee that
Twitch will credit every partial watch interval.

## Offline streak recovery

Offline streak recovery uses the three typed read-only operations above
(`RewardList`, `FilterableVideoTower_Videos`, and `ClipsCards__User`). Archived
videos require an ID, duration, and optional broadcast identifier; the scheduler
accepts a VOD only when its broadcast identifier exactly matches the missed live
broadcast and it is at least five minutes long. Clip nodes require an ID, slug,
URL, finite positive duration, and the same broadcast identifier. Playback
submission is opt-in, single-worker, bounded to 23.5 hours, and preempted by live
state. An HTTP 204 is recorded only as accepted playback progress. Runtime state
reports recovery only when a later typed `RewardList` preserves the same non-null
streak count and removes the targeted broadcast from `missedStreams`, or clears
the expiring-risk envelope entirely. Unconfirmed playback retains the short
retry cooldown; only typed confirmation receives the recovery-window cooldown.
Unresolved streak state is also reconciled once after a confirmed bonus claim
and once after accepted offline-recovery playback settles, using the typed
milestone rather than treating either trigger as proof.

## Drops

`DropsHighlightService_AvailableDrops` treats a null channel or null campaign
list as an empty result, matching the Go reference, while every entry in a
present list still requires a non-empty campaign ID. Its typed result also
gates `DROPS` watch priority when `farm_drops` is enabled. One typed inventory
snapshot per selection refresh retains campaign IDs and marks an ID complete
only when its non-empty drop list is explicitly fully claimed. Per-channel
available IDs are filtered against that completed set. New, incomplete,
missing-progress, and transient-failure cases remain eligible; completed
campaigns release their pin so the spare points slot is not suppressed. Unknown
and empty available-campaign results are not promoted, a broadcast/game change
invalidates the previous result, and later configured priorities continue
filling watcher capacity.

Channel campaign IDs alone do not establish Drops priority: selection requires
an observed unfinished watch reward in inventory. Omitted completed campaigns
therefore cannot regain priority. Channel-provided subscription and watch-time
requirements filter non-watch campaigns before inventory selection. Missing
inventory defers priority until an unfinished reward is observed; it does not
prevent ordinary channel-points watching.

### Reward-level inventory selection

`Inventory` retains `timeBasedDrops.id`, campaign ID, `startAt`/`endAt`,
`requiredMinutesWatched`, `requiredSubs`, and the viewer's partial progress even
when `self.dropInstanceID` is null. Claim requests still require a nonempty
instance ID, complete progress, and an unclaimed reward. Prerequisites use
`self.hasPreconditionsMet` when supplied, otherwise explicit `preconditionDrops`
IDs must all be claimed in the same inventory. Missing prerequisite information
or invalid/missing dates cannot establish deadline priority. The channel's
`viewerDropCampaigns` response remains the eligibility boundary. No new operation
or request cadence is added. The synthetic `twitch.inventory_progress.json`
fixture covers partial progress, locked prerequisites and subscription rewards.

Drop progress is retained before a claim-instance ID exists; a missing claim ID
still prevents a claim request. Reward and campaign IDs stabilize status
identity across claim readiness, successful claim responses update status
immediately, and stale unclaimed inventory does not reverse that status while
the reward remains in the reported inventory. Inventory timestamps describe
observations, not precise reward completion times. Status retains the current
inventory rather than a 16-reward history, preserving progress timestamps for
retained rewards and removing entries absent from the next successful inventory
unless that inventory explicitly confirms their awards.

`Inventory.earnedDropRewards.edges[].node` also provides awarded rewards after
their campaigns leave `dropCampaignsInProgress`. Only an explicit `CLAIMED`
status with nonempty campaign/item IDs and a valid `earnedAt` timestamp is
confirmation. Status reconciles a previously observed reward only when every
`benefitEdges[].benefit.id` matches that campaign's awards and the awards are at
least as recent as its last unclaimed observation, without future timestamps.
Names and disappearance alone never establish a claim. Missing benefit IDs,
unknown award states and invalid dates cannot establish confirmation.

Confirmed entries remain in status while the current inventory still contains
their awards. Their last observed watch minutes and progress timestamp remain
unchanged; observing an award neither sends a claim mutation nor increments the
miner's mutation counter, and the reconciliation adds no network requests.
Matching IDs stay in memory and are excluded from status serialization. The
synthetic `twitch.inventory_awarded.json` transition covers a 59/60 reward
moving directly to the awarded list.

## Mutation safety

Mutation contracts are verified with sanitized fixtures and response-validation
tests. Read-only requests are bounded and header-aware; mutations are never
automatically replayed after an uncertain response. A release with a changed
operation hash must add its sanitized fixture, update this inventory, and pass
the canary before publication.

A prediction whose response carries `data.makePrediction.error` is a typed
rejection: the reservation is cleared and no result is recorded. Only an
upper-case error code such as `NOT_ENOUGH_POINTS` is kept.

Bonus claims whose connections fail before the mutation is sent can be retried
after a later availability observation. Ambiguous outcomes remain reserved to
avoid replaying a potentially completed claim.

Prediction placements are recorded in a bounded journal before the mutation is
sent, and unresolved decisions are restored after a restart. Placement capacity
counts at most 128 unresolved requests separately from confirmed/rejected replay
records. Terminal records remain protected for seven days, and the entire
journal remains bounded to 256 KiB. Full storage fails closed rather than
evicting unresolved requests or recent replay records. Reload and capacity tests
cover more than 128 resolved placements. Admission budgets the largest
serialized state of every record that can still transition, including
rejected-to-confirmed upgrades and timestamp growth, so later reconciliation
fits. Saturated rejected history is upgraded and reopened in regression
coverage. A synthetic saturation test with nine-digit account/channel IDs,
UUID-sized event/outcome IDs and 50,000-point stakes retains 757 confirmed
records (261,955 bytes) before rejecting the next reservation. This
characterizes that fixture, not a universal record limit or a live workload. At
seven-day retention it represents about 108 such records per day.

The `prediction_journal` status byte count includes the newline and excludes
temporary files. `capacity_blocked` reports a full unresolved limit or a
capacity rejection in this process; a successful reservation, resolution or
expiry clears the rejection flag. After restart, only the unresolved limit is
known until another admission is attempted. A false flag does not guarantee
that an arbitrarily sized request fits. Counts and bytes contain no account,
channel or prediction identities.

## Transports

EventSub, PubSub, IRC, and presence polling are independently supervised. Both
event paths normalize into `tm-domain::MinerEvent`; mutation IDs, point-event
state, and prediction event IDs are boundedly deduplicated before effects are
scheduled. GQL remains the typed mutation/reconciliation path. Twitch currently
supports `drop.entitlement.grant` only through webhooks or conduits, not
WebSockets.

### EventSub

The preferred EventSub WebSocket path handles stream presence and observes
raids. Broadcaster prediction subscriptions are requested only when a tracked
channel ID exactly matches the authenticated user ID and the validated token
actually contains `channel:read:predictions` or
`channel:manage:predictions`; ordinary viewer tokens cannot authorize them for
arbitrary tracked channels. Other channels remain on PubSub compatibility.
EventSub creation and list responses use separate typed envelopes because only
the list response contains pagination. Both are capacity-planned; overflow or
failed presence capabilities use bounded GQL polling instead of silently
dropping channels.

Capacity is planned against a total subscription cost ceiling of `10` with an
assumed cost of `1` per subscription, and one connection is additionally bounded
to 300 subscriptions. An ordinary tracked streamer plans `stream.online` and
`stream.offline`, so it spends two cost units and the ceiling admits presence
subscriptions for five streamers. Streamers beyond that allocation are annotated
`capacity-overflow` and served by bounded GQL presence polling; `channel.raid`
is skipped for them and raid observation falls back to the PubSub compatibility
path. The same `capacity-overflow` class can also mean that an optional raid or
prediction subscription was skipped while that streamer's presence subscription
remains active. It is the designed allocation outcome, not a total-streamer
failure: a saturated plan reports ten planned and ten active subscriptions with
zero failed subscriptions and a non-zero overflow count. Normal runtime skips
the immediate post-create listing; `verified=false` means the current report
has not been verified, not necessarily that setup is unhealthy. After setup,
three capacity rechecks run with 60-second spacing, each bounded to four minutes,
while the WebSocket continues receiving events. Paginated listings refresh cost
and current-session ownership; freed capacity restores the existing presence-first
plan, replacing only known lower-priority subscriptions on that session when
necessary. Other sessions and unknown subscriptions are never deleted. Recheck
failures retain working subscriptions and polling, and do not restart the socket.
Changed sets are listed again for verification. After these bounded rechecks,
cost fields remain the last observed snapshot, not a live global-cost gauge.
The exclusive deployment canary enables immediate listing and requires a fully
verified report. The `--status` document reports the split under `eventsub` as
`planned_subscriptions`, `active_subscriptions`, `failed_subscriptions`,
`total_cost`, `max_total_cost`, and `overflow_streamers`, and each per-streamer
capability records its own `presence_source`, `prediction_source`, `raid_source`,
and `failure_class`. `raid_source` is `pubsub-compatibility` unless a
`channel.raid` subscription was actually allocated, in which case it is
`eventsub+pubsub-compatibility`.

The WebSocket requests Twitch's supported 30-second keepalive window and applies
a five-second delivery grace before reconnecting, avoiding an edge race at the
advertised silence boundary. A connected peer has 15 seconds to send the first
Welcome frame, and the complete connect, Welcome, and subscription-setup attempt
has a four-minute outer budget. Retry attempts refresh supervision activity
without being recorded as successes; the existing eight-minute silent-task
policy therefore still restarts a genuinely stuck task while bounded recovery
remains in process.

Message and subscription types this build does not model are ignored rather
than treated as protocol violations, so an additive Twitch change cannot force a
reconnect loop that shrinks the subscription set on each cycle; a payload for a
subscription type the miner does act on still fails closed. A session inherited
through a reconnect keeps its subscriptions. The supplied URL must use `wss`,
a `twitch.tv` subdomain host (Twitch requires the URL to be used as is and does
not promise the original host), no user information, the default/443 port, and
no fragment; its opaque path and query are then used unchanged. The
old socket continues delivering through the overlap until the replacement sends
Welcome, no duplicate subscription POSTs are made, and the active count is
re-derived from Twitch for the new session ID rather than carried over from the
previous session's report.

### PubSub compatibility

The isolated PubSub compatibility path connects to
`wss://pubsub-edge.twitch.tv/v1`. It supplies viewer prediction discovery and
result events, immediate point/bonus events, moment IDs, raid IDs, and
community-goal changes. It is unofficial/deprecated, so LISTEN acknowledgement,
message time, reconnect count, and fixed failure class are exposed separately
from EventSub status. User topics alone receive the auth token, connections are
limited to 50 topics, and failures cannot stop EventSub, polling, IRC, or drops.
Each channel also gets an undocumented `video-playback-by-id` presence topic,
listed after every other topic. Above the 500-topic limit (10 connections) the
list is truncated with a warning, so presence topics are dropped first and the
account topics are always kept.
Server-requested reconnects use the clean-close retry policy: a five-second base
plus a deterministic per-connection offset of up to six seconds, with backoff
for repeated short-lived connections. A
connection lasting at least five minutes resets the retry count. Subscriptions
must be acknowledged again before their capability is reported ready.

### IRC

Optional IRC chat presence connects only to `irc.chat.twitch.tv:6697` through
Rustls with WebPKI roots; the OAuth token is never sent over plaintext IRC.
A session that ends within a minute, such as a rejected login, backs off
exponentially from five seconds to five minutes. The rejection notice does not
count as activity, so repeated rejections reach the task health threshold.

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
settle a bet. Both observed `total_users`/`total_points` and
`users`/`channel_points` counter names are normalized; viewer results retain
only a recognized result type and optional nonnegative `points_won`. Parsing
errors retain only a fixed protocol class and operation context.

`ViewerDropsDashboard` deliberately retains unknown fields because Twitch
changes that experimental dashboard frequently and the miner only needs to
validate that the read completed. Bonus-claim responses are also allowed to
omit `status` when Twitch returns a balance-only success envelope; an explicit
non-empty error or an unknown status still fails closed. The older raw JSON
methods remain compatibility facades; runtime and canary code use the explicit
typed variants. Neither path logs or exposes the retained payload.
