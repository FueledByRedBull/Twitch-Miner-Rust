# Operator Guide

## Local Run

Use the workspace root and point the app at the checked-in data directory:

```powershell
cd C:/Users/ancha/Documents/Projects/TwitchMiner/Twitch-Miner-Rust
cargo run -p tm-app -- --config ./data/config.json --data-dir ./data
```

If you want to watch logs in the foreground, run the command directly in the terminal. If you want a background process, redirect `stdout` and `stderr` to `run.out.log` and `run.err.log` and tail them with `Get-Content -Wait`.

## First-Time Login

- Set `username` in `data/config.json`.
- Keep `auto_update=false` if you want to stay on the local source build.
- Leave `password` empty; device-code login does not use it.
- Start the app and open the Twitch activation URL shown in the console.
- Enter the device code and wait for cookie persistence under `data/cookies/<username>.json`.

## Docker Run

The repo ships two compose examples:

- `docker-compose.yml` for a bind-mount layout at `./data`.
- `deploy/docker-compose.volume.yml` for a named-volume layout.

The container expects:

- `TCPM_CONFIG=/data/config.json`
- `TCPM_DATA_DIR=/data`

## Stop And Restart

- The app listens for `CTRL-C` on Windows and `SIGTERM` in containers.
- Use `Stop-Process` for a local PowerShell session if you started the miner in the background.
- In Docker, keep `init: true` and a non-zero stop grace period.

## Multi-Arch Builds

Use `scripts/build-multiarch.ps1` from a machine with Docker and buildx installed. The script targets `linux/amd64`, `linux/arm64`, and `linux/arm/v7`.

```powershell
cd C:/Users/ancha/Documents/Projects/TwitchMiner/Twitch-Miner-Rust
./scripts/build-multiarch.ps1 -Push
```

## Notes

- Treat `data/cookies/<username>.json` as an authentication secret.
- This is unofficial Twitch automation; prefer a dedicated account if account risk matters.
- The local workspace here was validated with `cargo test` and `cargo clippy`.
- Docker and Raspberry Pi smoke validation were not performed in this environment.
