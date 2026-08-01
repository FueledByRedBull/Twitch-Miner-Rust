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
- it initializes a missing `farm_drops` field from the legacy `claim_drops`
  value, including per-streamer overrides, and adds the default single-watcher
  campaign control;
- it removes a legacy `auto_update=false` field; and
- it writes `<config>.bak` before the atomic replacement.

`auto_update=true` is rejected and must be removed manually. A newer schema
version is also rejected rather than overwritten. Cookie files are decoded from
the current JSON map and legacy JSON record-list shapes only; changed cookie
files are atomically replaced and retain a `.bak` copy of their previous
content. The application never copies data out of the configured directory.

Python cookie jars are outside that migration boundary. They are commonly
Python pickle files, and loading a pickle can execute code. The Rust miner does
not inspect or deserialize them and does not include a pickle runtime or parser.
When moving from the Python miner, preserve its cookie file unchanged as a
private backup outside the active Rust data mount, then complete Rust's device
login so it creates `data/cookies/<username>.json`. Do not auto-convert, rename,
or delete the old secret file. Keep both the old backup and the new JSON session
private; never upload or commit either one.

If a migration fails, stop the Rust container, restore the `config.json.bak`
or cookie `.bak` file in the mounted data directory, and restart the previous
digest. Preserve the backups until the new digest has passed the monitoring
window.
