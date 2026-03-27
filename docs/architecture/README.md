# Architecture

The rewrite currently runs as a Cargo workspace with crate boundaries matching the plan:

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
