# Release And Rollback Process

Releases are source-and-digest based. Mutable image tags are convenience
labels, never deployment input.

The Docker builder also pins the Rust toolchain to an immutable manifest
digest and pins `cargo-chef` to an exact locked version;
`scripts/verify-release-hygiene.ps1` rejects mutable builder inputs.

1. Update `CHANGELOG.md` with behavior, configuration, migration, and known
   compatibility changes.
2. Run the local QA commands in `CONTRIBUTING.md`, run
   `scripts/verify-go-baseline.ps1` against the pinned Go baseline, then run
   the read-only canary on a dedicated account.
3. Create and push a signed `v*` tag. GitHub Actions runs Rust QA, docs and
   Compose validation, dependency and secret checks, coverage, per-platform
   image smoke tests, SBOM/provenance generation, and a second smoke test of
   the published manifest digest.
4. Retrieve the `published-manifest-digest` artifact. Record that digest,
   source revision, supported platforms, canary result, and rollback digest in
   the release record.
5. Set `TWITCH_MINER_IMAGE` to the exact `ghcr.io/...@sha256:<digest>` value
   and run `docker compose -f deploy/docker-compose.rpi.yml config` before an
   update.
6. After deployment, verify `--version`, `--health`, container health, and a
   normal `SIGTERM` restart. Keep the previous digest until the new deployment
   has remained healthy through its monitoring window.

If the previous image bytes are unavailable, rebuild the known-good source
revision into a new, explicitly named rollback image. The helper never pushes
unless `-Push` is supplied:

```powershell
./scripts/build-rollback-image.ps1 -Revision 1c10f11
./scripts/build-rollback-image.ps1 -Revision 1c10f11 -Push
docker buildx imagetools inspect ghcr.io/fueledbyredbull/twitch-miner-rust:rollback-<resolved-sha>
```

Record the newly produced digest; the old digest cannot be recreated from its
hex string alone. Run `--check-config` against the rollback digest on the Pi
before placing it in the rollback Compose file.

## Raspberry Pi verification commands

Run these on the Pi after checking that the mounted data directory is owned by
the UID/GID used by Compose. Substitute only the recorded manifest digest; do
not use `latest` for deployment.

```sh
export TWITCH_MINER_IMAGE='ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<recorded-digest>'
export DATA_DIR="$PWD/data" # set this to the existing Pi data directory
docker run --rm --platform linux/arm64 --user 1000:1000 \
  -v "$DATA_DIR:/data:rw" "$TWITCH_MINER_IMAGE" \
  --data-dir /data --check-config
docker compose -f deploy/docker-compose.rpi.yml config
docker compose -f deploy/docker-compose.rpi.yml pull twitch-miner
docker compose -f deploy/docker-compose.rpi.yml up -d --force-recreate twitch-miner
docker exec twitch-miner /twitch-miner --version
docker exec twitch-miner /twitch-miner --health
docker inspect -f 'status={{.State.Status}} health={{.State.Health.Status}} restarts={{.RestartCount}} image={{.Image}}' twitch-miner
```

After the healthy window, exercise recovery and verify the same health checks:

```sh
docker kill --signal=SIGTERM twitch-miner
docker compose -f deploy/docker-compose.rpi.yml up -d twitch-miner
docker exec twitch-miner /twitch-miner --health
docker inspect -f 'status={{.State.Status}} health={{.State.Health.Status}} restarts={{.RestartCount}}' twitch-miner
```

Record only the source revision, manifest digest, timestamps, health result,
restart count, and sanitized failure class. Never record cookies, account IDs,
webhook URLs, request headers, or raw runtime data.

Rollback is a normal Compose update: set `TWITCH_MINER_IMAGE` back to the
previous recorded digest and run `docker compose up -d twitch-miner`. Do not
roll back by reusing `:latest`.

Only the current release receives operational support. A security regression
or invalid Twitch contract should result in a new digest-pinned release or a
rollback, not an in-place binary replacement. The Rust miner has no automatic
self-update feature.
