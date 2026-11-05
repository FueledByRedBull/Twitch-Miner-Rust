# Operations guide

How to run, update, and monitor the miner. First-time setup is in the
[README quick start](../README.md#quick-start-docker), and every setting is
described in the [configuration reference](configuration.md).

Run only one miner per Twitch account. Concurrent instances are not a supported
high-availability mode: they duplicate transport subscriptions, watch
heartbeats, and mutation decisions instead of sharing one runtime state.

## Running with Docker

The repository ships three Compose files:

- [`docker-compose.yml`](../docker-compose.yml) builds the image locally and
  bind-mounts `./data`.
- [`deploy/docker-compose.bind-mount.yml`](../deploy/docker-compose.bind-mount.yml)
  runs a published image with a bind-mounted data directory as the host user.
- [`deploy/docker-compose.volume.yml`](../deploy/docker-compose.volume.yml)
  runs a published image with a named volume.

`docker compose config --quiet` validates a Compose file without starting a
container or contacting Twitch. When building locally with `--build`, leave
`TWITCH_MINER_IMAGE` unset or set it to a tag; a digest cannot tag locally built
output.

### Data directory and container contract

Mount one directory at `/data` and keep `config.json` in it. The miner creates
`cookies/` and `log/` under the same root:

- `/data/config.json`
- `/data/cookies/<username>.json`
- `/data/log/*.log`
- `/data/earnings/YYYY-MM.jsonl`

The container sets `TCPM_CONFIG=/data/config.json` and `TCPM_DATA_DIR=/data`.
`TCPM_COOKIE_KEY_FILE` is optional and is never set by the image; see
[encrypting the saved session](#encrypting-the-saved-session).

Images contain a static Rust binary in a `scratch` runtime, with no shell,
package manager, or OS certificate bundle; TLS trust comes from the Rust TLS
stack. Run commands through the Compose service name, which does not depend on
the generated container name:

```sh
docker compose exec -T twitch-miner /twitch-miner --status --data-dir /data
```

A direct `docker exec` of `/twitch-miner` also works when you supply the actual
container name or ID; `twitch-miner` is not guaranteed to be that name outside
Compose. `docker exec ... sh` or `bash` cannot work, so use logs, the mounted
`/data` files, and host Docker tooling for inspection.

### Permissions on Linux

The Compose files run the container as a non-root UID/GID (`1000:1000` by
default in `docker-compose.yml`; the published bind-mount example pins the host
user). The mounted data directory and any existing cookie and log files must be
writable by that user. When moving from an older image that ran as root, a
one-time `chown` of the existing `config.json`, `cookies/`, and `log/` is
usually needed before the container can reuse saved cookies.

### Stopping and restarting

The miner shuts down cleanly on `SIGTERM` in containers and `CTRL-C` in a
terminal. Keep `init: true` and a short but non-zero stop grace period in
Compose. The image health check runs `--health`, which requires the runtime
heartbeat to stay fresh.

## Deploying a published image

On pushes to `main`, GitHub Actions builds, smoke-tests, signs, and publishes a
multi-architecture image for AMD64 and ARM64 to GHCR. A release promotes that
exact manifest only after its canary, soak, rollback, and required-check
evidence is approved; see the [release process](release-process.md).

Deploy by the recorded manifest digest, never `latest`. The published Compose
files keep the bind mount, read-only filesystem, dropped capabilities, restart
policy, stop grace, and health check:

```sh
export TWITCH_MINER_IMAGE='ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<recorded-digest>'
docker compose config --quiet
docker compose pull twitch-miner
docker compose up -d --no-build twitch-miner
docker compose exec -T twitch-miner /twitch-miner --health
```

<details>
<summary>Windows (PowerShell)</summary>

```powershell
$env:TWITCH_MINER_IMAGE = 'ghcr.io/fueledbyredbull/twitch-miner-rust@sha256:<recorded-digest>'
docker compose config --quiet
docker compose pull twitch-miner
docker compose up -d --no-build twitch-miner
docker compose exec -T twitch-miner /twitch-miner --health
```

</details>

Pass `-f deploy/docker-compose.bind-mount.yml` or
`-f deploy/docker-compose.volume.yml` to use the published-image layouts. For a
guarded update that checks the candidate and keeps a rollback ready, follow the
release process. To roll back, set `TWITCH_MINER_IMAGE` to the previous recorded
digest and run `docker compose up -d twitch-miner`.

## Running on Windows

The release workflow builds a self-contained portable ZIP and an MSI. The MSI
installs only the executable and documentation under Program Files; it does not
create configuration, cookie, or status files there. Keep runtime data in a
user-writable directory such as `%LOCALAPPDATA%\TwitchMiner`, under your
private profile, and pass it with `--data-dir` to every command, including
`--check-config`, `--status`, and `--health`. Release artifacts are unsigned
unless a separate signing record is published.

To watch logs, run the miner in the foreground. For a background process,
redirect `stdout` and `stderr` to `run.out.log` and `run.err.log`, follow them
with `Get-Content -Wait`, and stop the process with `Stop-Process`.

## Running without Docker

The `Release Binaries` workflow builds one archive per target for tags and
manual runs:

| Target | Built on | Executed in CI |
| --- | --- | --- |
| `x86_64-unknown-linux-musl` | Ubuntu 24.04 | yes |
| `aarch64-unknown-linux-musl` | Ubuntu 24.04 Arm | yes |
| `armv7-unknown-linux-musleabihf` | Ubuntu 24.04, cross-compiled | no: no hosted runner has an ARMv7 CPU, so CI only checks that the file is a static 32-bit ARM executable |
| `x86_64-apple-darwin` | macOS 15 Intel | yes |
| `aarch64-apple-darwin` | macOS 15 Arm | yes |

The Linux binaries are static and run on any distribution, including Alpine.
The ARMv7 build targets hard-float ARMv7 such as a 32-bit Raspberry Pi OS on a
Pi 2 or newer; ARMv6 (Pi 1, Pi Zero) is not supported, and the Docker images
remain AMD64 and ARM64 only. The macOS binaries are not signed or notarized; a
browser download carries a quarantine flag that
`xattr -d com.apple.quarantine twitch-miner` removes. Windows uses the ZIP and
MSI described in [Running on Windows](#running-on-windows).

Each archive is named `twitch-miner-<version>-<target>.tar.gz` and holds the
`twitch-miner` executable, `LICENSE`, `README.md`, `config.example.json` and
`BUILD-METADATA.txt`. The Linux archives also carry
[`deploy/systemd/twitch-miner.service`](../deploy/systemd/twitch-miner.service)
and [`deploy/openrc/twitch-miner`](../deploy/openrc/twitch-miner).

### Verifying a download

The archives are workflow artifacts kept for 30 days, each with a `.sha256` file
and a signed build-provenance attestation from the workflow run. Nothing
publishes them to a GitHub Release by itself; see the
[release process](release-process.md#release-binaries). Check the checksum, then
the attestation, which ties the archive to this repository's workflow and to the
tag you expect:

```sh
sha256sum --check twitch-miner-<version>-<target>.tar.gz.sha256
gh attestation verify twitch-miner-<version>-<target>.tar.gz \
  --repo FueledByRedBull/Twitch-Miner-Rust \
  --signer-workflow FueledByRedBull/Twitch-Miner-Rust/.github/workflows/release-binaries.yml \
  --source-ref refs/tags/v<version>
```

On macOS use `shasum -a 256 --check`. A checksum file next to the archive only
detects a damaged download; the attestation is what ties the bytes to the
workflow. A manual run from a branch is attested too, with that branch as its
source ref, which the `--source-ref` check rejects.

However the miner runs, keep one instance per Twitch account and the data
directory private, and pass `--data-dir` to every manual command, as in the
container.

### systemd

[`twitch-miner.service`](../deploy/systemd/twitch-miner.service) runs the binary
as a dedicated user with `/var/lib/twitch-miner` as its data directory:

```sh
sudo useradd --system --no-create-home --home-dir /var/lib/twitch-miner \
  --shell /usr/sbin/nologin twitch-miner
sudo install -m 0755 twitch-miner /usr/local/bin/twitch-miner
sudo install -m 0644 deploy/systemd/twitch-miner.service /etc/systemd/system/twitch-miner.service
sudo install -d -m 0700 -o twitch-miner -g twitch-miner /var/lib/twitch-miner
sudo install -m 0600 -o twitch-miner -g twitch-miner config.example.json /var/lib/twitch-miner/config.json
sudoedit /var/lib/twitch-miner/config.json
sudo -u twitch-miner twitch-miner --data-dir /var/lib/twitch-miner --check-config
sudo systemctl daemon-reload
sudo systemctl enable --now twitch-miner
journalctl -u twitch-miner -f
```

On first start the journal shows the device code, as in
[Sessions and login](#sessions-and-login). Later checks run as the service user:

```sh
sudo -u twitch-miner twitch-miner --health --data-dir /var/lib/twitch-miner
sudo -u twitch-miner twitch-miner --status --data-dir /var/lib/twitch-miner
```

**Restarts.** `Restart=on-failure` with a 30-second delay restarts the miner
after the exits the container's restart policy also covers: a lost session, a
failed background task, an expired device login. A normal stop is not
restarted. After five starts in ten minutes systemd gives up and the unit stays
failed until `systemctl reset-failed twitch-miner`. systemd has no equivalent of
the container health check, so a miner that hangs without exiting is not
restarted; watch `--health` if that matters. `TimeoutStopSec=30` matches the
30-second stop grace period of the Compose files.

**Sandbox.** The unit gives the service no capabilities and no new privileges,
a read-only system, no access to `/home`, a private `/tmp` and `/dev`,
read-only kernel interfaces, only IPv4, IPv6 and Unix sockets (the async
runtime's signal handling needs the Unix one), no namespaces or real-time
scheduling, no writable-and-executable memory, and a system-call filter of
`@system-service` without `@privileged` and `@resources`. The one writable path
is `/var/lib/twitch-miner`: the miner reads `config.json` there, writes its
session, logs, ledger and status there, and creates every temporary file beside
the file it replaces. Keep `config.json` in that directory. A `--config` path
elsewhere would need `ReadWritePaths=`, because the miner can rewrite the file
when it migrates it. The configuration reload watcher only reads.

The Compose files drop all capabilities, mount the root read-only and set
`no-new-privileges`; the unit covers all three. They set no memory or process
limit, so the unit sets none either. `systemctl show twitch-miner -p MemoryPeak`
shows what a run really used, and `MemoryMax=` or `TasksMax=` can then go in a
`systemctl edit` drop-in.

**Encrypting the session.** To use [`TCPM_COOKIE_KEY_FILE`](#encrypting-the-saved-session)
without making the key readable by the service user, keep it in a root-owned
file and let systemd hand it over as a credential:

```sh
sudo install -d -m 0700 /etc/twitch-miner
(umask 077; openssl rand -hex 32 | sudo tee /etc/twitch-miner/session.key >/dev/null)
sudo systemctl edit twitch-miner
```

```ini
[Service]
LoadCredential=session-key:/etc/twitch-miner/session.key
Environment=TCPM_COOKIE_KEY_FILE=%d/session-key
```

`%d` is the unit's credentials directory (systemd 250 or newer), so the key is
visible only inside the running service. `--auth-status`, `--import-token` and
`--decrypt-session` need the same key, which the service user cannot read from
`/etc`; run them through the same credential:

```sh
sudo systemd-run --pipe --wait --uid=twitch-miner \
  -p LoadCredential=session-key:/etc/twitch-miner/session.key \
  -p Environment=TCPM_COOKIE_KEY_FILE=%d/session-key \
  /usr/local/bin/twitch-miner --auth-status --data-dir /var/lib/twitch-miner
```

Keep the key out of backups of the data directory.

### OpenRC

[`deploy/openrc/twitch-miner`](../deploy/openrc/twitch-miner) runs the binary
under `supervise-daemon` as the same user and data directory. The commands are
for Alpine; use your distribution's user tools elsewhere:

```sh
addgroup -S twitch-miner
adduser -S -D -H -h /var/lib/twitch-miner -s /sbin/nologin -G twitch-miner twitch-miner
install -m 0755 twitch-miner /usr/local/bin/twitch-miner
install -m 0755 deploy/openrc/twitch-miner /etc/init.d/twitch-miner
install -d -m 0700 -o twitch-miner -g twitch-miner /var/lib/twitch-miner
install -m 0600 -o twitch-miner -g twitch-miner config.example.json /var/lib/twitch-miner/config.json
vi /var/lib/twitch-miner/config.json
rc-update add twitch-miner default
rc-service twitch-miner start
```

Output goes to syslog under the tag `twitch-miner` (`logread -f` on Alpine).
`supervise-daemon` restarts the miner after the same exits as the container's
restart policy, waiting 30 seconds each time, and leaves it stopped after six
exits within ten minutes. Unlike systemd, it also restarts a miner that exited
cleanly without being stopped, and the script applies no sandbox beyond the
dedicated user and a `0077` umask. For the session key, add
`export TCPM_COOKIE_KEY_FILE=/etc/twitch-miner/session.key` to
`/etc/conf.d/twitch-miner`, with a key file owned by `twitch-miner` and mode
`0600`.

### Windows scheduled task

[`deploy/windows/install-task.ps1`](../deploy/windows/install-task.ps1)
registers a scheduled task named `TwitchMiner` that starts the executable at
system startup under an account you choose, whether or not anyone is logged on,
and restarts it after a failure. It is deliberately a scheduled task and not a
Windows service. A service has to speak the service-control protocol, which
needs a Windows-service dependency and control code in the binary; the miner is
a plain console program that handles only `CTRL-C`.

The consequence is that nothing can ask it to stop. Ending the task, shutting
the machine down, or `Stop-Process` terminates the process on the spot, without
the shutdown that `CTRL-C` or `SIGTERM` performs. State files are published
through a temporary file, a flush and a rename, so a killed miner leaves
`config.json`, the session, the prediction journal, `streak-cache.json` and
`runtime-status.json` either old or new and never half written, and a
prediction is reserved in the journal before it is sent, so a kill leaves it
unresolved rather than forgotten. What a kill does lose is the work a graceful
stop finishes:

- streak-cache changes from the last 30 seconds;
- rewards still queued for the earnings ledger, a few at most;
- notifications still being delivered, including the `Ending session` message
  and the session summary;
- any request in flight, which is not replayed (see
  [mutation safety](protocol-inventory.md#mutation-safety)).

A kill can also leave `.*.tmp` files in the data directory, and on Windows a
status-file replace cut between its two renames can leave `runtime-status.json`
absent until the next heartbeat. Delete stray `.tmp` files once the miner is
stopped. When a clean stop matters, stop the miner with `CTRL-C` in a console
instead of ending the task.

From an elevated PowerShell, with the executable and a data directory (the
script creates a missing directory and restricts it to the account, SYSTEM and
Administrators; an existing directory keeps its permissions, which must keep
other users out because it holds the saved session):

```powershell
.\deploy\windows\install-task.ps1 `
  -ExecutablePath 'C:\Program Files\TwitchMiner\twitch-miner.exe' `
  -DataDirectory 'C:\ProgramData\TwitchMiner'
Start-ScheduledTask -TaskName TwitchMiner
```

The script prompts for the account's credential, fails without elevation, and
refuses to overwrite an existing task. The account needs the "Log on as a batch
job" right and access to the data directory; a dedicated local account is best.
The restart-on-failure rule belongs to Task Scheduler, not the miner: it retries
a failed run every minute, up to 999 times, and a clean exit is not restarted,
so a persistent failure repeats until the count is spent. A task has no
console, so set `"save_logs": true` for log files, and complete the first device
login in a foreground run as the same account and data directory before
registering the task. The script is not part of the ZIP or the MSI; take it from
the repository at the tag you installed. `.\deploy\windows\install-task.ps1
-Uninstall` removes the task and leaves the data directory.

## Sessions and login

On first start the miner prints a device code: open
<https://www.twitch.tv/activate>, enter the code, and the session is saved to
`cookies/<username>.json` in the data directory. Treat that file like a
password.

A saved session starts reauthorization only after a definitive authentication
rejection. Transient network or Twitch server failures leave the session
untouched and retry validation in-process with capped backoff; the wait can be
interrupted with `CTRL-C` or `SIGTERM`. Reauthorization tries the refresh token
saved with the session first and asks for a new device code only if that fails.

If device login is unavailable, import a token you already hold. It is read from
standard input so it stays out of the process list and shell history, must belong
to the configured login, and replaces the saved session. A token issued to a
different Twitch client logs a warning, because Twitch may reject its requests.

```sh
docker compose run --rm -T twitch-miner --import-token --data-dir /data < token.txt
```

While running, the miner validates its session every 10 minutes. If Twitch
definitively rejects it, the miner logs an error, sends a `SESSION_LOST` Discord
event, and exits with a failure status. The container's restart policy then
starts it again, and startup reauthorizes.

To see the state of the saved session without starting the miner, use
[`--auth-status`](#checking-the-saved-session).

### Device login notifications

A device login starts when there is no saved session, or Twitch rejected it and
the refresh token did not work, for instance after the restart described above.
The code is always printed to the log. Someone who is away from the machine cannot see the
log, so with `"notify_device_login": true` the miner also sends one `DEVICE_LOGIN`
message to Discord and the generic webhook:

```text
Twitch device login needed: open https://www.twitch.tv/activate and enter code ABCD-EFGH. The code expires in 15 minutes.
```

- The message holds the URL, the code, and how long the code lasts, and nothing
  else about the account. It is not anonymized under `privacy.anonymize_logs`,
  because the login cannot be completed without the URL and code.
- The event filters still apply: `discord.events` and `webhook.events` list
  which events each target receives, and an empty list receives all of them. If
  the key is on but no configured target would deliver `DEVICE_LOGIN`, the miner
  logs one warning at startup saying so.
- Sending never blocks or fails the login. The prompt is logged first, the
  message is sent in the background, and a failed send logs a warning while the
  login carries on.
- Each device code is announced once. If nobody enters it before it expires, the
  miner exits with a failure status, as before, and the restart policy starts it
  again with a new code and a new message. An early failure after the code was
  issued, such as an error response while polling, also exits, and the next start
  announces a new code. Without a restart delay in your restart policy, a
  persistent failure of that kind sends one message per restart.
- It is sent only when a device login really starts. A successful refresh, an
  imported token, and a valid saved session send nothing.

**What the code allows.** The code in the message cannot be exchanged for a
token. Collecting the token needs the separate device code, which the miner holds
in memory, sends only to Twitch while polling, and never logs or sends to a
notifier. Someone who sees the message could at most open the activation page,
sign in to their own Twitch account, and enter the code, which authorizes the
miner as that account. The miner then compares the login of the token it
receives with `username`. If they differ, it fails with an error naming both
logins and leaves the saved session unchanged, so the other account's token is
discarded and never replaces yours. The worst outcome is a failed login and a new
code after the restart. The webhook URL is a secret for the same reason as
before: anyone who can read the channel it posts to can race you to the code.

### Checking the saved session

`--auth-status` loads the saved session, asks Twitch once whether the token is
still valid, and prints the result. It reads `config.json` for `username` without
writing it, never starts a login, never writes the session file, and never prints
a token. Add `--json` for scripts.

```text
session file: present, plaintext
login: alice
token: valid
scopes: channel_read, chat:read
expires in: 3h 36m 40s
session file age: 26h 3m 1s
refresh token: present
```

```json
{"session_file":true,"encrypted":false,"login":"alice","validation":"valid","detail":null,"scopes":["channel_read","chat:read"],"expires_in_seconds":13000,"file_age_seconds":93781,"refresh_token":true}
```

| Field | Meaning |
| --- | --- |
| `session_file`, `encrypted` | Whether `cookies/<username>.json` exists and whether it is [encrypted](#encrypting-the-saved-session). |
| `login` | The configured login the token is checked against. |
| `validation` | `valid`; `rejected` (Twitch refused the token, or it belongs to another login); `unreachable` (timeout, rate limit, or a Twitch error, with the class in `detail`); or `not_checked` when there was nothing to check: the file is missing, damaged, encrypted without the right key, or holds no token. |
| `scopes`, `expires_in_seconds` | From Twitch's answer. The remaining validity is present only when Twitch reports one. |
| `file_age_seconds` | Time since the session file was last written. |
| `refresh_token` | Whether a refresh token is saved, not its value. |

The exit status is 0 for `valid` and `unreachable`, and non-zero for `rejected`
and for a missing or unusable session.

### Encrypting the saved session

By default `cookies/<username>.json` and its backup `<username>.json.bak` are
plaintext JSON protected by file permissions. Setting `TCPM_COOKIE_KEY_FILE` to
the path of a key file encrypts both with AES-256-GCM. The file must hold exactly
64 hexadecimal characters, a 256-bit key, optionally surrounded by whitespace,
saved as plain ASCII or UTF-8; anything else is an error that names the variable
and never shows the content. An unset or blank variable means no encryption, so
make sure a templated value is really set. Generate a key with
`(umask 077; openssl rand -hex 32 > session.key)` and keep it private: on Unix
the miner warns once if the file is readable by group or others.

```yaml
services:
  twitch-miner:
    environment:
      TCPM_COOKIE_KEY_FILE: /run/secrets/session_key
    secrets:
      - session_key
secrets:
  session_key:
    file: ./session.key
```

| Key file | Session file | Result |
| --- | --- | --- |
| not set | plaintext | As before. |
| set | plaintext | Read as is. Once the session is validated at that start, the file and its backup are rewritten encrypted, so the miner leaves no plaintext copy behind. |
| set | encrypted, right key | Works. |
| set | encrypted, wrong key or modified | The miner exits with an error that says the file could not be decrypted. It does not fall back to the backup, does not start a device login, and does not change either file. |
| not set | encrypted | The miner exits with an error that says the file is encrypted and names `TCPM_COOKIE_KEY_FILE`. Nothing is changed. |

The same rules apply to `--import-token`, `--canary`, and `--auth-status`, and
`--support-bundle` only counts the files. Each write uses a new random nonce and
is atomic, and the files keep `0600` permissions on Unix.

**File format.** `TMR-SESS`, a version byte (`1`), a random 96-bit nonce, then
the ciphertext and its 16-byte tag. The version byte and the login are
authenticated, so a file copied to another account's path no longer decrypts.

**What this protects.** Someone who gets a copy of the data disk, volume, or
backup but not the key file gets only ciphertext: they cannot read the tokens
or forge an encrypted file, though they can delete one. The miner still accepts
a plaintext session file when a key is set, so that an existing session can be
migrated, which means an attacker who can write to the data directory could
substitute one; its token would still have to belong to your login. The
encryption does not protect against anyone who can read the key file, the miner's memory, or its
environment, such as another process of the same user or `root`, and the miner
cannot erase plaintext that earlier writes left in disk blocks, snapshots, or
backups made before you turned encryption on. Keep the key file somewhere the
data does not go: a Docker secret, or a separate host directory that your data
backups skip. If the key file sits on the same disk or in the same backup as
`cookies/`, one copy yields both and the encryption means nothing.

**Turning it off or rolling back.** Releases before 0.3.8 cannot read an encrypted
file. They treat it as damaged, start a device login, and replace it. Before you
roll back, stop the miner and run the following with the key file still
configured, then roll back as described in the
[release process](release-process.md#rollback); or skip this and log in again
after the rollback.

```sh
docker compose run --rm -T twitch-miner --decrypt-session --data-dir /data
```

`--decrypt-session` needs `TCPM_COOKIE_KEY_FILE`, writes the session file and its
backup back as plaintext, each atomically and with the same private permissions,
and refuses when the file is already plaintext. If any existing file does not
decrypt it writes nothing. If you keep running this release afterwards, unset
the variable, or the next start encrypts the files again.

## Diagnostics

| Command | Use |
| --- | --- |
| `--check-config` | Validate the configuration and preview migrations without contacting Twitch. Use it before migrating. Add `--json` for scripts. |
| `--health` | Check that the running miner's tasks are fresh. Docker uses this as its health check; run it after starting or updating. |
| `--status` | Print the sanitized runtime status described below. |
| `--earnings` | Summarize the earnings ledger without contacting Twitch; see [earnings ledger and alarms](#earnings-ledger-and-alarms). Takes `--days N`, `--by day\|channel\|reason`, and `--json`. |
| `--import-token` | Read a Twitch OAuth token from standard input, validate it, and save it as the session. |
| `--auth-status` | Check the saved session once with Twitch and print its state without changing anything; see [checking the saved session](#checking-the-saved-session). Add `--json` for scripts. |
| `--decrypt-session` | Rewrite the [encrypted session](#encrypting-the-saved-session) as plaintext before a rollback. Needs `TCPM_COOKIE_KEY_FILE`. |
| `--support-bundle <path>` | Write version, status, and file-count metadata without cookies, configuration values, or log contents. |

```sh
docker compose exec -T twitch-miner /twitch-miner --check-config --json --data-dir /data
docker compose exec -T twitch-miner /twitch-miner --status --data-dir /data
docker compose exec -T twitch-miner /twitch-miner --health --data-dir /data
```

The running process rewrites `runtime-status.json` in the data directory;
`--status` prints it. Never include cookies, request headers, endpoint query
strings, or raw responses in a support report.

### Reading `--status`

The status document never contains topic suffixes, channel or user IDs,
cookies, tokens, request headers, or raw account payloads.

**Tasks and counters.** Each task reports its last successful work, its last
activity, consecutive failures, and the last redacted error class. Counters
cover claims, bets, reconnects, and refreshes; `successful_refreshes` counts
complete channel-points context refresh cycles, not OAuth token refreshes.
`playback_full_preflights` and `playback_cached_ticks` count this session's
ticks whose playback requests succeeded, by whether they ran the full preflight
or reused a cached media playlist. They say nothing about the watch event that
follows, and restart at zero with the process.
Runtime measurements cover queue depth, processed events, and local
transport-to-state latency.

**Watch slots.** Each watched channel is `measurement_unavailable`,
`awaiting_first_credit`, `first_credit_overdue`, `earning`, or `stalled`.
`progress_age_seconds` is monotonic time, observed while measurement is valid,
since the first-credit wait began or the last confirmed point changed. Earning
requires a confirmed credit during the current selection visit and broadcast;
a credit from an earlier visit does not count after reselection, and lost
measurement, deselection, or a changed broadcast resets the wait. After thirty
minutes without a first credit, a slot is marked overdue and a warning is
logged at most once per 30 minutes across both slots, listing every overdue
slot and its wait. Add `WATCH_STATUS` to `discord.events` to receive these
warnings (an empty list sends all events). Overdue slots do not rotate,
increment failure counters, or fail health.

**Transports.** EventSub, PubSub, and presence polling report separately.
EventSub shows planned, active, and failed subscriptions, `total_cost`,
`max_total_cost`, and `overflow_streamers`, and each channel records its
`presence_source`, `prediction_source`, `raid_source`, and `failure_class`.
`capacity-overflow` is the designed outcome when Twitch's subscription budget
is full: the channel uses GQL polling or the PubSub compatibility path instead,
and it can also mean that only an optional raid or prediction subscription was
skipped. `raid_source` names EventSub only when a `channel.raid` subscription was
actually allocated. `verified=false` means the current set has not been
verified, not that setup is unhealthy; judge health from active and failed
counts, recheck warnings, and task state. PubSub reports configured and
acknowledged topics, message times, and reconnects. The exact transport rules
are in the [protocol inventory](protocol-inventory.md#transports).

**Prediction journal.** `prediction_journal` reports `bytes`, `byte_limit`,
`unresolved_count`, `unresolved_limit`, `retained_count`, and
`capacity_blocked`. At capacity, new predictions are refused safely. Expired
resolved records are pruned during normal use while unknown outcomes stay
protected; never delete recent records or restore an older journal to make
room. The limits are described under
[mutation safety](protocol-inventory.md#mutation-safety).

**Earnings.** The `earnings` block reports `session_points` (rewards accepted
since the process started), `today_points` (rewards accepted today, in the
configured timezone, including earlier runs the same day), the ledger health
counters `ledger_dropped_writes` and `ledger_write_failures`, and the current
state of the two alarms, `earnings_stall` and `drops_stall`. A non-zero
`ledger_dropped_writes` means rewards happened that the ledger could not record;
the miner itself is unaffected. `--support-bundle` keeps the health counters and
alarm state but leaves out the point totals. Status files from earlier releases
have no `earnings` block, and earlier releases ignore it.

**Config reload.** The `config_reload` block is described under
[config reload](#config-reload).

**Network.** The `network` block reports `proxy_in_use` and, when a proxy
carries the connections, its `proxy_scheme` (`socks5h` or `http`). It never holds
the address, port, or credentials, and `--support-bundle` keeps it as it is.
Status files from earlier releases have no `network` block, and earlier releases
ignore it. See [using a proxy](#using-a-proxy).

## Earnings ledger and alarms

Every reward the runtime accepts is appended to `earnings/YYYY-MM.jsonl` in the
data directory, one file per UTC month, one compact JSON object per line:

```json
{"v":1,"ts":1793620800,"channel_id":"100","login":"alice","reason":"WATCH","points":10,"balance":12345,"game":"Chess"}
```

`ts` is Unix seconds, `reason` is Twitch's reason (`WATCH`, `CLAIM`,
`WATCH_STREAK`, `RAID`, and so on), and `balance` and `game` are left out when
unknown. Only gains are recorded: stakes and other spends are not, so
`PREDICTION` shows payouts only. The ledger holds channel names and balances
even when `privacy.anonymize_logs` is on, so treat it like the cookie
directory: it is created readable only by the miner's user, and the support
bundle never includes it. It grows by a few megabytes a month and is never
pruned; delete old months yourself if you do not want them. A line torn by a
crash is skipped by readers and the next record starts on a fresh line.

Writes go through a bounded queue to a separate task, so a slow or failing disk
never delays mining. When the queue is full or a write fails, the reward is
counted in `ledger_dropped_writes` and the miner carries on.

```sh
docker compose exec -T twitch-miner /twitch-miner --earnings --data-dir /data
docker compose exec -T twitch-miner /twitch-miner --earnings --days 30 --by channel --data-dir /data
docker compose exec -T twitch-miner /twitch-miner --earnings --by reason --json --data-dir /data
```

`--earnings` works while the miner runs or not and needs no network. It prints
the total, the number of rewards, the average points per hour since the first
reward in the period, and a breakdown `--by day` (the default, UTC days), `channel`,
or `reason`. `--days` defaults to 7 and accepts 1 to 3650. Only the monthly
files that overlap the period are read, line by line. Channel names are printed
as they are in the ledger, whatever `privacy.anonymize_logs` says.

**Measuring channel ranking.** Two watched slots earn `WATCH` and `CLAIM` points
at a fixed rate, about the same whichever channels are watched. Channel scoring,
the ledger yield tier, and streak discovery can only change the rewards that
vary: `WATCH_STREAK`, `RAID`, and the effect of multipliers. Expect a gain of a
few percent, not more. To see whether it shows up, compare the same length of
time before and after enabling it, with enough days to average out which
channels were live:

```sh
docker compose exec -T twitch-miner /twitch-miner --earnings --days 14 --by reason --data-dir /data
```

Look at the `WATCH_STREAK` and `RAID` rows, not at the total, and compare points
per hour rather than the sum when the miner ran for different lengths of time.
A channel that took a slot because of its score shows `score-jump` as its
selection reason in `--status`. The yield tier reads the ledger in the
background at most once an hour; without a ledger every channel ties and
rotation is plain first-in, first-out.

Two alarms report silent losses, each as one notification when it starts and one
when it clears. Both run every five minutes, log a warning, and appear in
`--status`. Add the event names to `discord.events` or `webhook.events` to
filter them; an empty list sends all events.

- `EARNINGS_STALL`: `WATCH` and `CLAIM` points over the last two hours fell below
  40% of your normal and have not yet recovered above 60%. Your normal is the
  median points per hour, over the last 14 days of ledger hours that earned
  anything, leaving out the two hours being judged, and half that while only one
  slot is selected. With fewer than 72 such hours it is 315 points per hour for
  each watched slot (two slots earn about 630). The alarm only judges a window in which at least one slot was selected
  at every check, so it stays quiet while channels are offline, after a restart
  until two hours have been observed, and after the machine was suspended.
- `DROPS_STALL`: a channel pinned for a Drop campaign has been selected for
  three cumulative hours and the unclaimed Drop minutes in your inventory have
  not changed at all. Any change, including a Drop being claimed, resets the
  count and clears the alarm. The count lives in memory and restarts with the
  process.

`DAILY_REPORT` is sent once per local day, at the first check after midnight in
the configured timezone: yesterday's points by reason, the three best channels,
and the unclaimed Drop minutes in your inventory when it is known. Streamer
names follow `privacy.anonymize_logs`; totals do not. The reported day is
remembered in `earnings/last-daily-report`, so a restart does not send it again,
and a day without rewards sends nothing.

## Config reload

The miner rereads the active configuration file every 30 seconds and acts only
when its contents changed. It never writes the file. Which settings apply
without a restart is listed in the
[configuration reference](configuration.md#changing-settings-while-the-miner-runs).

A reload that changes anything logs one line with the setting names, and the old
and new values only for settings that cannot hold a secret. Webhook and Discord
URLs, your username, and channel lists are never printed. Channel names that
were added, removed, or kept follow `privacy.anonymize_logs`. The same line is
sent as the `CONFIG_RELOAD` event. Add the name to `discord.events` or
`webhook.events` to filter it; an empty list sends all events.

```text
Config reload applied: chat_presence ("ONLINE" -> "NEVER"), streamers | needs restart: save_logs (false -> true) | channels added: Streamer3
```

**Telling whether it applied.** `--status` prints a `config_reload` block, and
the log line above appears once per applied file:

| Field | Meaning |
| --- | --- |
| `last_result` | `applied` (live changes took effect), `needs_restart` (nothing in the file could be applied), `rejected` (the file or a new channel could not be used), or absent before the first reload. |
| `last_applied_unix` | When the last `applied` reload finished. |
| `last_error_class` | Why the last attempt was rejected: `unreadable`, `invalid-config`, `channel-resolution`, `followers`, `no-channels`, or `runtime`. Empty after a reload that applied or needs a restart. |
| `rejected_attempts` | Attempts since the process started that left the running configuration unchanged. A retry counts again. |
| `needs_restart` | Names of settings that differ from the file but only take effect after a restart. Cleared when the file matches the running configuration again. |
| `deferred_removals`, `deferred_removal_count` | Channels removed from the file that are still tracked because a prediction placement on them is not settled. |

`--support-bundle` keeps the setting names and the deferred count and leaves
out the deferred channel names. Status files from earlier releases have no such
block, and earlier releases ignore it.

**Failure modes.** In every case the running configuration stays in force and
nothing is partly applied.

- The file is missing or unreadable: one warning, until it reads again.
- The file is not valid JSON or fails validation: one warning per distinct
  file content, quoting the validation message and never the file. Fix the file
  and the next check picks it up.
- A new channel cannot be resolved, or the follower list cannot be loaded: the
  whole change is rejected, and the attempt is repeated every five minutes while
  the file stays the same, with a warning only for the first.
- The change would leave no channel to mine: rejected.
- A removed channel has a prediction that is not settled: the rest applies, the
  channel stays tracked, and it is removed in a later check after the
  prediction resolves. It is listed under `deferred_removals` meanwhile.

A failed reload does not fail `--health`; the `config-watch` task reports
healthy as long as it keeps checking.

**Streak discovery.** With `streak_discovery_games` set, the same task lists live
channels about every six hours and logs one line, `Streak discovery: added ... |
removed ...`, when the set changed. A changed set reconnects the transports as
described below, so expect that about once per refresh; a refresh that finds the
same set does nothing and restarts nothing. If the listing fails, nothing changes, one warning
is logged, and the next try is five minutes later. A found channel that cannot
be loaded is skipped and retried then too. Discovered channels are never
written to the config file.

**What a reconnect restarts.** When the set of tracked channels changes, or a
setting changes which PubSub topics are needed, EventSub, PubSub, and the
presence poll are stopped, waiting up to ten seconds, and started again for the
new set, so two sets of subscriptions never deliver
events at the same time. Presence is then checked once for every channel, so a
stream that started or ended meanwhile is picked up. Events delivered twice
around the gap are ignored. Chat follows the tracked channels without a
restart. Minute watching, context refresh, Drop handling, the streak cache, the
earnings ledger, and the watch progress of channels that stay are not touched,
and a removed channel simply gives up its watch slot while the other keeps its
progress. Because the old subscriptions stop first, an event that arrives in
that short gap and is not redelivered by Twitch is missed.

## Using a proxy

Set [`proxy`](configuration.md#network-and-proxy) to a `socks5h://` or `http://`
address and restart the miner. Every connection then goes through the proxy, and
none is made directly if the proxy fails. Only this setting counts: the
`HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` environment variables are not read.

**Confirming it works.**

1. `--check-config` accepts the value. A `socks5://` address is refused with a
   message to use `socks5h://`, because plain `socks5://` would look names up
   locally.
2. `--status` shows `"network": {"proxy_in_use": true, "proxy_scheme": "socks5h"}`.
3. The proxy's own log shows the Twitch hosts as destinations (`gql.twitch.tv`,
   `eventsub.wss.twitch.tv`, `pubsub-edge.twitch.tv`, `irc.chat.twitch.tv`, the
   CDN hosts playback uses), each as a name rather than an address.
4. For proof that nothing leaves directly, stop the proxy, or block this host's
   outbound traffic except to the proxy, and watch the miner fail as described
   below instead of carrying on.

**What an unavailable proxy looks like.** The transports behave as they do for
any lost connection, and none falls back to a direct one:

- EventSub, PubSub and chat log reconnect warnings and retry with their usual
  backoff. Their tasks report consecutive failures in `--status`, and
  `--health` fails once a task is stale or has failed repeatedly.
- Reads from Twitch are retried a few times and then fail; mutations are not
  replayed. No points are earned while the minute-watched requests cannot get
  through, which the earnings-stall alarm eventually reports (it is delivered
  through the same proxy, so a Discord or webhook notification may not arrive
  while the proxy is down).
- At startup, the saved-session check keeps retrying with backoff while the proxy
  is down. A new device login needs the proxy and stops with an error without
  it.

The proxy's own host name is looked up on this machine. To keep even that off
the local resolver, write the proxy's IP address in `proxy`.

## Logs and storage

Console and saved logs use the format
`HH:MM:SS DD/MM/YY - LEVEL - [operation]: message`, with seconds controlled by
`show_seconds`. With `"log_format": "json"` each record is instead one JSON
object per line, for example
`{"timestamp":"2026-11-02T12:00:00.000+02:00","level":"INFO","operation":"on_message","message":"..."}`;
extra values appear in a `fields` object of strings, and are left out under
`privacy.anonymize_logs` exactly as the text format leaves them out. Important events use stable operation names such as `run`,
`set_online`, `set_offline`, `on_message`, `claim_bonus`, `update_raid`, and
`make_predictions`, with emoji when `emojis` is enabled.

On normal shutdown the miner prints the session ID, saved log path, duration as
`HH:MM:SS.ffffff`, a bounded report of completed predictions, and a per-streamer
points and history summary. Report lines use the shape
`HH:MM:SS DD/MM/YY - emoji/content` without the level and operation. With
`privacy.anonymize_logs`, streamer names become aliases; channel, event and
outcome IDs, titles, channel balances, decisions, and results are hidden; each
reward's points and the session's total points gained are still shown; and the
log file is named
`miner.log`.

Writes are bounded. `runtime-status.json` is replaced atomically on the
30-second supervision heartbeat, `streak-cache.json` is flushed every 30
seconds only when it changed, and saved logs rotate at 10 MiB, keep at most
five archives, and prune archives older than 30 days. On flash storage such as
an SD card, keep `/data` persistent for configuration and sessions, and move it
to more durable storage if that write rate is still too high. Never put the
cookie directory on ephemeral storage.

## Building images yourself

`scripts/build-multiarch.ps1` needs Docker with buildx. Without `-Push` it
builds and loads one image for the local platform for smoke testing. With
`-Push` it builds and publishes `linux/amd64` and `linux/arm64` under a
revision-scoped `candidate-<full-sha>` tag; stable tags belong to the protected
promotion workflow. ARMv7 images are not built; the static ARMv7 binary is
described in [running without Docker](#running-without-docker).

```powershell
./scripts/build-multiarch.ps1
docker run --rm twitch-miner-rust:local --help
./scripts/build-multiarch.ps1 -Push
```
