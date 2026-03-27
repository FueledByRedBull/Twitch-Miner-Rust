# Container Deployment

The container is designed around a single bind mount at `/data`.

Runtime paths:

- Config: `/data/config.json`
- Cookies: `/data/cookies/<username>.json`
- Logs: `/data/log/<username>.log`

The app reads `TCPM_DATA_DIR` first, so the recommended container setup is to mount `/data` and let the miner create its own config, cookies, and log directories there.

Shutdown and health:

- The container uses `SIGTERM` for shutdown.
- Compose examples use `init: true` and a `30s` grace period so the miner can flush session state cleanly.
- There is no HTTP health endpoint. Use the process lifecycle and restart policy as the liveness signal.

Build targets:

- `linux/amd64`
- `linux/arm64`
- `linux/arm/v7`

Example build:

```powershell
docker buildx build --platform linux/amd64,linux/arm64,linux/arm/v7 -t ghcr.io/0x8fv/twitch-miner-rust:latest --push .
```

The same platform set is also captured in `scripts/build-multiarch.ps1` and `.github/workflows/multiarch-build.yml` for repeatable local and CI execution.
