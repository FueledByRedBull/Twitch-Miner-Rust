# Architecture

The project runs as a Cargo workspace with crate boundaries split by responsibility:

- `tm-app` owns process bootstrap and task wiring.
- `tm-config` owns config/path resolution and write-back.
- `tm-auth` owns cookie persistence and device auth helpers.
- `tm-domain` owns pure logic and shared types.
- `tm-twitch` owns Twitch HTTP, GQL, and scraping contracts.
- `tm-pubsub` owns PubSub transport parsing and topic batching.
- `tm-irc` owns IRC protocol handling.
- `tm-runtime` owns the single-writer runtime state model.
- `tm-observability` owns logging, privacy helpers, and Discord webhook payloads.
- `tm-updater` owns release lookup and binary replacement logic.

`tm-runtime` owns the single-writer runtime state model, and `tm-app` owns bootstrap, process lifecycle, and top-level scheduling glue that drives it.

## Internal module layout

The largest crates are decomposed behind stable crate facades:

- `tm-app` keeps `main.rs` as the executable entrypoint and splits orchestration into startup, shutdown, drops, PubSub, runtime effects, context refresh, minute watching, chat, and shared utilities.
- `tm-twitch` exposes the same public API from `lib.rs` while separating HTTP client code, GQL operation construction, Twitch contract extraction, parsers, cookie helpers, ID generation, and public types.
- `tm-pubsub` exposes the same public API from `lib.rs` while separating the WebSocket client, topic/payload construction, parser code, prediction parsing, error types, and event types.
- `tm-runtime` exposes the same public API from `lib.rs` while separating the actor handle, runtime state/types, effects, prediction settlement helpers, and summary/history formatting.

Quality gates are handled by `.github/workflows/ci.yml`; Docker image validation and publishing remain in the multiarch workflow.
