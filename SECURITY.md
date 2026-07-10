# Security Policy

## Account And Platform Risk

Twitch Miner Rust is an unofficial automation tool. It is not affiliated with Twitch, and using it may violate Twitch rules, product expectations, or campaign rules. Prefer a dedicated Twitch account and do not run it with credentials you cannot afford to lose.

## Authentication Data

The app uses Twitch device-code login and persists session data under:

- `data/cookies/<username>.json`
- `/data/cookies/<username>.json` in the default container layout

These files contain Twitch authentication material and should be treated like credentials. Do not commit, publish, paste, or share them. On Unix, newly written cookie files use `0600` permissions.

`config.json` may contain a Discord webhook URL, and logs may contain account
or channel activity. Treat both as sensitive runtime data. On Unix, newly
written or updated config and log files, including rotated log archives, use
`0600` permissions.

The app does not need your Twitch password for device-code login. If an older config still has a `password` field, remove it instead of trying to keep it in sync.

## Network Destinations

Normal operation talks to Twitch endpoints needed for auth, GQL, PubSub, IRC, drops, channel points, and watch progress. Discord webhooks are contacted only when configured.

Automatic self-update was removed. Deploy only digest-pinned images through the documented release procedure.

## Revoking Access

If a cookie file is exposed, delete the local file and revoke the Twitch session from Twitch account settings. Changing your password and signing out other sessions is also recommended if you suspect account compromise.

## Reporting Security Issues

Please report sensitive issues privately to the repository owner rather than opening a public issue with tokens, logs, or cookie contents. Include the source revision or image digest and a sanitized support bundle when possible; never attach runtime data.
