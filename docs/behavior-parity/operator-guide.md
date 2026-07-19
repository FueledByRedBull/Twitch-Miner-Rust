# Operator Guide

## Local Run

Use the workspace root and point the app at the checked-in data directory:

```powershell
cd Twitch-Miner-Rust
cargo run -p tm-app -- --config ./data/config.json --data-dir ./data
```

If you want to watch logs in the foreground, run the command directly in the terminal. If you want a background process, redirect `stdout` and `stderr` to `run.out.log` and `run.err.log` and tail them with `Get-Content -Wait`.

## First-Time Login

- Set `username` in `data/config.json`.
- `auto_update` was removed. A legacy `false` value is migrated away and `true`
  is rejected.
- Remove `password` from older configs; device-code login does not use it.
- `claim_moments` is the global moment-discovery/claim default; a streamer
  override can enable or disable it for one channel.
- `farm_drops` controls drop progress and campaign-aware selection, while
  `claim_drops` controls only claim mutations. Leave
  `watch_one_stream_when_drops_active=true` to limit watching to the
  highest-ranked eligible campaign; all three settings support per-streamer
  overrides.
- `watch_streak_vod_recovery` is an opt-in global/per-streamer setting. It uses
  one bounded offline worker, exact missed-broadcast VOD matching where
  available, clip fallback, and immediate live-stream preemption. Accepted
  playback events are progress evidence only; the miner requires a newer typed
  streak milestone before it reports recovery. The private
  `data/streak-cache.json` stores only bounded channel/streak metadata.
- `LONGEST_STREAK` and `EXPIRING_STREAK` may be used in `watch_priority`; both
  stay inside the existing 15-minute live-streak budget.
- Start the app and open the Twitch activation URL shown in the console.
- Enter the device code and wait for cookie persistence under `data/cookies/<username>.json`.
- A saved session starts reauthorization only after a definitive authentication
  rejection. Transient network or Twitch server failures leave the saved
  session untouched and fail startup for the service supervisor to retry.

## Docker Run

The repo ships two compose examples:

- `docker-compose.yml` for a bind-mount layout at `./data`.
- `deploy/docker-compose.volume.yml` for a named-volume layout.
- `deploy/docker-compose.rpi.yml` for a Raspberry Pi bind-mount layout that runs as the host user.

The container expects:

- `TCPM_CONFIG=/data/config.json`
- `TCPM_DATA_DIR=/data`

For automation and diagnostics:

```powershell
docker exec twitch-miner /twitch-miner --check-config --json --data-dir /data
docker exec twitch-miner /twitch-miner --status --data-dir /data
docker exec twitch-miner /twitch-miner --health --data-dir /data
```

`--status` prints only the sanitized runtime-status document. It includes task
freshness, bounded claim/bet/reconnect/refresh counters, the last redacted error
class, runtime queue/processing measurements, EventSub planned/active/cost
capabilities, and PubSub configured/acknowledged/message/reconnect capabilities.
It never prints topic suffixes, channel/user IDs, cookies,
tokens, request headers, or raw account payloads. `followers_order` accepts
`ASC` or `DESC`; `DESC` remains the default.

Human-facing console and saved logs use the Python-compatible envelope
`HH:MM:SS DD/MM/YY - LEVEL - [operation]: message` (seconds remain controlled
by `show_seconds`). High-value events use stable operations such as `run`,
`set_online`, `set_offline`, `on_message`, `claim_bonus`, `update_raid`, and
`make_predictions`, with the familiar emoji messages when `emojis` is enabled.

On normal shutdown the miner prints the session ID, saved log path, duration in
`HH:MM:SS.ffffff`, a bounded detailed report for completed predictions, and a
per-streamer point/history summary. Report blocks use the Python final-report
shape `HH:MM:SS DD/MM/YY - emoji/content` without the ordinary level/operation
envelope. Privacy mode substitutes streamer aliases,
hides channel/event/outcome IDs, titles, points, decisions, and result details,
and uses `miner.log` rather than an account-named log file.

Published images are static Rust binaries in a `scratch` runtime. There is no shell, package manager, or OS certificate bundle inside the image; TLS trust is provided by the Rust TLS stack.

If you are migrating a Linux bind mount from an older root-run image, make sure existing `config/`, `cookies/`, and `log/` files are readable and writable by the UID/GID configured in Compose before restarting the Rust container.

## Stop And Restart

- The app listens for `CTRL-C` on Windows and `SIGTERM` in containers.
- Use `Stop-Process` for a local PowerShell session if you started the miner in the background.
- In Docker, keep `init: true` and a non-zero stop grace period.

## Multi-Arch Builds

Use `scripts/build-multiarch.ps1` from a machine with Docker and buildx installed. Without `-Push`, the script builds and loads one local-platform image for smoke testing. With `-Push`, it builds and publishes `linux/amd64`, `linux/arm64`, and `linux/arm/v7`, matching the GitHub Actions workflow.

```powershell
cd Twitch-Miner-Rust
./scripts/build-multiarch.ps1
docker run --rm twitch-miner-rust:local --help
./scripts/build-multiarch.ps1 -Push
```

On pushes to `main`, GitHub Actions builds, smoke-tests, and publishes the
multi-architecture GHCR image. A signed `v*` tag promotes the already-tested
manifest for that exact commit without rebuilding it. Deploy the recorded
manifest digest, not `latest`; see [release-process.md](../release-process.md).

## Go/Rust Parity Gate

The normalized vectors in `tests/parity/vectors.json` cover common streamer
settings, prediction decisions and settlements, point-history updates, watch
selection, and a legacy PubSub point event. Rust runs them through the contract
test package. `scripts/verify-go-baseline.ps1` temporarily copies the matching
Go test harness into the pinned baseline checkout, runs the same vectors, and
removes the generated files before returning. The gate fails if either
implementation diverges; it does not read credentials or live Twitch data.

## Transport health

EventSub and PubSub are separate health entries. EventSub `unauthorized`,
`revoked`, or `no-subscriptions` means the official subscription path needs
authorization or contract attention; `rate-limited`, `server-error`,
`connection-reset`, and `timeout` are bounded recovery states. Presence entries
whose source is `gql-polling` are intentionally covered by the fallback poller.
One fallback cycle may query many streamers, but health counts that batch as a
single success or failure so a brief shared network outage cannot exhaust the
consecutive-failure threshold in one minute.

PubSub `bad-auth` requires a fresh session. `listen-rejected` means one topic
class was not accepted. `pong-timeout`, `connection-error`, `connection-closed`,
and `reconnect` trigger bounded independent reconnection. Check the redacted
per-class configured versus acknowledged counts; do not copy cookies or raw
responses into support reports.

## Notes

- Treat `data/cookies/<username>.json` as an authentication secret.
- This is unofficial Twitch automation; prefer a dedicated account if account risk matters.
- Use `tm-app --check-config` before a migration, `--health` after startup, and
  `--status` for sanitized diagnostics and `--support-bundle ./support.json`
  for a privacy-safe support artifact. Use `--check-config --json` in scripts.
