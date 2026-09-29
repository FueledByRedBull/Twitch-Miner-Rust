# Release Record Template

Copy this template into the signed GitHub release notes for a `v*` tag. The
machine checked evidence is an external acceptance record supplied to the
manual `Promote Release` workflow; it must pass
`scripts/verify-release-evidence.ps1` before the image tag moves. Do not commit
the record after the candidate image is built, because that would change the
source revision. Do not put evidence or runtime secrets in Compose,
configuration, or status files.

## Candidate identity

- tag: `vX.Y.Z`
- source revision: `<40-character Git SHA>`
- manifest digest: `sha256:<64 hexadecimal characters>`
- platform digests: `linux/amd64=sha256:<64>`, `linux/arm64=sha256:<64>`
- immutable image reference: `ghcr.io/<owner>/<name>@sha256:<manifest digest>`
- required checks: `<CI run ID>`, `<Multiarch Build run ID>`, `<Deep Quality run ID>`, `<Windows Release run ID>`
- check source: first three from `push` or `workflow_dispatch` at `refs/heads/main`; Windows Release from the signed `refs/tags/vX.Y.Z`, all exact SHA
- evidence file SHA-256: `<64 hexadecimal characters>`
- commit-SHA manifest and release-tag digest equality: `<pass/fail>`
- signed provenance: `<verified by gh attestation verify>`
- signed SPDX SBOM: `<verified by gh attestation verify>`
- differential review: `<path or URL and candidate revision>`

The four required run IDs must be completed successful runs of the named
workflow files. Pull-request merge runs do not establish exact source identity
for promotion.

## Windows artifacts

- portable ZIP: `twitch-miner-X.Y.Z-windows-x86_64.zip`
- portable ZIP SHA-256: `<64 hexadecimal characters>`
- MSI: `twitch-miner-X.Y.Z-windows-x86_64.msi`
- MSI SHA-256: `<64 hexadecimal characters>`
- MSI container repeatability: `WiX ProductCode/package metadata is run-specific; exact accepted checksum recorded above`
- packaged executable SHA-256 equals MSI extracted executable: `<pass/fail>`
- MSI quiet install/version/runtime-file/uninstall check: `<pass/fail>`
- CRT import inspection: `<pass/fail and tool used>`
- signing status: `<signed/unsigned and external signing record>`

The MSI installs the executable and documentation below Program Files. Runtime
configuration, cookies, and status data remain in a user-writable directory,
for example `%LOCALAPPDATA%\TwitchMiner`; every command that reads or writes
runtime state must pass that path with `--data-dir`.

## Read-only acceptance

- configuration fingerprint (SHA-256, redacted): `<64 hexadecimal characters>`
- canary source revision and image digest: `<exact candidate values>`
- canary session: `<start/end UTC, monotonic seconds>`
- canary observed timestamp: `<RFC3339 UTC>`
- mutations invoked: `none`
- EventSub: `<pass and sanitized capability counts>`
- PubSub: `<pass and sanitized topic-class counts>`
- watch: `<pass and server-confirmed result counts>`
- drops: `<pass/not-exercised with fixed reason>`
- predictions: `<pass/not-exercised/unsupported with fixed reason>`

## Soak evidence

- source revision and manifest digest: `<exact candidate values>`
- configuration fingerprint: `<same redacted fingerprint as canary>`
- wall-clock session: `<start/end UTC>`
- monotonic duration: `<seconds; at least 259200 and within 300 seconds of wall time>`
- eligible opportunity seconds: `<positive measured number no greater than soak wall time + 300 seconds>`
- live opportunity seconds: `<positive measured number within eligible time>`
- server-confirmed watch rewards: `<positive integer when watch=pass>`
- server-confirmed claims: `<positive integer when watch/drops=pass>`
- server-confirmed drops: `<positive integer when drops=pass>`
- server-confirmed predictions: `<integer; zero is valid only with predictions marked not-exercised>`
- failure recovery: `<injected/recovered counts, maximum recovery seconds>`
- resource use: `<maximum RSS, CPU seconds, disk bytes>`
- evidence freshness at approval: `<pass/fail>`

Counts must come from server-confirmed outcomes and sanitized runtime telemetry.
The verifier allows a five-minute wall/monotonic clock-adjustment tolerance and
requires the measured opportunity interval to fit within the soak session.
Opportunity ratios are measurements for this session, not claims of exact
efficiency or predictive profitability.

## Deployment and rollback

- prior production digest: `sha256:<64 hexadecimal characters>`
- executable rollback digest: `sha256:<64 hexadecimal characters>`
- rollback source revision: `<40-character Git SHA>`
- rollback preflight and running-image match: `<timestamp and pass/fail>`
- candidate predeployment canary: `<timestamp and pass/fail>`
- deployed candidate digest: `sha256:<same candidate digest>`
- deployment state file: `<path and SHA-256, no contents>`
- runtime data snapshot: `<path and timestamp, no contents>`
- compatibility policy: `compatible`
- state restore: `<pass/fail>`
- candidate data preserved on failed update: `<pass/fail>`
- post-start revision/health/restarts: `<sanitized result>`
- normal SIGTERM recovery: `<timestamp and pass/fail>`
- rollback exercise: `<timestamps and pass/fail>`

After a successful guarded update, a fresh shell must load the persistent pin:

```powershell
docker compose --env-file deploy/.twitch-miner.env -f deploy/docker-compose.bind-mount.yml config
docker compose --env-file deploy/.twitch-miner.env -f deploy/docker-compose.bind-mount.yml up -d twitch-miner
```

Never remove the candidate data preservation directory or the rollback digest
until the accepted soak is complete. Remote Twitch mutations are not reversed
by local image or data restoration.

Never include account IDs, channel IDs, cookies, tokens, headers, webhook URLs,
raw Twitch payloads, configuration contents, runtime logs, or package API
responses.
