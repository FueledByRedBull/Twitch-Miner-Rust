# Security Policy

## Account And Platform Risk

Twitch Miner Rust is an unofficial automation tool. It is not affiliated with Twitch, and using it may violate Twitch rules, product expectations, or campaign rules. Prefer a dedicated Twitch account and do not run it with credentials you cannot afford to lose. You are responsible for how and where you use it.

## Authentication Data

The app uses Twitch device-code login and persists session data under:

- `data/cookies/<username>.json`
- `/data/cookies/<username>.json` in the default container layout

These files contain Twitch authentication material and should be treated like credentials. Do not commit, publish, paste, or share them. On Unix, newly written cookie files use `0600` permissions.

`config.json` may contain a Discord webhook URL, and logs may contain account
or channel activity. Treat both as sensitive runtime data. On Unix, newly
written or updated config and log files, including rotated log archives, use
`0600` permissions. On Windows, keep the data directory under a user-private
profile directory; the app relies on inherited Windows ACLs rather than changing
them.

The repository ignores `data/` and local runtime paths such as `./config.json`,
`./cookies/`, `./log/`, and `.env*` so they are not committed by accident.

The app does not need your Twitch password for device-code login. An empty legacy `password` field is removed automatically and a non-empty one is rejected; see the [migration guide](docs/migration.md).

## Network Destinations

Normal operation talks to Twitch endpoints needed for auth, GQL, EventSub, PubSub, IRC, drops, channel points, playback preflight, and watch progress. Discord webhooks are contacted only when configured.

TLS certificate verification is always enforced; there is no option to bypass it. Optional IRC uses verified TLS on port 6697 and never sends the OAuth token over plaintext IRC. Requests to playback and telemetry URLs supplied by Twitch bypass system proxies so their redirect and DNS-address validation cannot be bypassed; the full policy is in the [protocol inventory](docs/protocol-inventory.md#remote-endpoints-and-client-identity).

Automatic self-update was removed. Deploy only digest-pinned images through the documented release procedure.

## Revoking Access

If a cookie file is exposed, delete the local file and revoke the Twitch session from Twitch account settings. Changing your password and signing out other sessions is also recommended if you suspect account compromise.

## Reporting Security Issues

Please report sensitive issues privately to the repository owner rather than opening a public issue with tokens, logs, or cookie contents. Include the source revision or image digest and a sanitized `--support-bundle` result when possible; never attach runtime data.

Maintainers acknowledge a report, reproduce it with synthetic data, prepare a fix with a release and rollback plan, and publish an advisory only after affected users have a safe update path.
