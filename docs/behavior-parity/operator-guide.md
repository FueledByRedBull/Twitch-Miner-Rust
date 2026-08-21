# Operator Guide

## Local Run

`config.example.json` is a credential-free tracked template. The runtime
configuration belongs in the ignored `data/` directory, so a fresh clone must
copy the template before starting the app:

```powershell
cd Twitch-Miner-Rust
New-Item -ItemType Directory -Force ./data | Out-Null
Copy-Item ./config.example.json ./data/config.json
notepad ./data/config.json
cargo run -p tm-app -- --config ./data/config.json --data-dir ./data --check-config
cargo run -p tm-app -- --config ./data/config.json --data-dir ./data
```

Replace `your_twitch_login` and `your_twitch_streamer` in the copied file
before running the validation command. `--check-config` is local and
network-free; the final command performs device-code login when no saved
session exists.

The app can also create a missing config from built-in defaults and extend an
existing file during migration. Treat that as a runtime fallback, not a second
starter template: use the tracked `config.example.json` when you want a
reproducible manual setup, then inspect generated or migrated values with
`--check-config`.

If you want to watch logs in the foreground, run the command directly in the terminal. If you want a background process, redirect `stdout` and `stderr` to `run.out.log` and `run.err.log` and tail them with `Get-Content -Wait`.

## First-Time Login

- Set `username` and the initial `streamers` list in the copied
  `data/config.json`; both are placeholders in the tracked example.
- `auto_update` was removed. A legacy `false` value is migrated away and `true`
  is rejected.
- Empty legacy passwords are migrated away; a non-empty password is rejected
  because device-code login does not use it. The obsolete secure-default TLS
  flag and unused watch-queue logging flag are also migrated away.
- `farm_drops`, `claim_drops`, `claim_moments`, streak recovery, campaign
  selection, and prediction settings support the global/per-streamer boundaries
  documented in the [parity matrix](parity-matrix.md). Their network and
  scheduling contracts are defined once in the
  [protocol inventory](../protocol-inventory.md).
- `betting(make_predictions)` is the historical Go/Python field name retained
  for config compatibility. The tracked template leaves it `false`; set it to
  `true` only when prediction betting is intended. Do not rename the key.
- `LONGEST_STREAK` and `EXPIRING_STREAK` may be used in `watch_priority`; both
  stay inside the evidence-based ten-minute live-streak budget. The budget is
  internal policy, not an operator setting; campaign pinning and the 15-minute
  all-channel fair-rotation window are separate rules.
- Start the app and open the Twitch activation URL shown in the console.
- Enter the device code and wait for cookie persistence under `data/cookies/<username>.json`.
- A saved session starts reauthorization only after a definitive authentication
  rejection. Transient network or Twitch server failures leave the saved
  session untouched and retry validation in-process with capped backoff. The
  wait remains interruptible by `CTRL-C` or container `SIGTERM`.

## Docker Run

The repo ships three Compose examples:

- `docker-compose.yml` for a bind-mount layout at `./data`.
- `deploy/docker-compose.volume.yml` for a named-volume layout.
- `deploy/docker-compose.bind-mount.yml` for a published-image bind-mount layout
  that runs as the host user.

For the bind-mount example, prepare the ignored data directory from the
tracked template before starting the service, then validate Compose without
starting a container:

```powershell
New-Item -ItemType Directory -Force ./data | Out-Null
Copy-Item ./config.example.json ./data/config.json
notepad ./data/config.json
docker compose config --quiet
docker compose up --build
```

Run the local `tm-app --check-config` command above after editing the copied
file when you want config validation as well. Neither check contacts Twitch or
requires an authenticated session.

Run only one active miner instance per Twitch account. Concurrent instances are
not a supported high-availability mode: they duplicate transport subscriptions,
watch heartbeats, and mutation decisions instead of sharing one runtime state.

The container expects:

- `TCPM_CONFIG=/data/config.json`
- `TCPM_DATA_DIR=/data`

For automation and diagnostics:

```powershell
docker compose exec -T twitch-miner /twitch-miner --check-config --json --data-dir /data
docker compose exec -T twitch-miner /twitch-miner --status --data-dir /data
docker compose exec -T twitch-miner /twitch-miner --health --data-dir /data
```

These commands target the Compose service, so they do not depend on the
generated container name. The image is `scratch`: direct `docker exec` of
`/twitch-miner` also works when you provide the actual container name or ID,
but there is no `sh` or `bash` to start for an interactive shell.

`--status` prints only the sanitized runtime-status document. It includes each
task's last successful work and last activity, bounded
claim/bet/reconnect/refresh counters, the last redacted error class, runtime
queue/processing measurements, EventSub planned/active/cost capabilities, and
PubSub configured/acknowledged/message/reconnect capabilities.
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

Persistent writes are bounded. `runtime-status.json` is atomically replaced on
the 30-second supervision heartbeat, and `streak-cache.json` is flushed on its
30-second timer only when state changed. Saved logs are optional; each active
log rotates at 10 MiB, retains at most five archives, and prunes archives older
than 30 days. On flash-backed hosts, keep `/data` persistent for configuration
and sessions, and move the bind mount to storage that meets the host's endurance
requirements if that bounded write rate is still unsuitable. Do not place the
cookie directory on ephemeral storage.

If you are migrating a Linux bind mount from an older root-run image, make sure existing `config/`, `cookies/`, and `log/` files are readable and writable by the UID/GID configured in Compose before restarting the Rust container.

## Stop And Restart

- The app listens for `CTRL-C` on Windows and `SIGTERM` in containers.
- Use `Stop-Process` for a local PowerShell session if you started the miner in the background.
- In Docker, keep `init: true` and a non-zero stop grace period.

## Multi-Arch Builds

Use `scripts/build-multiarch.ps1` from a machine with Docker and buildx installed. Without `-Push`, the script builds and loads one supported local-platform image for smoke testing. With `-Push`, it builds and publishes `linux/amd64` and `linux/arm64`, matching the GitHub Actions workflow. ARMv7 is not supported.

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

For behavior-level differences and limits, including the typed
playback-token/HLS preflight before minute-watch submission, see the
[parity matrix](parity-matrix.md). This guide keeps the operational steps
without duplicating that comparison.

## Transport health

Use `--status` for the separate EventSub, PubSub, and polling health entries.
The authoritative timeout, retry, fallback, and mutation-replay rules are in the
[protocol inventory](../protocol-inventory.md); never include cookies, request
headers, endpoint query strings, or raw responses in a support report.

## Notes

- Treat `data/cookies/<username>.json` as an authentication secret.
- This is unofficial Twitch automation; prefer a dedicated account if account risk matters.
- Use `tm-app --check-config` before a migration, `--health` after startup, and
  `--status` for sanitized diagnostics and `--support-bundle ./support.json`
  for a privacy-safe support artifact. Use `--check-config --json` in scripts.
