# Changelog

## Unreleased

- Adds task-aware runtime health checks, supervised task exit/panic handling,
  bounded reconnect backoff, and a privacy-safe support bundle.
- Removes the dormant automatic updater and migrates/rejects legacy
  `auto_update` configuration safely.
- Adds atomic, backed-up config and cookie migration with a `--check-config`
  preview command.
- Adds build provenance, release-digest smoke tests, SBOM/provenance, pinned
  Actions, dependency/license/secret checks, coverage, and bounded parser
  fuzz-regression tests.
- Adds the read-only Twitch canary, protocol inventory, digest-pinned
  deployment/rollback instructions, migration guide, issue template, and
  contributor review checklist.

Release notes must identify the source revision, published image digest,
supported platforms, configuration migration impact, and rollback digest.
