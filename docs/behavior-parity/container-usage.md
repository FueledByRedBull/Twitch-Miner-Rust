# Container Usage

Recommended layout:

- Bind mount `/data` for the miner state.
- Keep `config.json` in that directory.
- Let the app create `cookies/` and `log/` under the same root.

The published image is a static Rust binary in a `scratch` runtime. It has no shell, package manager, or OS certificate bundle, so operational debugging should use logs, mounted `/data` files, and the host Docker tooling rather than `docker exec` shell sessions.

Example:

```yaml
services:
  twitch-miner:
    image: ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<recorded-digest>
    user: "${UID:-1000}:${GID:-1000}"
    environment:
      TCPM_CONFIG: /data/config.json
      TCPM_DATA_DIR: /data
    volumes:
      - ./data:/data
```

Named-volume example:

```yaml
services:
  twitch-miner:
    image: ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<recorded-digest>
    user: "${UID:-1000}:${GID:-1000}"
    environment:
      TCPM_CONFIG: /data/config.json
      TCPM_DATA_DIR: /data
    volumes:
      - twitch-miner-data:/data

volumes:
  twitch-miner-data:
```

The Raspberry Pi compose example pins `linux/arm64` and follows the same `/data` convention. The miner exits on `SIGTERM`, so Compose should be given a short but non-zero stop grace period. The image health check executes `--health` and requires the runtime heartbeat to remain fresh.

On Linux bind mounts, the mounted directory and any existing cookie/log files must be writable by the configured container UID/GID. If you are migrating from an older root-run image, a one-time `chown` of the data directory may be required before the Rust container can reuse saved cookies.

On pushes to `main`, GitHub Actions builds, smoke-tests, and publishes the multi-architecture GHCR image. A signed `v*` tag promotes the already-tested manifest for that exact commit without rebuilding it. Set `TWITCH_MINER_IMAGE` to the recorded manifest digest before using either published Compose example; `latest` is not a deployment input.

For a shorter operator-oriented checklist, see [operator-guide.md](operator-guide.md).
