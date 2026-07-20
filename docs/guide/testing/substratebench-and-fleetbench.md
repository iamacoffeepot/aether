# SubstrateHarness and FleetBench

Aether has two integration harnesses because “use the real actor runtime” and
“use the real process boundary” answer different questions.

| Harness | Boundary crossed | Best for |
|---|---|---|
| Unit/pure test | function/module only | codecs, parsers, state machines, validation |
| `SubstrateHarness` | real substrate, scheduler, capabilities; in process | actor chains, settlement, frames, filesystem, component behavior |
| `FleetBench` | real hub RPC plus forked child process | stores/selectors, spawn/terminate, proxy routing, cross-process load/replace |

Choose the narrowest harness that can falsify the contract. Process tests are
valuable, but they are slower and produce less-local failures.

## SubstrateHarness topology

The in-process `SubstrateHarness` boots the substrate-harness chassis and owns it on the test
thread. Replies route through a recording loopback rather than a socket. API
methods pump chassis events synchronously and correlate each reply by a fresh
correlation id.

It still uses the real:

- mailbox registry, scheduler, actor runtimes, and settlement graph;
- component loader and wasm host;
- filesystem capability when the builder is given namespace roots (the default
  `SubstrateHarness` omits `aether.fs`);
- offscreen render/capture path;
- lifecycle driver and input/tick stages;
- logging/tracing rings and typed replies.

It is not a mock engine. The simplification is process/transport ownership.

## Builder and isolation

`SubstrateHarnessBuilder` configures the boundary a test needs: target size, namespace
roots, worker count, log/trace capacities, settlement cap, clipboard mode, and
game gateway configuration.

Prefer builder-scoped values to process environment. Tests that need the
filesystem must provide `NamespaceRoots` through the builder; doing so both
installs `FsCapability` and redirects `save://`, `assets://`, and `config://`.
Point those namespaces at temporary roots so parallel tests do not share host
files. Use the in-memory clipboard when testing deterministic text interaction.

Dropping the bench tears down its passives and scheduler. Do not leak it into a
global or run several tests against one mutable bench unless the shared lifetime
is itself the contract.

## Settlement-gated operations

`send_mail` and related primitives wait for the pushed causal chain to settle
before the next observation. A slow-chain heartbeat can extend patience; the
cumulative cap identifies a genuine wedge and reports pending roots/hold counts.

This gate prevents a common flaky pattern:

```text
send producer mail
capture/assert immediately
descendant work arrives after the assertion
```

Do not add arbitrary sleeps. If the chain should cause the observation, preserve
lineage and let settlement order it. If it is intentionally detached, expose a
real readiness/result signal.

## Declarative operation sequences

`SubstrateHarness::execute` runs labelled `HarnessOp`s and centralizes the settlement
discipline:

- `Advance` drives complete frames;
- `SendMail` fires typed mail and waits for settlement;
- `SendAndAwait` stores a typed reply for later decode;
- `Capture` reads the current frame;
- `CaptureWithMails` atomically applies pre-mail, captures, and performs
  after-mail cleanup.

`ExecutionResult` retrieves output by label. This is the typed-Rust successor to
the retired YAML scenario runner: the compiler checks kind construction, while
the harness owns ordering.

Use `CaptureWithMails` when geometry must land in the same frame as readback;
separate send/capture steps describe a different temporal contract.

## Visual evidence

`ArtifactGuard` preserves PNG, check results, and optional reference evidence on
panic or explicit persistence. It avoids filling successful CI runs with images
while making a failed visual assertion inspectable.

Pair it with structural checks (dimensions, non-background pixels, regions,
reference relation) rather than only golden-byte equality. GPU/render changes
can be semantically correct without byte-identical PNG compression or edge
rasterization.

## FleetBench topology

`FleetBench` is the `aether-fleet-bench` test-support crate, taken as a
dev-dependency by the fleet scenario suites. It
starts a real hub, connects over the production RPC framing, and can fork actual
child substrate binaries — the headless chassis resolves through
`dist/manifest.json` (run `cargo xtask dist` first, or set
`AETHER_FLEET_BENCH_HEADLESS_BIN`). It exercises the same boundary an MCP
coordinator uses without requiring an interactive MCP session.

Use it for:

- binary/component artifact store and selector behavior;
- spawn failure, heartbeat, recently-dead, and terminate semantics;
- cross-process mail/reply routing;
- component load, describe, replace, drop, and state transfer;
- inline-child addressing over the wire;
- TCP/load and handler-cost behavior at a process boundary.

`FleetBench` owns its processes and store roots and cleans them on drop. A test
should never discover unrelated processes by set difference and terminate them.

## Artifact preconditions and fixtures

Fleet tests that load wasm read the `dist/manifest.json` artifact set. If the
required stem is unavailable, use the repository's precondition helpers so the
skip/failure is explicit; do not search arbitrary `target/` directories for a
same-named stale file.

The fixture crates cover distinct contracts:

- shared kind vocabulary and a main multi-actor bundle;
- typed and reshaped state replacement;
- split capability surface;
- defaultless multi-actor selection;
- behavior script/host variants.

Reuse these when the contract matches. A new fixture creates another build
artifact and CI cost, so it should prove a boundary the current matrix cannot.

## Failure triage

| Failure | Likely layer |
|---|---|
| Pure encode/validation mismatch | unit test / kind schema |
| In-process settlement timeout | actor lineage, hold, or scheduler contract |
| SubstrateHarness unknown mailbox | load/wire/lineage name |
| Capture mismatch with correct mail | render/frame ordering |
| FleetBench cannot spawn | dist manifest, binary selector, process boot |
| FleetBench mail fails after spawn | RPC proxy, engine id, child registry |
| Only parallel CI fails | shared env/files, port allocation, timing assumption |

## Source routes

- Public SubstrateHarness API: `crates/aether-harness-substrate/src/`
- Scenario examples: `crates/aether-substrate-bundle/tests/substrate_harness_scenario/`
- FleetBench harness: `crates/aether-fleet-bench/src/lib.rs`
- Fleet scenarios: `crates/aether-substrate-bundle/tests/fleetbench_*.rs`
- Fixtures: `crates/aether-test-fixtures/`
- Decisions: ADR-0067 and the subsystem ADR for the behavior under test
