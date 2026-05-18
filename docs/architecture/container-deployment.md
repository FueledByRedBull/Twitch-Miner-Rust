# Container Deployment

The container is designed around a single bind mount at `/data`.

Runtime paths:

- Config: `/data/config.json`
- Cookies: `/data/cookies/<username>.json`
- Logs: `/data/log/<username>.log`

The app reads `TCPM_DATA_DIR` first, so the recommended container setup is to mount `/data` and let the miner create its own config, cookies, and log directories there.

The published runtime image is a static Rust binary copied into `scratch`. It contains no shell, package manager, or OS certificate bundle. TLS trust comes from the Rust TLS dependencies, not from a system CA package.

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
./scripts/build-multiarch.ps1
docker run --rm ghcr.io/fueledbyredbull/twitch-miner-rust:latest --help
./scripts/build-multiarch.ps1 -Push
```

The helper script builds and loads one local-platform image by default. Its `-Push` mode uses the same `linux/amd64`, `linux/arm64`, and `linux/arm/v7` platform set as `.github/workflows/multiarch-build.yml`. GitHub Actions publishes GHCR images on pushes to `main` and `v*` tags after platform smoke tests pass.
