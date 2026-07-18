# Release And Rollback Process

Releases are source-and-digest based. Mutable image tags are convenience
labels, never deployment input.

Use [release-record-template.md](release-record-template.md) for the signed
release evidence. Candidate and rollback digests belong in that release record,
never in runtime configuration.

The Docker builder also pins the Rust toolchain to an immutable manifest
digest and pins `cargo-chef` to an exact locked version;
`scripts/verify-release-hygiene.ps1` rejects mutable builder inputs.

1. Update `CHANGELOG.md` with behavior, configuration, migration, and known
   compatibility changes. Review and attach the candidate's differential-review
   artifact to the release evidence.
2. Run the local QA commands in `CONTRIBUTING.md`, run
   `scripts/verify-go-baseline.ps1` against the pinned Go baseline, and push the
   candidate commit to `main`. The multiarch workflow builds the three platform
   images, SBOM/provenance attestations, the manifest, and the immutable
   `sha-<40-character-commit>` tag.
3. Retrieve the `published-manifest-digest` artifact, run the read-only canary
   against that exact digest, deploy it by digest, and complete the required
   monitoring window.
4. Create and push a signed `v*` tag at the accepted commit. The tag workflow
   does not rebuild. It resolves the existing commit-SHA manifest, verifies all
   three platform revisions and attestations, uses the documented single-index
   carbon-copy promotion behavior of
   [`docker buildx imagetools create`](https://docs.docker.com/reference/cli/docker/buildx/imagetools/create/),
   and fails unless the release tag retains the exact accepted digest.
5. Retrieve the tag run's `published-manifest-digest` artifact and require it
   to equal the canaried/soaked digest. Record that digest, source revision,
   platforms, canary and soak results, and rollback digest in the release
   record.
6. Set `TWITCH_MINER_IMAGE` to the exact `ghcr.io/...@sha256:<digest>` value
   and `TWITCH_MINER_DATA_DIR` to the existing data directory before an update.
7. After deployment, verify `--version`, `--health`, container health, and a
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

## GHCR retention

Package cleanup must preserve every digest referenced by the deployed Compose
file, the current signed release record, or its executable rollback record.
Before deleting any package version, resolve all mutable tags to manifests,
compare their digests with those three records, and abort cleanup if a protected
digest would lose its final package version. Keep the previous and candidate
image bytes through the complete 72-hour soak. A long-SHA tag is useful for
discovery but is not proof that cleanup retained the referenced digest; verify
the digest with `docker buildx imagetools inspect` after any retention action.

Do not use an age-only or "delete all untagged" rule without this protected
digest check. Record the cleanup timestamp, protected candidate/rollback
digests, and fixed pass/fail result in release evidence; never record registry
credentials or package API responses.

## Raspberry Pi verification commands

Run these on the Pi after checking that the mounted data directory is owned by
the UID/GID used by Compose. Substitute only the recorded manifest digest; do
not use `latest` for deployment.

```sh
export TWITCH_MINER_IMAGE='ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<recorded-digest>'
export DATA_DIR="$PWD/data" # set this to the existing Pi data directory
export TWITCH_MINER_DATA_DIR="$DATA_DIR"
docker run --rm --platform linux/arm64 --user 1000:1000 \
  -v "$DATA_DIR:/data:ro" "$TWITCH_MINER_IMAGE" \
  --data-dir /data --check-config --json
docker run --rm --platform linux/arm64 --user 1000:1000 \
  -v "$DATA_DIR:/data:ro" "$TWITCH_MINER_IMAGE" \
  --data-dir /data --canary
docker compose -f deploy/docker-compose.rpi.yml config
docker compose -f deploy/docker-compose.rpi.yml pull twitch-miner
docker compose -f deploy/docker-compose.rpi.yml up -d --force-recreate twitch-miner
docker compose -f deploy/docker-compose.rpi.yml exec -T twitch-miner /twitch-miner --version
docker compose -f deploy/docker-compose.rpi.yml exec -T twitch-miner /twitch-miner --health
CONTAINER_ID="$(docker compose -f deploy/docker-compose.rpi.yml ps -q twitch-miner)"
test -n "$CONTAINER_ID"
docker inspect -f 'status={{.State.Status}} health={{.State.Health.Status}} restarts={{.RestartCount}} image={{.Image}}' "$CONTAINER_ID"
```

After the healthy window, exercise recovery and verify the same health checks:

```sh
CONTAINER_ID="$(docker compose -f deploy/docker-compose.rpi.yml ps -q twitch-miner)"
test -n "$CONTAINER_ID"
docker kill --signal=SIGTERM "$CONTAINER_ID"
docker compose -f deploy/docker-compose.rpi.yml up -d twitch-miner
docker compose -f deploy/docker-compose.rpi.yml exec -T twitch-miner /twitch-miner --health
CONTAINER_ID="$(docker compose -f deploy/docker-compose.rpi.yml ps -q twitch-miner)"
docker inspect -f 'status={{.State.Status}} health={{.State.Health.Status}} restarts={{.RestartCount}}' "$CONTAINER_ID"
```

Record only the source revision, manifest digest, timestamps, health result,
restart count, and sanitized failure class. Never record cookies, account IDs,
webhook URLs, request headers, or raw runtime data.

Rollback is a normal Compose update: set `TWITCH_MINER_IMAGE` back to the
previous recorded digest and run `docker compose up -d twitch-miner`. Do not
roll back by reusing `:latest`.

For a guarded candidate update, use the helper below with full immutable image
references and both 40-character revisions. It preflights candidate and
rollback config compatibility and revision identity, requiring structured
`--json` validation from the candidate while using the plain check supported by
older rollback images. It verifies that the supplied
rollback reference is the image used by the running service, runs the candidate
read-only canary, and backs up Compose. After replacement it waits through the
bounded startup window for the expected revision and healthy state; any failed
gate restores and verifies the rollback image:

```powershell
./scripts/deploy-with-rollback.ps1 `
  -CandidateImage 'ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<candidate>' `
  -RollbackImage 'ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<rollback>' `
  -CandidateRevision '<40-character-source-revision>' `
  -RollbackRevision '<40-character-rollback-revision>' `
  -DataDir './data'
```

## Power-loss limits

Config, cookie, and runtime-status publication uses a temporary file, file
flush, and same-directory rename, so readers do not observe a partially written
JSON document during an ordinary process crash. That is not proof against a
host power loss: filesystem journaling, directory-entry persistence, storage
controller caches, SD-card firmware, and the timing of the final rename remain
outside the process's control. A backup may therefore be the last durable copy
even when the pre-loss write returned successfully.

Before a controlled Pi power-cycle test, verify the current config/cookie
backups are readable, record their metadata without contents, stop unrelated
writes, and retain the previous image digest. After power returns, run
`--check-config --json`, start only `twitch-miner`, verify `--health` and restart
count, and confirm no temporary publication files remain. Do not perform an
uncontrolled power cut until those backups and rollback bytes are available.

Only the current release receives operational support. A security regression
or invalid Twitch contract should result in a new digest-pinned release or a
rollback, not an in-place binary replacement. The Rust miner has no automatic
self-update feature.
