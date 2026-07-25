# Architecture

The project runs as a Cargo workspace with crate boundaries split by responsibility:

- `tm-app` owns process bootstrap and task wiring.
- `tm-config` owns config/path resolution and write-back; typed streamer-setting
  composition is isolated from persistence and migration.
- `tm-auth` owns cookie persistence and device auth helpers.
- `tm-domain` owns pure logic and shared types.
- `tm-events` owns transport-neutral runtime events shared by EventSub and PubSub compatibility.
- `tm-twitch` owns Twitch HTTP, GQL, and scraping contracts. HTTP execution and
  typed response normalization remain separate modules.
- `tm-pubsub` owns the isolated PubSub `/v1` compatibility client, EventSub
  WebSocket transport, and the explicit source-selection policy. EventSub
  connection lifecycle, capacity planning, and protocol normalization are
  separate modules behind one facade.
- `tm-irc` owns IRC protocol handling.
- `tm-runtime` owns the single-writer runtime state model.
- `tm-observability` owns logging, privacy helpers, and Discord webhook payloads.

`tm-runtime` owns the single-writer runtime state model, and `tm-app` owns bootstrap, process lifecycle, and top-level scheduling glue that drives it.

## Internal module layout

The largest crates are decomposed behind stable crate facades:

- `tm-app` keeps `main.rs` as the executable entrypoint and splits orchestration into startup, shutdown, drops, independently supervised EventSub/PubSub tasks, presence fallback polling, runtime effects, context refresh, minute watching, chat, and shared utilities.
- `tm-config` keeps the stable flat schema and atomic persistence in `lib.rs`;
  `settings.rs` owns typed base/override composition.
- `tm-twitch` exposes the same public API from `lib.rs`; `client.rs` owns HTTP
  execution and bounded retry policy, while `responses.rs` owns strict typed
  response validation and normalization.
- `tm-pubsub` exposes the same public API from `lib.rs`; `eventsub.rs` owns the
  connection/subscription lifecycle, `eventsub/planning.rs` owns deterministic
  cost planning, and `eventsub/protocol.rs` owns wire validation, normalization,
  and bounded message deduplication.
- `tm-runtime` exposes the same public API from `lib.rs` while separating the actor handle, runtime state/types, effects, prediction settlement helpers, and summary/history formatting.

Large private unit-test suites live under each crate's `tests/unit/` directory
and are included with `cfg(test)` path modules. They retain private access
without becoming integration targets or forcing test hooks into public APIs.

## Runtime ownership and effects

All mutable mining state remains owned by one `tm-runtime` actor. Transport,
polling, and watcher tasks submit typed commands and receive snapshots or
effect decisions; they never retain a second mutable copy. The reducer
deduplicates external mutation identifiers before returning effects. Network
mutations are executed once by `tm-app` after the actor decision and are never
silently replayed.

Application orchestration uses contexts only where values share a lifecycle:
EventSub, PubSub, runtime-effect execution, minute watching, and offline streak
recovery. Startup, canary, and CLI paths are split by preparation, decision,
and execution responsibility. Flat serialized config structures and exhaustive
state/watch reducers stay explicit because wrapping them solely to satisfy a
line or boolean count would obscure the contract.

EventSub and PubSub task contexts embed the same `RuntimeEffectContext` used to
execute actor-approved network effects. This removes duplicated runtime/client/
identity/observability/health wiring while leaving transport credentials and
subscription state in their narrower transport contexts.

## Enforced invariants

| Boundary | Invariant |
| --- | --- |
| Domain | `tm-domain` remains deterministic and has no async, HTTP, transport, tracing, CLI, or application dependency. |
| Events | `tm-events` contains transport-neutral observations only. |
| Runtime | One actor owns mutable state; its bounded queue applies backpressure, and reducers emit effects without executing network mutations. |
| Twitch HTTP | Only classified read operations retry; mutations are not replayed after an uncertain response. |
| Transports | EventSub, PubSub, IRC, and polling are independently supervised and cannot directly own runtime state. |
| Watching | At most two Twitch-creditable slots run; an eligible campaign can pin one while the remaining slot fair-rotates. |
| Privacy | Credentials, identifiers, balances, and raw responses do not cross privacy-safe diagnostic interfaces. |
| Lifecycle | Health belongs to the current process session, and shutdown uses normal signal-driven task termination. |

`scripts/verify-architecture.ps1` checks the allowed internal Cargo dependency
graph, rejects network/application dependencies in `tm-domain`, and prevents
substantial `*_tests.rs` files from returning to production source trees. CI
runs it without installing another analysis dependency.

Production workspace code forbids `unsafe`. CI also rejects production
`unwrap`, `expect`, panic, `todo!`, and `unimplemented!` shortcuts, except for
narrowly documented compile-time invariants such as fixed regex literals and
the built-in default-config schema. Test fixtures may still fail loudly.

Quality gates are handled by `.github/workflows/ci.yml`; Docker image validation and publishing remain in the multiarch workflow. Automatic self-update is intentionally absent: releases are source- and digest-pinned.
