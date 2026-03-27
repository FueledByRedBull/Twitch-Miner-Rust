# Container Usage

Recommended layout:

- Bind mount `/data` for the miner state.
- Keep `config.json` in that directory.
- Let the app create `cookies/` and `log/` under the same root.

Example:

```yaml
services:
  twitch-miner:
    image: ghcr.io/0x8fv/twitch-miner-rust:latest
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
    image: ghcr.io/0x8fv/twitch-miner-rust:latest
    environment:
      TCPM_CONFIG: /data/config.json
      TCPM_DATA_DIR: /data
    volumes:
      - twitch-miner-data:/data

volumes:
  twitch-miner-data:
```

The Raspberry Pi compose example pins `linux/arm/v7` and follows the same `/data` convention. The miner exits on `SIGTERM`, so Compose should be given a short but non-zero stop grace period.

For a shorter operator-oriented checklist, see [operator-guide.md](operator-guide.md).
