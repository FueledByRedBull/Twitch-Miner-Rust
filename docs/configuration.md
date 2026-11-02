# Configuration reference

The miner reads one JSON file, normally `config.json` in its data directory.
Start from [`config.example.json`](../config.example.json), which contains every
setting below, and check your edits without contacting Twitch:

```sh
tm-app --config data/config.json --data-dir data --check-config
```

Unknown keys, unsupported values, and out-of-range numbers are rejected with
their exact JSON path (for example `config.bet.strategy`) before the miner
starts or writes anything. Older Go and Python files are migrated as described
in the [migration guide](migration.md).

If the file is missing, the miner creates one from built-in defaults that match
the template except for the placeholder username and streamer list. Missing keys
in an existing file are filled from the same defaults.

## Where the files live

- `--data-dir <dir>` sets the data directory. The configuration then defaults to
  `<dir>/config.json`; `--config <file>` overrides it.
- With only `--config <file>`, the data directory is the folder containing that
  file.
- Without flags, `TCPM_DATA_DIR` and then `TCPM_CONFIG` are used. The container
  image sets both, to `/data` and `/data/config.json`.
- Otherwise the miner uses `config.json` in the current directory if it exists,
  and falls back to a default location next to the executable or in the user
  configuration directory.

The data directory also holds `cookies/<username>.json` (your Twitch session,
which is secret; it can be [encrypted](operations.md#encrypting-the-saved-session)),
optional `log/` files, `streak-cache.json`,
`prediction-placements.json`, `runtime-status.json`, and `earnings/`, the
[earnings ledger](operations.md#earnings-ledger-and-alarms).

## Changing settings while the miner runs

The miner rereads the configuration file every 30 seconds and applies what it
can without a restart. It only ever reads the file: a reload never writes or
migrates it. An unchanged file does nothing. A file that is missing, cannot be
read, is not valid JSON, or fails the same validation as `--check-config` is
ignored with one warning, and the running configuration stays in force. How to
tell whether a reload applied is described in the
[operations guide](operations.md#config-reload).

**Applied live:**

| Setting | Notes |
| --- | --- |
| `streamers`, `streamers_exclude` | Channels are added and removed while the miner runs; see below. |
| `game_priority`, `game_exclude`, `watch_priority` | Used by the next watch selection. |
| `farm_drops`, `watch_one_stream_when_drops_active`, `claim_moments` | |
| `betting(make_predictions)`, `follow_raid`, `community_goals`, `chat_presence` | `make_predictions` applies to predictions created after the reload. |
| `claim_drops`, `watch_streak_vod_recovery` | Live while they keep the same background tasks running; see below. |
| `streak_discovery_games` | The next check refreshes streak discovery at once: new games are listed, and channels of games you removed are dropped. Otherwise it refreshes about every six hours. |
| `bet` (every field) | Used by the next prediction. |
| `streamer_overrides` | Replaces the per-channel overrides. A new or changed `points_limit` applies to the next watch selection. |
| `emojis`, `show_claimed_bonus_msg`, `show_game` | |
| `discord.webhook_api`, `discord.events`, `webhook.url`, `webhook.events` | Notifications use the new target and filter from the reload on. |

**Needs a restart:** `config_schema_version`, `username`, `debug`,
`debug_deep`, `show_seconds`, `claim_drops_startup`, `save_logs`,
`show_username_in_console`, `disable_at_in_nickname`, `followers_order`,
`drop_account_link`, `drop_discovery_games`, `log_format`,
`notify_device_login`, `proxy`, `timezone`, and `privacy.anonymize_logs`. These are read once at startup. A file that changes
only these is not applied, and the status block lists their names. A file that
changes both applies the live settings and reports the rest.

The Drop claim loop and the watch-streak recovery loop exist only when some
channel needs them at startup. If a reload would leave no channel needing one
that is running, or give a channel the need when none is running, `claim_drops`
or `watch_streak_vod_recovery` (including the same keys in
`streamer_overrides`) is held back and reported as needing a restart, while the
other settings of that reload still apply.

**Channel changes.** Adding a channel resolves it the way startup does, so a
name that does not exist rejects the whole reload: nothing is applied, the
running configuration stays, and the attempt is retried every five minutes
while the file is unchanged. Removing a channel stops tracking it. A channel
with a prediction whose placement is not settled stays tracked, even though it
is no longer listed, until that prediction resolves, and is then removed.
Only channels this change drops are removed; channels found by
`drop_discovery_games` stay until a restart. Channels found by
`streak_discovery_games` are never added or removed by a change to `streamers`,
and a login you add to `streamers` that discovery already tracks becomes a
normal configured channel. In follower mode the follower list
is fetched again only when `streamers` or `streamers_exclude` changed, never on
a timer. A change that would leave no channel is rejected.

**PubSub subscriptions.** `follow_raid`, `betting(make_predictions)`,
`farm_drops`, `claim_drops`, `claim_moments`, and `community_goals` decide which
PubSub topics are subscribed. A reload that changes the topics reconnects the
transports, as a channel change does. Streak-discovered channels add one
presence topic each, listed last, so they are the first topics dropped when the
500-topic limit is reached.

**Missing keys.** A file that lacks keys, such as the fields a partial
`streamer_overrides` entry omits, is read with the same defaults a restart would
use and applies normally. The file itself stays as you wrote it until the next
start fills the keys in.

## Account and channels

| Setting | Default | Meaning |
| --- | --- | --- |
| `username` | placeholder | Your Twitch login, not your display name: ASCII letters, digits, and underscores, at most 25 characters, normalized to lowercase. Windows device names such as `CON`, `AUX`, `COM1`, and `LPT1` are rejected on every platform so the data directory stays portable. Required. |
| `streamers` | `[]` | Channels to watch. Their order is the `ORDER` priority. When the list is empty, the miner loads every channel you follow. |
| `streamers_exclude` | `[]` | Logins that are never watched. |
| `followers_order` | `DESC` | Order of followed channels when `streamers` is empty: `ASC` or `DESC`. |
| `drop_account_link` | `linked` | Which Drop campaigns to farm by game-account link: `linked` skips campaigns you have not linked, `all` farms everything, `none` skips any campaign that involves a game account. |
| `drop_discovery_games` | `[]` | Game slugs (the last part of the game's Twitch directory URL, such as `rocket-league`) to find channels for. At each start the miner adds up to five live channels with Drops enabled per game. They use your global settings and `streamers_exclude` still applies, but the list is only refreshed when the miner restarts. |
| `streak_discovery_games` | `[]` | Opt-in. Game slugs, as for `drop_discovery_games`, to look for channels where a watch streak may be waiting. About every six hours the miner adds up to five live channels per game, at most 20 in all, most viewed first, and drops the ones that went offline or are no longer listed. A discovered channel gets a watch slot only while its streak is not yet collected in the current broadcast, for at most ten minutes of watching per broadcast, never ahead of a configured channel, and runs with predictions, raids, community goals, Drops, moments, streak recovery, and chat off. `streamers_exclude`, `game_exclude`, and your own `streamers` apply: a channel you also list stays a normal configured channel. Discovered channels are never written to the file. A failed lookup changes nothing and logs one warning. Existing files are not rewritten to add the key, so a rollback to an older release can still read them. |
| `game_priority` | `[]` | Games to prefer, in order. |
| `game_exclude` | `[]` | Games whose streams are not watched. |
| `watch_priority` | `["STREAK", "DROPS", "ORDER"]` | How the two watched channels are chosen. Priorities are tried in order; see the next table. |

| Watch priority | Picks |
| --- | --- |
| `STREAK` | Channels whose watch streak needs attention in the current broadcast. |
| `DROPS` | Channels with an unfinished, earnable Drop reward. |
| `ORDER` | Channels in the order listed in `streamers`. |
| `SUBSCRIBED` (also `SUBS`, `MULTIPLIER`) | Channels where you have an active points multiplier, highest first. |
| `POINTS_ASCENDING` (`POINTS_ASC`) / `POINTS_DESCENDING` (`POINTS_DESC`) | Channels by your channel-points balance, lowest or highest first. |
| `LONGEST_STREAK` (`STREAK_LONGEST`) / `EXPIRING_STREAK` (`STREAK_EXPIRING`) | Streak candidates, ranked by streak length or by expiry. |

Priority names are case-insensitive. Streak-based priorities stay within an
internal ten-minute live-streak budget that is not configurable. Campaign
pinning and fair rotation are separate rules, described in the
[protocol inventory](protocol-inventory.md#watch-selection).

## Earning features

| Setting | Default | Meaning |
| --- | --- | --- |
| `claim_drops_startup` | `true` | Claim already-earned Drops when the miner starts. |
| `farm_drops` | `true` | Let Drop campaigns influence which channels are watched (the `DROPS` priority). |
| `claim_drops` | `true` | Claim Drop rewards once they are complete. |
| `watch_one_stream_when_drops_active` | `true` | While `DROPS` has picked a channel with an unfinished reward, watch only that channel and leave the second slot unused. Twitch advances Drop progress on one channel at a time, so set this to `false` to keep earning channel points on a second channel. |
| `claim_moments` | `true` | Claim moment rewards. |
| `watch_streak_vod_recovery` | `false` | Try to recover a missed watch streak by playing back the matching VOD or clip. |
| `betting(make_predictions)` | `false` | Place prediction bets using the `bet` settings. The key keeps its historical Go/Python name; do not rename it. |
| `follow_raid` | `false` | Join raids started by watched channels. |
| `community_goals` | `false` | Contribute points to community goals. |
| `chat_presence` | `ONLINE` | When to join the channel's chat over IRC: `ALWAYS`, `ONLINE` (only while live), `OFFLINE` (only while offline), or `NEVER`. |
| `disable_at_in_nickname` | `false` | Also treat your plain username, without `@`, as a chat mention. |

## Logging and notifications

| Setting | Default | Meaning |
| --- | --- | --- |
| `debug` | `false` | Verbose diagnostic logging. |
| `debug_deep` | `false` | Extra detail. Takes effect only with `debug` enabled and `privacy.anonymize_logs` disabled. |
| `show_seconds` | `false` | Include seconds in log timestamps. |
| `emojis` | `true` | Use emoji in log messages. |
| `save_logs` | `false` | Also write logs to `log/` in the data directory. Each log rotates at 10 MiB, keeps at most five archives, and prunes archives older than 30 days. |
| `show_username_in_console` | `false` | Show your username in console log lines. |
| `show_claimed_bonus_msg` | `true` | Log each claimed bonus chest. |
| `show_game` | `true` | Include the game name in point messages. |
| `log_format` | `text` | `text` for the human-readable lines, or `json` for one JSON object per line on the console and in saved logs, with `timestamp`, `level`, `operation`, `message`, and a `fields` object of extra values (omitted under `privacy.anonymize_logs`, like the text format). Any other value is rejected. Existing config files are not rewritten to add it, so a rollback to an older release can still read them. |
| `notify_device_login` | `false` | Opt-in. When the miner must start a device login, also send the verification URL and the code through the `DEVICE_LOGIN` event to Discord and the generic webhook, so you can finish the login away from the machine. The message contains the URL, the code, and how long the code lasts, and nothing else about the account; it is not anonymized, because the login cannot be completed without them. The usual event filters apply, and the miner logs one warning at startup if no configured target would deliver the event. Read once at startup. Existing config files are not rewritten to add it, so a rollback to an older release can still read them. See [device login notifications](operations.md#device-login-notifications). |
| `timezone` | `null` | IANA time zone for timestamps and for the day boundary of the daily report and the status block's "today", such as `Europe/Athens`. `null` uses the host's local time. |
| `privacy.anonymize_logs` | `true` | Replace streamer names with aliases and hide channel, event and outcome IDs, titles, channel balances, and results in logs and Discord messages. The points of each reward and your own total points gained, in the session summary and the daily report, stay visible. The saved log is named `miner.log` instead of after the account. |
| `webhook.url` | `""` | Any HTTP(S) endpoint that should receive events, such as ntfy, Gotify, or Home Assistant. Each event is a POST with the JSON body `{"event": "DROP_CLAIM", "message": "..."}`. Empty disables it. Treat it as a secret. |
| `webhook.events` | `[]` | Which events to send, from the same names as `discord.events`. |
| `discord.webhook_api` | `""` | Discord webhook URL. Empty disables Discord. Treat it as a secret. |
| `discord.events` | `[]` | Events to send. An empty list sends all events. |

Discord event names: `STARTUP`, `SHUTDOWN`, `STREAMER_ONLINE`,
`STREAMER_OFFLINE`, `GAIN_FOR_WATCH`, `GAIN_FOR_WATCH_STREAK`,
`GAIN_FOR_CLAIM`, `GAIN_FOR_RAID`, `BONUS_CLAIM`, `MOMENT_CLAIM`, `JOIN_RAID`,
`DROP_CLAIM`, `DROP_STATUS`, `BET_START`, `BET_GENERAL`, `BET_FILTERS`,
`BET_WIN`, `BET_LOSE`, `BET_REFUND`, `BET_FAILED`, `CHAT_MENTION`,
`WATCH_STATUS`, `SESSION_LOST`, `EARNINGS_STALL`, `DROPS_STALL`,
`DAILY_REPORT`, `CONFIG_RELOAD`, and `DEVICE_LOGIN`. `EARNINGS_STALL`,
`DROPS_STALL`, and `DAILY_REPORT` are described under
[earnings ledger and alarms](operations.md#earnings-ledger-and-alarms),
`CONFIG_RELOAD` under [config reload](operations.md#config-reload), and
`DEVICE_LOGIN` under
[device login notifications](operations.md#device-login-notifications); like the
others, an empty list sends them.

## Network and proxy

| Setting | Default | Meaning |
| --- | --- | --- |
| `proxy` | `""` | Opt-in. A proxy that carries every connection the miner makes, so Twitch and your notification targets see the proxy's address instead of yours. Empty means no proxy. Read once at startup, and never shown in logs, notifications, `--status`, the reload report, or the support bundle. Existing config files are not rewritten to add it, so a rollback to an older release can still read them. |

**Formats.** Two forms are accepted, each with an optional `user:password@` and a
required host and port:

- `socks5h://host:1080` or `socks5h://user:password@host:1080`: SOCKS5, where
  the proxy resolves host names.
- `http://host:3128` or `http://user:password@host:3128`: an HTTP proxy that
  tunnels with `CONNECT`.

Percent-encode characters such as `@`, `:` or `/` in the user name or password
(`p%40ss` for `p@ss`). `socks5://` is rejected, because it resolves names on
this machine before asking the proxy and so leaks every Twitch host name to your
local resolver; use `socks5h://`. `https://` proxies and other schemes are
rejected too. `--check-config` and every startup validate the value, and the
error says what is wrong without repeating the value.

**No direct fallback.** With a proxy configured, every connection goes through
it: the Twitch and Discord HTTP requests (including auth and playback), the
PubSub, Hermes and EventSub WebSockets, and IRC chat. If the proxy is down,
refuses a connection, rejects the credentials, or does not answer in time (15
seconds for the WebSocket and chat handshakes), that connection fails and is
retried the way any failed connection is. It is never retried directly, so your
address is not exposed. The [operations guide](operations.md#using-a-proxy)
shows how to confirm this.

**Environment variables are no longer used.** Some connections used to follow
`HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` through the HTTP library's defaults.
Since this release no connection reads them, whatever they hold: only `proxy`
counts. If you relied on them, put the same address in `proxy`. When one is set
and `proxy` is empty, the miner logs a warning at startup naming the variable,
because it is connecting directly.

**What a proxy cannot do.** The proxy's own host name is resolved on this
machine; use an IP address to avoid even that lookup. Playback and other
requests to URLs Twitch supplies are normally checked against the addresses their
host names resolve to, to refuse private destinations. Through a proxy the proxy
does the resolving, so that address check cannot apply; the other checks on those
URLs (HTTPS only, no credentials, no `localhost` or `.local` names, no private
IP literals) and the ban on redirects still do. An `http://` proxy sees the
destination host and port of each connection but not its contents, which stay
encrypted, and receives its own `user:password` unencrypted, as does a SOCKS5
proxy, so use credentials only on a network you trust.

## Predictions (`bet`)

Every `bet` field accepts `null`, which means the default shown here.

| Setting | Default | Meaning |
| --- | --- | --- |
| `strategy` | `SMART` | Which outcome to back; see the next table. |
| `percentage` | `5` | Percent of your balance to stake, `0`-`100`. |
| `percentage_gap` | `20` | Threshold used by `SMART`, `0`-`100`. |
| `max_points` | `50000` | Largest stake. Values above `250000` are rejected. |
| `minimum_points` | `0` | Bet only while your balance is above this. |
| `stealth_mode` | `false` | Keep the stake 1-5 points below the largest single bet on the chosen outcome, so you never become its top predictor. |
| `deduct_stake_on_place` | `true` | Subtract the stake from the tracked balance as soon as the bet is placed. |
| `delay_mode` | `FROM_END` | When to bet: `FROM_START` (`delay` seconds after the prediction opens), `FROM_END` (`delay` seconds before it closes), or `PERCENTAGE` (at fraction `delay` of the prediction window). |
| `delay` | `6` | Seconds, or a fraction from `0` to `1` for `PERCENTAGE`. Must be finite and non-negative. |
| `filter_condition` | none | Skip the bet unless a condition holds; see below. |

Each stake is also bounded by Twitch's 10-point minimum and 250,000-point
per-viewer maximum.

| Strategy | Backs |
| --- | --- |
| `MOST_VOTED` | The outcome with the most users. |
| `HIGH_ODDS` | The outcome with the highest odds. |
| `PERCENTAGE` | The outcome with the highest odds percentage. |
| `SMART_MONEY` | The outcome holding the single largest bet. |
| `SMART` | The highest-odds outcome when the top two outcomes' user shares are within `percentage_gap` points, otherwise the outcome with the most users. |
| `NUMBER_1` ... `NUMBER_8` | That outcome by position, or the highest-odds outcome when it does not exist. |

`filter_condition` compares one value with `value` and skips the bet unless the
comparison holds. It applies only when `value` is set.

- `by`: `TOTAL_USERS` or `TOTAL_POINTS` (summed over all outcomes), or
  `DECISION_USERS`, `DECISION_POINTS`, `PERCENTAGE_USERS`, `ODDS`,
  `ODDS_PERCENTAGE`, or `TOP_POINTS` (for the chosen outcome).
- `where`: `GT`, `LT`, `GTE`, or `LTE`.
- `value`: a finite number.

## Per-channel overrides

`streamer_overrides` maps a channel login to settings that apply to that channel
only. An override may set `make_predictions`, `follow_raid`, `farm_drops`,
`claim_drops`, `watch_one_stream_when_drops_active`, `claim_moments`,
`watch_streak`, `watch_streak_vod_recovery`, `community_goals`,
`chat_presence`, and a partial `bet` object; anything it omits is inherited.
`watch_streak` exists only here: streak handling is on for every channel unless
an override turns it off.

`points_limit` also exists only here. It is a positive whole number, and the
file is rejected with the key's exact path otherwise. Once your known balance on
that channel reaches it, the channel gets no watch slot for points; it can still
be watched for a Drop campaign. Claims, presence, and everything else carry on,
and the channel returns to the rotation when the balance drops below the limit.
An unknown balance never counts as reached. Like the opt-in top-level keys, it
is never filled into an existing override, so older releases can still read the
file.

```json
"streamer_overrides": {
  "some_streamer": {
    "make_predictions": true,
    "bet": { "strategy": "MOST_VOTED", "percentage": 2 }
  }
}
```

## Schema version

`config_schema_version` is `1`. Migration adds it to older files, and a file
with a newer version is rejected rather than overwritten.
