# Decisions

Durable technical choices, the alternatives considered, and why. Replace an entry
when the decision changes.

## Time zones: jiff

`tm-observability` uses `jiff` for log timestamps, the configured `timezone`, and
local day boundaries. The `tzdb-bundle-always` feature bundles the IANA database
into the binary, because the runtime image is built `FROM scratch` and has no
zoneinfo.

- Replaced: `chrono` and `chrono-tz`, which RustSec lists as unmaintained
  (RUSTSEC-2026-0543 and RUSTSEC-2026-0555) with no maintained release, so
  `cargo deny` failed on every pull request.
- Runner-up: `time`, which has no IANA zone support without another crate.
- Behavior differences: zone names are matched case-insensitively, and a time
  outside years -9999 to 9999 prints as `invalid-time` instead of panicking.
  An unknown system zone still falls back to UTC. A `TZ` variable set on the
  container now takes effect, because its name is looked up in the bundled
  database; before, the scratch image had no zoneinfo and used UTC.
