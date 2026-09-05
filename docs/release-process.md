# Release And Rollback Process

Releases are source-and-digest based. Mutable image tags are convenience
labels, never deployment input.

Use [release-record-template.md](release-record-template.md) for the signed
release record and its external machine-checked evidence. Candidate and
rollback digests belong in that record, never in runtime configuration.

The read-only canary and live transport expectations are defined in the
[protocol inventory](protocol-inventory.md); this document owns only release,
deployment, rollback, and evidence procedure.

The Docker builder also pins the Rust toolchain to an immutable manifest
digest and pins `cargo-chef` to an exact locked version;
`scripts/verify-release-hygiene.ps1` rejects mutable builder inputs.

The main-image publication path runs `scripts/verify-build-integrity.ps1` once
per candidate. It performs two isolated optimized builds and requires identical
executable SHA-256 values plus embedded revision metadata. Ordinary pull-request
CI performs one build through the normal workspace checks instead of repeating
the release-only comparison.

The Windows lane uses `scripts/build-windows-release.ps1`. It builds an explicit
`x86_64-pc-windows-msvc` binary with a statically linked MSVC CRT, embeds the
full source revision and source date, and produces a portable ZIP with a SHA-256
sidecar. CI reuses the same Windows job as tag/manual releases, building an MSI from
`installer/Product.wxs` with the pinned WiX 4.0.6 tool. The MSI contains only
the executable and documentation under Program Files; it does not create a
configuration, cookie, or runtime-status file there. Use a user-writable path
such as `%LOCALAPPDATA%\TwitchMiner` and pass it explicitly with `--data-dir`
for every command, including `--status` and `--health`. The portable ZIP and
MSI are required to carry the same executable bytes, and CI inspects the PE
imports for dynamic Visual C++ runtime dependencies. Artifacts are unsigned
unless a separately recorded code-signing step is configured. The portable ZIP
normalizes its allowlisted file timestamps to `SOURCE_DATE_EPOCH`; its checksum
is therefore repeatable for the same binary and source metadata. WiX generates
MSI package metadata such as `ProductCode` during each build, so MSI container
checksums can differ even when the embedded executable and inputs are identical.
Record the exact MSI checksum produced by the accepted Windows run and use the
extracted executable comparison as the cross-package identity check. CI also
installs the MSI quietly on a disposable Windows runner, checks the full version
and revision under Program Files without active runtime files, and uninstalls it
before publishing the artifact. The build also runs WiX MSI database validation.

Stable promotion consumes an external acceptance record. It is intentionally
not committed after the candidate build: adding a record to the source tree
would change the source revision that the image represents. The manual
`Promote Release` workflow receives the existing signed tag, exact source SHA,
manifest digest, and sanitized evidence JSON through protected environment
approval. Schema version 2 binds both platform child digests, exactly one
successful run each for CI, Multiarch Build, Deep Quality, and the tag-triggered
Windows Release MSI lane, a read-only
canary, a current 72-hour soak, rollback/state evidence, and approval. Required
runs must be successful `push` or `workflow_dispatch` executions from
`refs/heads/main` (and the signed tag ref for Windows Release); pull-request
merge runs are not accepted as exact-source evidence. The workflow verifies the
signed tag points at the source SHA, then checks immutable image attestations and
promotes the accepted manifest digest without rebuilding to both the version tag
and `latest`.

For an offline recovery build, package the exact Git revision together with
every locked crates.io source:

```powershell
./scripts/create-offline-source-bundle.ps1 -Revision <40-character-source-revision>
```

The helper uses `git archive`, `cargo vendor --locked --versioned-dirs --sync
fuzz/Cargo.toml`, writes an offline Cargo source replacement, validates locked
offline metadata for both the root workspace and the isolated fuzz workspace,
and emits a `.tar.gz` plus `.sha256` under `target/` by default.
Keep that checksum in the release record and store the bundle outside the
repository. The bundle excludes working-tree edits and contains no runtime
configuration, cookies, logs, or credentials.

After verifying the checksum and extracting the bundle, build from its root with
the pinned Rust toolchain and native compiler already installed. Preserve the
recorded revision in the executable:

```powershell
$env:BUILD_REVISION = (Get-Content SOURCE_REVISION -Raw).Trim()
cargo build --release --locked --offline -p tm-app
```

The bundle supplies Cargo sources, not the Rust toolchain or operating-system
build tools. Windows portable distribution still requires the static-CRT flags
and import verification used by `build-windows-release.ps1`.

Maintainers can exercise the complete procedure without retaining the generated
archive by adding `-ValidateOnly`; that mode is restricted to output under
`target/`.

1. Update `CHANGELOG.md` with behavior, configuration, migration, and known
   compatibility changes. Keep any review artifact with the release record.
2. Run the local QA commands in `CONTRIBUTING.md`, including the architecture
   boundary check, run
   `scripts/verify-go-baseline.ps1` against the pinned Go baseline, and use the
   performance guidance in [performance.md](performance.md) when
   shared prediction/selection behavior or performance changes. Also require
   a successful Deep Quality run for the exact revision. Deep Quality must pass
   bounded parser fuzzing, the ratcheted 60% critical-core and
   46.0% application branch-coverage floors. Then push the candidate commit to
   `main`. The multiarch workflow builds the AMD64 and ARM64 images, resolves
   their runtime child digests, signs the child SBOM/provenance statements, and
   publishes a candidate manifest. After immutable-digest verification, only
   the long SHA discovery tag is published; `latest`, version tags, and branch
   aliases remain withheld until protected promotion.
3. Retrieve the `published-manifest-digest` artifact, run the read-only canary
   against that exact digest, deploy it by digest, and complete the required
   monitoring window. Record sanitized session boundaries, a configuration
   fingerprint, eligible/live opportunity windows, server-confirmed outcomes,
   failure recovery, resource use, and explicit capability statuses in the
   external evidence record. Mark predictions `not-exercised` or `unsupported`
   when they were not tested; a generic transport health result is not
   prediction proof.
4. Create and push a signed `v*` tag at the accepted commit only after the
   external evidence is complete. Dispatch `Promote Release` from that tag
   ref with the signed tag, source SHA, accepted manifest digest, and evidence
   JSON. The workflow
    verifies the tag points at that SHA, checks both platform revisions and
    signed attestations, uses the documented same-manifest-digest promotion
    behavior of
   [`docker buildx imagetools create`](https://docs.docker.com/reference/cli/docker/buildx/imagetools/create/),
    and fails unless both stable aliases retain the exact accepted digest.
   For a local sanitized record at `$evidencePath`, the dispatch shape is:

   ```powershell
   gh workflow run promote-release.yml --ref vX.Y.Z `
     -f release_tag=vX.Y.Z `
     -f source_revision=<40-character-source-revision> `
     -f manifest_digest=sha256:<64-hex-manifest-digest> `
     -f evidence_json="$(Get-Content -Raw $evidencePath)"
   ```

   The tag ref is required by the protected `release` environment; the record
   itself remains external to the source commit.
5. After `Promote Release` succeeds, resolve both stable image aliases once and
   require each to equal the canaried/soaked digest. Record that digest, source
   revision, platforms, canary and soak results, and rollback digest in the
   release record.
6. Set `TWITCH_MINER_IMAGE` to the exact `ghcr.io/...@sha256:<digest>` value
   and `TWITCH_MINER_DATA_DIR` to the existing data directory before an update.
7. After deployment, verify `--version`, `--health`, container health, and a
   normal `SIGTERM` restart. Require the status session timestamp to belong to
   the current container start and wait for all runtime tasks plus EventSub and
   PubSub capabilities to recover; Docker health by itself can precede complete
   transport setup. Keep the previous digest until the new deployment has
   remained healthy through its monitoring window.

If the previous image bytes are unavailable, rebuild the known-good source
revision into a new, explicitly named rollback image. The helper never pushes
unless `-Push` is supplied:

```powershell
./scripts/build-rollback-image.ps1 -Revision 1c10f11 -Platform linux/arm64
./scripts/build-rollback-image.ps1 -Revision 1c10f11 -Platform linux/arm64 -Push
docker buildx imagetools inspect ghcr.io/fueledbyredbull/twitch-miner-rust:rollback-<resolved-sha>
```

Record the newly produced digest; the old digest cannot be recreated from its
hex string alone. The rollback builder accepts `linux/amd64` or `linux/arm64`
and records the full source SHA in the image. Run `--check-config` against the
rollback digest on the target host before placing it in the rollback Compose
file.

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

## Linux bind-mount verification commands

Run these on the target AMD64 or ARM64 host after checking that the mounted data
directory is owned by the UID/GID used by Compose. Substitute only the recorded
manifest digest; do not use `latest` for deployment.

```sh
export TWITCH_MINER_IMAGE='ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<recorded-digest>'
export DATA_DIR="$PWD/data" # set this to the existing host data directory
export TWITCH_MINER_DATA_DIR="$DATA_DIR"
docker run --rm --user 1000:1000 \
  -v "$DATA_DIR:/data:ro" "$TWITCH_MINER_IMAGE" \
  --data-dir /data --check-config --json
docker run --rm --user 1000:1000 \
  -v "$DATA_DIR:/data:ro" "$TWITCH_MINER_IMAGE" \
  --data-dir /data --canary
docker compose -f deploy/docker-compose.bind-mount.yml config
docker compose -f deploy/docker-compose.bind-mount.yml pull twitch-miner
docker compose -f deploy/docker-compose.bind-mount.yml up -d --force-recreate twitch-miner
docker compose -f deploy/docker-compose.bind-mount.yml exec -T twitch-miner /twitch-miner --version
docker compose -f deploy/docker-compose.bind-mount.yml exec -T twitch-miner /twitch-miner --health
CONTAINER_ID="$(docker compose -f deploy/docker-compose.bind-mount.yml ps -q twitch-miner)"
test -n "$CONTAINER_ID"
docker inspect -f 'status={{.State.Status}} health={{.State.Health.Status}} restarts={{.RestartCount}} image={{.Image}}' "$CONTAINER_ID"
```

After the healthy window, exercise recovery and verify the same health checks:

```sh
CONTAINER_ID="$(docker compose -f deploy/docker-compose.bind-mount.yml ps -q twitch-miner)"
test -n "$CONTAINER_ID"
docker kill --signal=SIGTERM "$CONTAINER_ID"
docker compose -f deploy/docker-compose.bind-mount.yml up -d twitch-miner
docker compose -f deploy/docker-compose.bind-mount.yml exec -T twitch-miner /twitch-miner --health
CONTAINER_ID="$(docker compose -f deploy/docker-compose.bind-mount.yml ps -q twitch-miner)"
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
older rollback images. The deployed status probe uses plain `--status`, which
already emits JSON; it does not add the redundant `--json` flag. The helper
parameterizes the target platform and writes an atomic
`deploy/.twitch-miner.env` pin after success. It verifies that the supplied
rollback reference is the image used by the running service and backs up
Compose plus the runtime data directory. Because an active miner can consume
Twitch's complete EventSub cost budget, the helper then stops the rollback
service with normal `SIGTERM` before running the candidate read-only canary
exclusively. After replacement it waits through the bounded startup window for
the expected revision and healthy state; any failed canary or deployment gate
restores and verifies the rollback image.
This guarded path therefore includes a short, intentional mining interruption:

```powershell
./scripts/deploy-with-rollback.ps1 `
  -CandidateImage 'ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<candidate>' `
  -RollbackImage 'ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<rollback>' `
  -CandidateRevision '<40-character-source-revision>' `
  -RollbackRevision '<40-character-rollback-revision>' `
  -DataDir './data'
```

The helper leaves the previous data snapshot and any failed candidate data in
`target/deploy-backups/` (or the explicit `-DataBackupPath`) for inspection.
On Linux the data snapshot is a restrictive tar archive that records numeric
owner and mode metadata; the Windows fallback uses the host filesystem copy.
The `.twitch-miner.env` file is not Compose's automatic `.env` file, so a fresh
shell must load the persistent pin explicitly before invoking Compose:

```powershell
docker compose --env-file deploy/.twitch-miner.env -f deploy/docker-compose.bind-mount.yml config
docker compose --env-file deploy/.twitch-miner.env -f deploy/docker-compose.bind-mount.yml up -d twitch-miner
```

Rollback restores the state file with the known rollback digest (while
preserving any other state entries) and restores the metadata-preserving data
snapshot, preserving the candidate directory for diagnosis. Backup paths are
restricted to the deployment operator on the host. This is a local
persistent-state guarantee;
it cannot reverse remote Twitch mutations, and a rollback binary still needs a
compatible data-format check.

## Power-loss limits

Config, cookie, and runtime-status publication uses a temporary file, file
flush, and same-directory rename, so readers do not observe a partially written
JSON document during an ordinary process crash. That is not proof against a
host power loss: filesystem journaling, directory-entry persistence, storage
controller caches, SD-card firmware, and the timing of the final rename remain
outside the process's control. A backup may therefore be the last durable copy
even when the pre-loss write returned successfully.

Before a controlled host power-cycle test, verify the current config/cookie
backups are readable, record their metadata without contents, stop unrelated
writes, and retain the previous image digest. After power returns, run
`--check-config --json`, start only `twitch-miner`, verify `--health` and restart
count, and confirm no temporary publication files remain. Do not perform an
uncontrolled power cut until those backups and rollback bytes are available.

Only the current release receives operational support. A security regression
or invalid Twitch contract should result in a new digest-pinned release or a
rollback, not an in-place binary replacement. The Rust miner has no automatic
self-update feature.
