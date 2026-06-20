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
    image: ghcr.io/fueledbyredbull/twitch-miner-rust:latest
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
    image: ghcr.io/fueledbyredbull/twitch-miner-rust:latest
    environment:
      TCPM_CONFIG: /data/config.json
      TCPM_DATA_DIR: /data
    volumes:
      - twitch-miner-data:/data

volumes:
  twitch-miner-data:
```

The Raspberry Pi compose example pins `linux/arm/v7` and follows the same `/data` convention. The miner exits on `SIGTERM`, so Compose should be given a short but non-zero stop grace period.

On Linux bind mounts, the mounted directory and any existing cookie/log files must be writable by the configured container UID/GID. If you are migrating from an older root-run image, a one-time `chown` of the data directory may be required before the Rust container can reuse saved cookies.

GitHub Actions publishes the GHCR image on pushes to `main` and `v*` tags after the multi-arch workflow builds and smoke-tests the platform images.

For a shorter operator-oriented checklist, see [operator-guide.md](operator-guide.md).
