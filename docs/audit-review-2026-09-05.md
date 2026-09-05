# September 2026 audit review

This review follows the supplied `AUDITS.MD`, including its overlapping P1/P2
and R01-R14 lists and eight comparative design suggestions. The starting
revision was `26888495b16027293b90c623a6fcc71a6b05a422`. Nine files were already
modified, principally EventSub capacity recovery and context balance reporting;
those changes were preserved and reviewed alongside the audit work.

## Assessment

Keep the Rust workspace, typed protocol boundary, serialized runtime state,
bounded work, and policy against blindly repeating spending mutations. The
strongest audit findings concern ordering, cancellation, fairness, and release
evidence. They call for corrections at those boundaries, not another runtime
architecture or an unsupported claim of better earning performance.

The source-only audit overstated P1-01. A freshly built release binary from the
audited revision accepts `--status --json`: with an isolated synthetic schema-5
status file, it exits 0 and prints the status. Without that file it exits 1 on
the file read, rather than exiting 2 during argument parsing. The redundant
flag can be removed from the deployment helper, but an impossible command-line
combination was not reproduced. The tracked example configuration also passes
the real binary's `--check-config --json` command.

The initial dirty EventSub changes passed all 54 transport tests. Review found
an additional reconnect-verification gap: matching subscription types alone
does not establish matching channels. Reconnect reconciliation now checks each
subscription's condition and unique identity as well. Both the valid transfer
and wrong-channel/duplicate-ID rejection cases run against a local HTTP server;
the complete transport suite then passed 55 tests.

## Evidence boundaries

The baseline was built offline in a detached worktree under `target/`, using
the existing lockfile and toolchain. Native tests and local HTTP/WebSocket
servers exercise Rust behavior; they are not translated scheduling models.
Windows startup timings measure process invocation, not credited mining yield.
No account credentials were read for this review and no live Twitch mutation
or new 72-hour soak has been claimed.

There was no existing Windows portable ZIP or MSI build pipeline at the starting
revision. The existing pipeline covered Rust QA, native AMD64/ARM64 container
builds, build repeatability, image verification, and offline source bundling.
The host has Cargo and GitHub CLI but initially had neither Docker nor WiX on
PATH. Container execution must therefore be verified by the CI runners or a
separately provisioned host, not described as locally tested.

The Hermes comparison was checked against the donor's current source revision
`218d75f7584c19d1a3e7d1570acde71a492fc139`. Its typed adapter boundary is useful,
but its private protocol is not established by public EventSub documentation.
Any experiment must remain observational, normalize into the existing event
model, keep certificate checks, and establish topic coverage before receiving
mutation authority. The donor is evidence of an implementation, not proof that
the protocol works with this account.

## Repository controls

GitHub initially reported `main` unprotected, no rulesets, and no deployment
environments. Main now requires the GitHub Actions `required-checks` aggregate
against an up-to-date branch, applies the rule to administrators, requires
resolved conversations, and prohibits force pushes and deletion. Ruleset
`22335633` prevents update or deletion of `v*` release tags. These are remote
repository settings; a checkout alone cannot enforce them.

The release branch is `release/0.2.0-audit-hardening`. The requested handoff is
an open pull request after verification, not a merge or stable release.

The `release` environment requires explicit approval by the repository owner,
does not allow an administrator bypass, and only permits `v*` tag deployments.
Self-review remains enabled because this is a single-maintainer repository.

## Measured development checkpoint

A temporary Rust executable compiled the actual baseline and modified
`watching.rs` modules with `rustc -C opt-level=3`. It alternated their order for
11 samples of 1,000 selections each, advancing simulated time by one minute per
selection with every eligible channel in a campaign. It did not copy either
algorithm into another language. Median CPU time per selection was:

| Eligible channels | Baseline | Modified scheduler |
| ---: | ---: | ---: |
| 4 | 0.289 microseconds | 0.433 microseconds |
| 17 | 1.065 microseconds | 1.585 microseconds |
| 100 | 25.129 microseconds | 37.884 microseconds |

This is a correctness fix with a small additional CPU cost, not a scheduler
speedup. Both implementations retain quadratic list membership work at large
channel counts; these measurements do not justify replacing the data structure
for the observed scale. The candidate source SHA-256 at this checkpoint was
`a13ded73fee043ad9d286e92dd779bbc61d638a805403c5cee93ab6a3177a45f`.
It was a dirty development measurement, not release acceptance evidence.

The clean baseline Windows executable was 7,008,256 bytes. Twenty-one warm
`--version` invocations had a median of 8.9055 milliseconds. Its import table
included `VCRUNTIME140.dll`, so copying that binary alone did not establish a
portable runtime contract. Windows packaging now explicitly targets the static
CRT and requires inspection of the resulting executable's imports.

The integration workspace test run passed all 548 tests after correcting the
claim and rename-recovery tests. All-target Clippy, strict production panic
checks, rustdoc with denied warnings, architecture checks, Markdown checks, and
release hygiene checks also passed.
The static-CRT Windows release build also succeeded, its import table contained
only Windows system libraries, and the resulting executable accepted the tracked
example configuration. These development checks were followed by the clean-source
package and CI checks linked from the pull request; they do not constitute live
release acceptance.

Clean Windows packages were built from
`5fce110bc6d7ba2edb4706d2dcc3664a7a3a891f`. WiX validation passed; administrative
MSI extraction and ZIP extraction produced executables identical to the release
binary by SHA-256. Each extracted executable passed version/revision and example
configuration checks. The executable was 7,411,712 bytes, compared with the
7,008,256-byte baseline. In 31 alternating warm `--version` samples, baseline
and candidate medians were 15.5127 and 15.2311 milliseconds respectively. This
desktop measurement does not demonstrate a meaningful startup improvement or
mining throughput change.

The same source bundle was extracted and built with `cargo run --release
--locked --offline`; its executable reported the recorded 0.2.0 revision.
The first CI pass also completed native AMD64/ARM64 builds, reproducibility,
Windows install/uninstall, fuzzing, and both coverage floors. It exposed a Linux
temporary-directory assumption and an exact SPDX predicate mismatch; these were
corrected before rerunning the pipelines. The actual signed SPDX 2.3 subject was
independently verified with GitHub CLI.

## Finding dispositions

These rows record the disposition of each supplied finding. Test and package
verification must be read separately from the original audit's confidence labels.

| Audit items | Review and change |
| --- | --- |
| P1-01 | The alleged argument conflict was disproved with the baseline executable. Deployment uses the simpler `--status` invocation. |
| P1-02 | Removed in-flight cache ownership from the spade URL cache. A cancelled fetch cannot strand later attempts; successful values alone are cached. |
| P1-03, R01, R02 | Reconcile the actual spare candidate pool without rebuilding its order, and keep spare rotation timing independent of campaign pin changes. |
| P2-01, R03 | Separate metadata health from watch health and retain per-channel failures. Status schema 6 reports selection reasons and distinct accepted-watch, confirmed-points, context, and drop-progress observations. |
| P2-02, R04 | Issue request generations before network work, guard balance application with a revision counter, and reject stale broadcast/game eligibility and watch completions atomically. |
| P2-03, R05 | Reserve prediction placement before network I/O and persist unresolved decisions. Preserve unknown outcomes, suppress replay after restart, and retain terminal event tombstones. This is a prediction placement journal, not a new general runtime database. |
| R06 | Separate bounded prediction scheduling from ordinary effects; measure transport queue time and effect queue delay. Bound background notifications and use a shared shutdown grace period. |
| R07 | Retain bounded nonadjacent point-event identities. A validated server timestamp is combined with the event facts; payloads without a trustworthy source identity retain an explicitly weaker balance-epoch fingerprint. |
| R08 | Run bounded metadata refresh in a background task, reuse the current watch snapshot, and cancel/reap the refresh on shutdown. |
| P1-04, P2-05, R09 | Promote an already built immutable digest only with external, exact-source CI/canary/soak/rollback evidence and protected-environment approval. No new soak has been claimed. |
| P1-05, R10 | Resolve a tag once, inspect immutable references thereafter, and verify signed provenance and SPDX subjects against the repository, workflow, source ref, and revision. Sign the runtime child manifest, not its enclosing BuildKit index. |
| R11 | Assemble and verify a candidate manifest before exposing its SHA discovery alias. Stable consumer aliases belong to protected promotion. |
| P2-04, R12 | Parameterize native platform selection, accept recovered errors only after current health recovers, and write a deployment state file with a repeatable Compose invocation. |
| P2-04, R13 | Snapshot runtime data after a clean stop, preserve candidate data on failure, and restore the snapshot and known rollback digest before health verification. An image change alone is not called a state rollback. |
| R14 | Configure actual main protection, immutable release tags, and the protected release environment, as described above. |

## Comparative suggestions

1. **Progress-driven recovery:** added a conservative points watchdog. It needs
   fresh metadata/context, ready measurement capabilities, a selected live
   channel, and prior server-confirmed progress for the current broadcast.
   It uses monotonic time and one bounded spare-slot reselection after a
   confirmed stall. Initial never-observed progress and per-reward Drop stalls
   remain unmeasurable; transport connectivity alone does not establish earning.
2. **Prompt claims:** reuse the refreshed inventory immediately rather than
   waiting for the periodic recovery loop. Share in-flight, unknown, and completed
   claim records so cancellation or stale inventory cannot create a second claim.
3. **Reward dependency planning:** not enabled. The registered Inventory contract
   and checked-in fixtures provide requirements and progress but do not establish
   reward deadlines, prerequisite edges, or channel-to-reward progress identity.
   Implementing the proposed graph against invented fields would give false
   eligibility decisions. A reachable reward must not be discarded merely because
   later rewards in its campaign are infeasible; retain that requirement when
   adding a verified richer inventory contract.
4. **Hermes:** add a bounded offline replay adapter and runnable observer example.
   Map subscription IDs to existing topics and wrap the donor's event body for
   the existing typed parser. It has no socket, authentication, reconnect, or
   mutation authority. Live coverage and latency comparison remain prerequisites
   for transport adoption.
5. **Scenario tests:** extend native Rust tests and local HTTP/WebSocket servers
   for cancellation, stale responses, replay, slot fairness, and ambiguous claims.
   Keep real executable checks in packaging; a translated scheduler is not used
   as application evidence.
6. **Read batching:** retain bounded per-channel reads. Decoupling their completion
   from watch scheduling addresses the demonstrated delay without inventing a
   bulk retry contract. This review does not establish a measured throughput
   benefit for a new Twitch batch protocol, so no mutation or background request
   batching is introduced.
7. **Persistent identity:** add a bounded startup cache. Preserve concurrent fresh
   configured-login reads first; after failure, a cached numeric ID must resolve
   to a freshly verified login and fresh context. No cached liveness or Drop
   eligibility is accepted.
8. **Explanations:** expose slot selection and recovery reasons with measurement
   ages before adding a dashboard. Reward urgency explanations remain unavailable
   until the richer reward contract is verified.

Keep the existing nine crates, reducer/side-effect boundary, and short state
locks. Do not restore the removed actor layer, add a general database, or infer
prediction profitability from an unexercised prediction capability. No account
linking requirement, simultaneous-campaign multiplier, blanket raw-GraphQL retry,
playback cache, self-update service, fleet controller, or remote API was added.

## Additional review corrections

Independent review found ordering cases beyond the original lists: a late HTTP
rejection after a personal prediction confirmation, terminal prediction events
arriving before personal confirmation, journal restoration after balance or status
changes, stale metadata resurrecting offline presence, and stale measurements
following a channel into a different watch slot during a new broadcast. These
cases need atomic state transitions and regression tests, rather than retries.

Transport supervisors now cancel their owned child tasks if the supervisor is
aborted, and stopped effect queues discard buffered mutations. Prediction delay
does not hold the channel lane while a newer event waits for evaluation.

Point messages without a prediction identity cannot establish which prediction
was accepted from their channel and amount alone. Only the mutation response or
personal prediction notification confirms placement. Point-accounting correlation
remains weaker than the event-specific placement journal; fresh context remains
the balance reconciliation source. No stronger private wire contract is assumed.

The new Windows pipeline packages an explicitly allowlisted executable and
documentation, validates the MSI database, and compares extracted executable
hashes. WiX 4.0.6 is pinned locally and in CI. ZIP input timestamps are normalized
to the source commit time. MSI bytes are not claimed reproducible: WiX generates
a ProductCode, which changes even for repeated builds of identical inputs.

The offline source bundle vendors both the main and fuzz workspaces and validates
each with locked offline Cargo metadata. Native AMD64/ARM64 image checks, signed
attestation verification, and the disposable Windows installer lifecycle run in
GitHub Actions because Docker is not installed on the review host.

## Sources

- [Audited source revision](https://github.com/FueledByRedBull/Twitch-Miner-Rust/tree/26888495b16027293b90c623a6fcc71a6b05a422)
- [Twitch EventSub connection and reconnect contract](https://dev.twitch.tv/docs/eventsub/handling-websocket-events/)
- [Hermes donor transport and tests](https://github.com/LudovicoPiero/twitch-miner/tree/218d75f7584c19d1a3e7d1570acde71a492fc139/TwitchChannelPointsMiner/classes/websocket/hermes)
- [Hermes subscription-to-message adapter](https://github.com/LudovicoPiero/twitch-miner/blob/218d75f7584c19d1a3e7d1570acde71a492fc139/TwitchChannelPointsMiner/classes/websocket/hermes/Pool.py)
