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
which is secret), optional `log/` files, `streak-cache.json`,
`prediction-placements.json`, and `runtime-status.json`.

## Account and channels

| Setting | Default | Meaning |
| --- | --- | --- |
| `username` | placeholder | Your Twitch login, not your display name: ASCII letters, digits, and underscores, at most 25 characters, normalized to lowercase. Windows device names such as `CON`, `AUX`, `COM1`, and `LPT1` are rejected on every platform so the data directory stays portable. Required. |
| `streamers` | `[]` | Channels to watch. Their order is the `ORDER` priority. When the list is empty, the miner loads up to 100 channels you follow. |
| `streamers_exclude` | `[]` | Logins that are never watched. |
| `followers_order` | `DESC` | Order of followed channels when `streamers` is empty: `ASC` or `DESC`. |
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
| `timezone` | `null` | IANA time zone for timestamps, such as `Europe/Athens`. `null` uses the host's local time. |
| `privacy.anonymize_logs` | `true` | Replace streamer names with aliases and hide channel, event and outcome IDs, titles, points, and results in logs and Discord messages. The saved log is named `miner.log` instead of after the account. |
| `discord.webhook_api` | `""` | Discord webhook URL. Empty disables Discord. Treat it as a secret. |
| `discord.events` | `[]` | Events to send. An empty list sends all events. |

Discord event names: `STARTUP`, `SHUTDOWN`, `STREAMER_ONLINE`,
`STREAMER_OFFLINE`, `GAIN_FOR_WATCH`, `GAIN_FOR_WATCH_STREAK`,
`GAIN_FOR_CLAIM`, `GAIN_FOR_RAID`, `BONUS_CLAIM`, `MOMENT_CLAIM`, `JOIN_RAID`,
`DROP_CLAIM`, `DROP_STATUS`, `BET_START`, `BET_GENERAL`, `BET_FILTERS`,
`BET_WIN`, `BET_LOSE`, `BET_REFUND`, `BET_FAILED`, `CHAT_MENTION`, and
`WATCH_STATUS`.

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
