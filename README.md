# Twitch Miner Rust

An unofficial Twitch channel points miner rebuilt in Rust for a codebase that is easier to reason about, test, ship, and operate.

This project keeps the behavior that matters in day-to-day use:

- device-code login with persisted cookies
- automatic bonus chest claims
- minute-watched farming and streak handling
- prediction betting with configurable strategies and delays
- drops, raids, chat-presence, Discord notifications, and privacy-aware logging
- Docker-friendly runtime layout and multi-arch delivery paths

It is not a toy rewrite. The workspace is split into focused crates, the Twitch parsers are fixture-backed, and the runtime is organized around a single-writer state model instead of a pile of ad-hoc side effects.

## Why this rewrite exists

The point was not to rewrite working behavior for the sake of language preference. The point was to keep the miner useful while making the internals less fragile.

- `tm-runtime` owns mutable state instead of scattering it across the process
- `tm-domain` keeps decision logic pure and testable
- `tm-twitch`, `tm-pubsub`, and `tm-irc` isolate protocol boundaries
- `tm-auth` and `tm-config` make startup, persistence, and local operation predictable
- `tm-observability` keeps logging, anonymization, and Discord plumbing out of the hot path

## What it does

```mermaid
flowchart LR
    A["Device Auth"] --> B["Persist Session Cookies"]
    B --> C["Bootstrap Streamers / Followers"]
    C --> D["Watch Live Channels"]
    D --> E["Claim Bonuses, Drops, Moments"]
    D --> F["Track Predictions + Place Bets"]
    D --> G["PubSub / IRC Events"]
    E --> H["Logs / Discord / Shutdown Summary"]
    F --> H
    G --> H
```

## Quick start

### Local

```powershell
cd C:/Users/ancha/Documents/Projects/TwitchMiner/Twitch-Miner-Rust
cargo run -p tm-app -- --config ./data/config.json --data-dir ./data
```

On first launch:

1. Set `username` in `data/config.json`.
2. Start the app.
3. Open `https://www.twitch.tv/activate`.
4. Enter the device code shown in the terminal.
5. Wait for cookies to be written to `data/cookies/<username>.json`.

### Docker

```powershell
cd C:/Users/ancha/Documents/Projects/TwitchMiner/Twitch-Miner-Rust
docker compose up --build
```

The container layout is centered on `/data`:

- `/data/config.json`
- `/data/cookies/<username>.json`
- `/data/log/*.log`

There is also a named-volume variant in [deploy/docker-compose.volume.yml](deploy/docker-compose.volume.yml).

## Configuration

The miner will create and extend its config automatically, but a minimal manual setup looks like this:

```json
{
  "username": "your-twitch-username",
  "auto_update": false,
  "streamers": ["StreamerHouse"],
  "claim_drops": true,
  "claim_drops_startup": true,
  "community_goals": false,
  "privacy": {
    "anonymize_logs": false
  }
}
```

Important paths:

- config: `data/config.json`
- cookies: `data/cookies/<username>.json`
- optional logs: `data/log/`

## Workspace map

| Crate | Responsibility |
| --- | --- |
| `tm-app` | process bootstrap, lifecycle, scheduling glue |
| `tm-auth` | device auth, session loading, cookie persistence |
| `tm-config` | config creation, resolution, normalization, write-back |
| `tm-domain` | pure logic, prediction math, shared types |
| `tm-irc` | Twitch IRC transport and chat events |
| `tm-observability` | logging, anonymization, Discord payloads |
| `tm-pubsub` | PubSub batching, parsing, connection handling |
| `tm-runtime` | single-writer runtime state |
| `tm-twitch` | Twitch HTTP, GQL, scraping, parser contracts |
| `tm-updater` | release lookup and binary replacement |

## Project status

The source-level rewrite and fixture-backed parity work are documented. Live Twitch and platform-specific smoke tests are tracked separately because they require real accounts, network access, Docker publishing, or ARM hardware.

- parity checklist: [docs/behavior-parity/parity-checklist.md](docs/behavior-parity/parity-checklist.md)
- gap list: [docs/behavior-parity/gap-list.md](docs/behavior-parity/gap-list.md)
- operator guide: [docs/behavior-parity/operator-guide.md](docs/behavior-parity/operator-guide.md)
- migration notes: [docs/behavior-parity/migration-notes.md](docs/behavior-parity/migration-notes.md)
- architecture notes: [docs/architecture/README.md](docs/architecture/README.md)

## Validation

The workspace has been exercised with:

```powershell
cargo fmt --all
cargo test --workspace --quiet
cargo clippy --workspace -- -D warnings
cargo test --manifest-path tests/contract/Cargo.toml --quiet
cargo clippy --manifest-path tests/contract/Cargo.toml -- -D warnings
cargo test --manifest-path tests/integration/Cargo.toml --quiet
cargo clippy --manifest-path tests/integration/Cargo.toml -- -D warnings
```

## Safety notes

- This project is unofficial and may carry Twitch account or campaign-rule risk.
- Use a dedicated Twitch account if that risk matters to you.
- Do not commit `data/` or cookie files.
- Cookie files contain authentication material; treat them like credentials.
- The app uses device-code login and does not need your Twitch password.
- Keep `auto_update=false` for manual source builds.
- The repo ignores runtime data and logs by default.
- This project is unofficial and not affiliated with Twitch.
- You are responsible for how and where you use it.
- See [SECURITY.md](SECURITY.md) for the credential and reporting model.

## License

Licensed under the [GNU General Public License v3.0 or later](LICENSE).
