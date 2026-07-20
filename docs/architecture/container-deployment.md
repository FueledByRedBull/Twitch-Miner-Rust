# Container Deployment

The container is designed around a single bind mount at `/data`.

Runtime paths:

- Config: `/data/config.json`
- Cookies: `/data/cookies/<username>.json`
- Logs: `/data/log/<username>.log`

The app reads `TCPM_DATA_DIR` first, so the recommended container setup is to mount `/data` and let the miner create its own config, cookies, and log directories there.

The published runtime image is a static Rust binary copied into `scratch`. It contains no shell, package manager, or OS certificate bundle. TLS trust comes from the Rust TLS dependencies, not from a system CA package.

For Linux bind mounts, run the container with a UID/GID that can write the mounted data directory, or pre-chown that directory and any existing cookie files before first startup. The Raspberry Pi example does this with an explicit Compose `user` override.

Shutdown and health:

- The container uses `SIGTERM` for shutdown.
- Compose examples use `init: true` and a `30s` grace period so the miner can flush session state cleanly.
- There is no HTTP health endpoint. The image health check runs `twitch-miner --health`, which validates the runtime heartbeat file in `/data`.
- Transient startup-auth and task recovery stays inside the process with bounded backoff. Docker may report the service unhealthy during an outage, but an active retry loop does not create a restart storm; silent or exited tasks still fail supervision.

Build targets:

- `linux/amd64`
- `linux/arm64`
- `linux/arm/v7`

Example build:

```powershell
./scripts/build-multiarch.ps1
docker run --rm twitch-miner-rust:local --help
./scripts/build-multiarch.ps1 -Push
```

The helper script builds and loads one local-platform image by default. Its `-Push` mode uses the same `linux/amd64`, `linux/arm64`, and `linux/arm/v7` platform set as `.github/workflows/multiarch-build.yml`. On pushes to `main`, GitHub Actions builds, smoke-tests, and publishes the multi-architecture GHCR image. A signed `v*` tag promotes the already-tested manifest for that exact commit without rebuilding it. Use the recorded manifest digest from [release-process.md](../release-process.md) for a deployment.
