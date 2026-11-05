# Security Policy

## Account And Platform Risk

Twitch Miner Rust is an unofficial automation tool. It is not affiliated with Twitch, and using it may violate Twitch rules, product expectations, or campaign rules. Prefer a dedicated Twitch account and do not run it with credentials you cannot afford to lose. You are responsible for how and where you use it.

## Authentication Data

The app uses Twitch device-code login and persists session data under:

- `data/cookies/<username>.json`
- `/data/cookies/<username>.json` in the default container layout

These files contain Twitch authentication material and should be treated like credentials. Do not commit, publish, paste, or share them. On Unix, newly written cookie files use `0600` permissions.

By default the session file is plaintext JSON protected only by file
permissions. Setting `TCPM_COOKIE_KEY_FILE` to a file that holds a 256-bit key
(64 hexadecimal characters) makes the miner write the session file and its
backup encrypted with AES-256-GCM, and read either form. That protects a copy of
the data disk or of a backup made without the key. It does not protect against
anyone who can read the key file or the running process, and it achieves nothing
if the key file is stored next to `cookies/` on the same disk or in the same
backup: keep the key in a Docker secret or on separate storage. An encrypted
file that cannot be decrypted stops the miner rather than starting a new login
or replacing it. Releases before 0.3.8 cannot read an encrypted file; the
[operations guide](docs/operations.md#encrypting-the-saved-session) describes the
format, the key file rules, and how to decrypt before a rollback.

`--auth-status` reports the state of the saved session and never prints a token.
With `notify_device_login` enabled, the device-login URL and user code, but
nothing else about the account, are also sent to your configured Discord or
generic webhook target. The user code cannot be exchanged for a token without the
device code, which the miner never logs or sends, and a token for a different
Twitch login than `username` is rejected without touching the saved session; see
[device login notifications](docs/operations.md#device-login-notifications).

`config.json` may contain a Discord webhook URL, a generic webhook URL, or proxy
credentials, and logs may contain account or channel activity. Treat both as
sensitive runtime data. On Unix, newly written or updated config and log files,
including rotated log archives, use `0600` permissions. On Windows, keep the
data directory under a user-private profile directory; the app relies on
inherited Windows ACLs rather than changing them.

The repository ignores `data/` and local runtime paths such as `./config.json`,
`./cookies/`, `./log/`, and `.env*` so they are not committed by accident.

The app does not need your Twitch password for device-code login. An empty legacy `password` field is removed automatically and a non-empty one is rejected; see the [migration guide](docs/migration.md).

## Network Destinations

Normal operation talks to Twitch endpoints needed for auth, GQL, EventSub, PubSub, IRC, drops, channel points, playback preflight, and watch progress. Discord webhooks are contacted only when configured.

TLS certificate verification is always enforced; there is no option to bypass it. Optional IRC uses verified TLS on port 6697 and never sends the OAuth token over plaintext IRC. An optional `proxy` setting (`socks5h://` or `http://`) carries every connection the miner makes, including WebSockets, chat and notifications, and there is no direct fallback: if the proxy is unavailable the connection fails rather than exposing your address. Destination names are resolved by the proxy; only the proxy's own host name is resolved locally, which an IP address avoids. `socks5://` is refused because it would resolve names locally. The `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` environment variables are never read, so only `proxy` can route traffic. The value is never logged, shown in `--status`, or included in the support bundle, which report only whether a proxy is in use and its protocol. Requests to playback and telemetry URLs supplied by Twitch normally validate every address their host name resolves to, to refuse private destinations. Through a proxy the proxy resolves the name, so that address validation cannot apply; the checks on the URL itself and the ban on redirects still do. The full policy is in the [protocol inventory](docs/protocol-inventory.md#remote-endpoints-and-client-identity) and the [configuration reference](docs/configuration.md#network-and-proxy).

Automatic self-update was removed. Deploy only digest-pinned images through the documented release procedure.

## Revoking Access

If a cookie file is exposed, delete the local file and revoke the Twitch session from Twitch account settings. Changing your password and signing out other sessions is also recommended if you suspect account compromise.

## Reporting Security Issues

Please report sensitive issues privately to the repository owner rather than opening a public issue with tokens, logs, or cookie contents. Include the source revision or image digest and a sanitized `--support-bundle` result when possible; never attach runtime data.

Maintainers acknowledge a report, reproduce it with synthetic data, prepare a fix with a release and rollback plan, and publish an advisory only after affected users have a safe update path.
