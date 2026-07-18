# Release Record Template

Copy this template into the signed GitHub release notes for a `v*` tag. Do not
place release evidence in runtime configuration or status files.

## Candidate

- tag: `vX.Y.Z`
- source revision: `<40-character Git SHA>`
- manifest digest: `sha256:<64 hexadecimal characters>`
- commit-SHA manifest and signed-tag digest equality: `<pass/fail>`
- platforms: `linux/amd64`, `linux/arm64`, `linux/arm/v7`
- CI run: `<URL and successful conclusion>`
- multiarch run: `<URL and successful conclusion>`
- SBOM/provenance: `<attestation URLs>`
- differential review: `docs/differential-review-2026-07-13.md` at the candidate revision

## Read-only acceptance

- dedicated account: `redacted`
- canary timestamp (UTC): `<RFC3339>`
- canary image digest: `sha256:<same candidate digest>`
- EventSub result: `<fixed success/failure class and redacted capability counts>`
- PubSub result: `<fixed success/failure class and redacted topic-class counts>`
- mutations invoked: `none`

## Deployment and rollback

- prior production digest: `sha256:<64 hexadecimal characters>`
- executable rollback digest: `sha256:<64 hexadecimal characters>`
- rollback source revision: `<40-character Git SHA>`
- rollback preflight: `<timestamp and pass/fail>`
- running image matched rollback reference: `<timestamp and pass/fail>`
- candidate predeployment canary: `<timestamp and pass/fail>`
- deployed candidate digest: `sha256:<same candidate digest>`
- runtime UID/GID: `<numeric UID:GID>`
- post-start revision/health/restarts: `<redacted result>`
- normal SIGTERM recovery: `<timestamp and pass/fail>`
- rollback exercise: `<timestamps and pass/fail>`

## Soak and feature acceptance

- 24-hour window: `<start/end UTC and sanitized counter summary>`
- 72-hour window: `<start/end UTC and sanitized counter summary>`
- controlled feature matrix: `<pass/fail/unsupported with fixed classes>`
- known limitations: `<PubSub compatibility and Twitch contract risks>`

Never include account IDs, channel IDs, cookies, tokens, headers, webhook URLs,
raw Twitch payloads, configuration contents, or log contents.
