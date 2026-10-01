# Operations guide

How to run, update, and monitor the miner. First-time setup is in the
[README quick start](../README.md#quick-start-docker), and every setting is
described in the [configuration reference](configuration.md).

Run only one miner per Twitch account. Concurrent instances are not a supported
high-availability mode: they duplicate transport subscriptions, watch
heartbeats, and mutation decisions instead of sharing one runtime state.

## Running with Docker

The repository ships three Compose files:

- [`docker-compose.yml`](../docker-compose.yml) builds the image locally and
  bind-mounts `./data`.
- [`deploy/docker-compose.bind-mount.yml`](../deploy/docker-compose.bind-mount.yml)
  runs a published image with a bind-mounted data directory as the host user.
- [`deploy/docker-compose.volume.yml`](../deploy/docker-compose.volume.yml)
  runs a published image with a named volume.

`docker compose config --quiet` validates a Compose file without starting a
container or contacting Twitch. When building locally with `--build`, leave
`TWITCH_MINER_IMAGE` unset or set it to a tag; a digest cannot tag locally built
output.

### Data directory and container contract

Mount one directory at `/data` and keep `config.json` in it. The miner creates
`cookies/` and `log/` under the same root:

- `/data/config.json`
- `/data/cookies/<username>.json`
- `/data/log/*.log`

The container sets `TCPM_CONFIG=/data/config.json` and `TCPM_DATA_DIR=/data`.

Images contain a static Rust binary in a `scratch` runtime, with no shell,
package manager, or OS certificate bundle; TLS trust comes from the Rust TLS
stack. Run commands through the Compose service name, which does not depend on
the generated container name:

```sh
docker compose exec -T twitch-miner /twitch-miner --status --data-dir /data
```

A direct `docker exec` of `/twitch-miner` also works when you supply the actual
container name or ID; `twitch-miner` is not guaranteed to be that name outside
Compose. `docker exec ... sh` or `bash` cannot work, so use logs, the mounted
`/data` files, and host Docker tooling for inspection.

### Permissions on Linux

The Compose files run the container as a non-root UID/GID (`1000:1000` by
default in `docker-compose.yml`; the published bind-mount example pins the host
user). The mounted data directory and any existing cookie and log files must be
writable by that user. When moving from an older image that ran as root, a
one-time `chown` of the existing `config.json`, `cookies/`, and `log/` is
usually needed before the container can reuse saved cookies.

### Stopping and restarting

The miner shuts down cleanly on `SIGTERM` in containers and `CTRL-C` in a
terminal. Keep `init: true` and a short but non-zero stop grace period in
Compose. The image health check runs `--health`, which requires the runtime
heartbeat to stay fresh.

## Deploying a published image

On pushes to `main`, GitHub Actions builds, smoke-tests, signs, and publishes a
multi-architecture image for AMD64 and ARM64 to GHCR. A release promotes that
exact manifest only after its canary, soak, rollback, and required-check
evidence is approved; see the [release process](release-process.md).

Deploy by the recorded manifest digest, never `latest`. The published Compose
files keep the bind mount, read-only filesystem, dropped capabilities, restart
policy, stop grace, and health check:

```sh
export TWITCH_MINER_IMAGE='ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<recorded-digest>'
docker compose config --quiet
docker compose pull twitch-miner
docker compose up -d --no-build twitch-miner
docker compose exec -T twitch-miner /twitch-miner --health
```

<details>
<summary>Windows (PowerShell)</summary>

```powershell
$env:TWITCH_MINER_IMAGE = 'ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<recorded-digest>'
docker compose config --quiet
docker compose pull twitch-miner
docker compose up -d --no-build twitch-miner
docker compose exec -T twitch-miner /twitch-miner --health
```

</details>

Pass `-f deploy/docker-compose.bind-mount.yml` or
`-f deploy/docker-compose.volume.yml` to use the published-image layouts. For a
guarded update that checks the candidate and keeps a rollback ready, follow the
release process. To roll back, set `TWITCH_MINER_IMAGE` to the previous recorded
digest and run `docker compose up -d twitch-miner`.

## Running on Windows

The release workflow builds a self-contained portable ZIP and an MSI. The MSI
installs only the executable and documentation under Program Files; it does not
create configuration, cookie, or status files there. Keep runtime data in a
user-writable directory such as `%LOCALAPPDATA%\TwitchMiner`, under your
private profile, and pass it with `--data-dir` to every command, including
`--check-config`, `--status`, and `--health`. Release artifacts are unsigned
unless a separate signing record is published.

To watch logs, run the miner in the foreground. For a background process,
redirect `stdout` and `stderr` to `run.out.log` and `run.err.log`, follow them
with `Get-Content -Wait`, and stop the process with `Stop-Process`.

## Sessions and login

On first start the miner prints a device code: open
<https://www.twitch.tv/activate>, enter the code, and the session is saved to
`cookies/<username>.json` in the data directory. Treat that file like a
password.

A saved session starts reauthorization only after a definitive authentication
rejection. Transient network or Twitch server failures leave the session
untouched and retry validation in-process with capped backoff; the wait can be
interrupted with `CTRL-C` or `SIGTERM`.

## Diagnostics

| Command | Use |
| --- | --- |
| `--check-config` | Validate the configuration and preview migrations without contacting Twitch. Use it before migrating. Add `--json` for scripts. |
| `--health` | Check that the running miner's tasks are fresh. Docker uses this as its health check; run it after starting or updating. |
| `--status` | Print the sanitized runtime status described below. |
| `--support-bundle <path>` | Write version, status, and file-count metadata without cookies, configuration values, or log contents. |

```sh
docker compose exec -T twitch-miner /twitch-miner --check-config --json --data-dir /data
docker compose exec -T twitch-miner /twitch-miner --status --data-dir /data
docker compose exec -T twitch-miner /twitch-miner --health --data-dir /data
```

The running process rewrites `runtime-status.json` in the data directory;
`--status` prints it. Never include cookies, request headers, endpoint query
strings, or raw responses in a support report.

### Reading `--status`

The status document never contains topic suffixes, channel or user IDs,
cookies, tokens, request headers, or raw account payloads.

**Tasks and counters.** Each task reports its last successful work, its last
activity, consecutive failures, and the last redacted error class. Counters
cover claims, bets, reconnects, and refreshes; `successful_refreshes` counts
complete channel-points context refresh cycles, not OAuth token refreshes.
Runtime measurements cover queue depth, processed events, and local
transport-to-state latency.

**Watch slots.** Each watched channel is `measurement_unavailable`,
`awaiting_first_credit`, `first_credit_overdue`, `earning`, or `stalled`.
`progress_age_seconds` is monotonic time, observed while measurement is valid,
since the first-credit wait began or the last confirmed point changed. Earning
requires a confirmed credit during the current selection visit and broadcast;
a credit from an earlier visit does not count after reselection, and lost
measurement, deselection, or a changed broadcast resets the wait. After thirty
minutes without a first credit, a slot is marked overdue and a warning is
logged at most once per 30 minutes across both slots, listing every overdue
slot and its wait. Add `WATCH_STATUS` to `discord.events` to receive these
warnings (an empty list sends all events). Overdue slots do not rotate,
increment failure counters, or fail health.

**Transports.** EventSub, PubSub, and presence polling report separately.
EventSub shows planned, active, and failed subscriptions, `total_cost`,
`max_total_cost`, and `overflow_streamers`, and each channel records its
`presence_source`, `prediction_source`, `raid_source`, and `failure_class`.
`capacity-overflow` is the designed outcome when Twitch's subscription budget
is full: the channel uses GQL polling or the PubSub compatibility path instead,
and it can also mean that only an optional raid or prediction subscription was
skipped. `raid_source` names EventSub only when a `channel.raid` subscription was
actually allocated. `verified=false` means the current set has not been
verified, not that setup is unhealthy; judge health from active and failed
counts, recheck warnings, and task state. PubSub reports configured and
acknowledged topics, message times, and reconnects. The exact transport rules
are in the [protocol inventory](protocol-inventory.md#transports).

**Prediction journal.** `prediction_journal` reports `bytes`, `byte_limit`,
`unresolved_count`, `unresolved_limit`, `retained_count`, and
`capacity_blocked`. At capacity, new predictions are refused safely. Expired
resolved records are pruned during normal use while unknown outcomes stay
protected; never delete recent records or restore an older journal to make
room. The limits are described under
[mutation safety](protocol-inventory.md#mutation-safety).

## Logs and storage

Console and saved logs use the format
`HH:MM:SS DD/MM/YY - LEVEL - [operation]: message`, with seconds controlled by
`show_seconds`. Important events use stable operation names such as `run`,
`set_online`, `set_offline`, `on_message`, `claim_bonus`, `update_raid`, and
`make_predictions`, with emoji when `emojis` is enabled.

On normal shutdown the miner prints the session ID, saved log path, duration as
`HH:MM:SS.ffffff`, a bounded report of completed predictions, and a per-streamer
points and history summary. Report lines use the shape
`HH:MM:SS DD/MM/YY - emoji/content` without the level and operation. With
`privacy.anonymize_logs`, streamer names become aliases; channel, event and
outcome IDs, titles, points, decisions, and results are hidden; and the log
file is named `miner.log`.

Writes are bounded. `runtime-status.json` is replaced atomically on the
30-second supervision heartbeat, `streak-cache.json` is flushed every 30
seconds only when it changed, and saved logs rotate at 10 MiB, keep at most
five archives, and prune archives older than 30 days. On flash storage such as
an SD card, keep `/data` persistent for configuration and sessions, and move it
to more durable storage if that write rate is still too high. Never put the
cookie directory on ephemeral storage.

## Building images yourself

`scripts/build-multiarch.ps1` needs Docker with buildx. Without `-Push` it
builds and loads one image for the local platform for smoke testing. With
`-Push` it builds and publishes `linux/amd64` and `linux/arm64` under a
revision-scoped `candidate-<full-sha>` tag; stable tags belong to the protected
promotion workflow. ARMv7 is not supported.

```powershell
./scripts/build-multiarch.ps1
docker run --rm twitch-miner-rust:local --help
./scripts/build-multiarch.ps1 -Push
```
