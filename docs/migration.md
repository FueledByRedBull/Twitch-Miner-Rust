# Go-To-Rust Data Migration

The Rust miner can reuse the Go layout when its data directory is mounted at
`/data`, including a legacy host directory named `twitch-miner-go`. The name is
only a host path; it does not select the Go implementation.

Before changing containers, copy the data directory somewhere outside the
mount. Do not upload or commit the copy: it contains cookies.

```sh
docker run --rm -v /host/twitch-miner-go:/data:ro <image-digest> \
  --data-dir /data --check-config
```

`--check-config` validates and previews the migration without writing. A
normal Rust startup performs a versioned config migration only when necessary:

- it adds `config_schema_version`;
- it fills missing supported defaults;
- it removes a legacy `auto_update=false` field; and
- it writes `<config>.bak` before the atomic replacement.

`auto_update=true` is rejected and must be removed manually. A newer schema
version is also rejected rather than overwritten. Cookie files are decoded from
the current and legacy formats; changed cookie files are atomically replaced
and retain a `.bak` copy of their previous content. The application never
copies data out of the configured directory.

If a migration fails, stop the Rust container, restore the `config.json.bak`
or cookie `.bak` file in the mounted data directory, and restart the previous
digest. Preserve the backups until the new digest has passed the monitoring
window.
