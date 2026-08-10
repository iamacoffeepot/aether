# SubstrateHarness, FleetHarness, and LaneHarness

Aether has three integration harnesses because “use the real actor runtime,”
“use the real engine process boundary,” and “use the real Bloomery coordinator
boundary” answer different questions.

| Harness | Boundary crossed | Best for |
|---|---|---|
| Unit/pure test | function/module only | codecs, parsers, state machines, validation |
| `SubstrateHarness` | real substrate, scheduler, capabilities; in process | actor chains, settlement, frames, filesystem, component behavior |
| `FleetHarness` | real hub RPC plus forked child process | stores/selectors, spawn/terminate, proxy routing, cross-process load/replace |
| `LaneHarness` | forked production Bloomery coordinator plus local lane process | coordinator progress, durable dispatch/evidence/retry/wedge contracts |

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
- lifecycle driver plus synthetic window-event and tick stages;
- deterministic synthetic windows and selector-aware window-event routing;
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

`send_and_settle` and related primitives wait for the pushed causal chain to
settle before the next observation. A slow-chain heartbeat can extend patience;
the cumulative cap identifies a genuine wedge and reports pending roots/hold
counts.

The frame pump also subscribes to the exact lifecycle root it is waiting on.
While that chain remains outstanding, quiet polls stay at the 50 µs floor;
after settlement, or when no exact chain is available, they resume geometric
backoff toward `AETHER_HARNESS_POLL_CAP_MICROS`. This keeps a silent wasm
handler's frame measurement from absorbing a coarse observer sleep without
pinning slow reply-only waits to the fine cadence. Historical frame timings
collected before issue 4454 with the default 10 ms ceiling should be treated as
observer-inflated unless they were remeasured or explicitly used a fine cap.

This gate prevents a common flaky pattern:

```text
send producer mail
capture/assert immediately
descendant work arrives after the assertion
```

## Choosing a wait

Three operations wait, and they are not interchangeable. Pick by where the
effect being asserted on actually lands.

**The effect is on the caller's chain** — the recipient's handler produces it,
or a descendant mail it sent does. Use `send_and_settle`. It blocks on
`Settled { root }`, so the handler and everything it spawned have run before the
next step starts. This is the strongest barrier and the right default: whenever
lineage carries the effect, preserve the lineage and let settlement order it.

**The effect is genuinely detached** — it lands on a chain the caller never
joins, so no settlement here can order it. A `MonitorNotice` pruning a parent's
view after a child departs is the canonical case; so is a slot's own teardown
turn retiring an id. Use `poll_until`, which re-sends a probe mail until its
reply satisfies an observation or a wall-clock budget elapses:

```rust
HarnessOp::poll_until(WindowCapability::NAMESPACE, &ListWindows, move |reply: &ListWindowsResult| {
    matches!(reply, ListWindowsResult::Ok { windows }
        if windows.iter().map(|window| window.id).eq([surviving]))
});
```

The budget is wall clock rather than an iteration count, so a starved runner
takes more probes and still passes while a real regression still fails inside
the bound. When the observation never holds, the step fails with the value its
last probe actually saw — `last aether.window.list_result seen: Ok { windows: [] }` —
so the red names the state reached rather than only that the wait ran out.
`poll_until_within` takes an explicit budget; the satisfying reply is stored
under the step's label, so `ExecutionResult::reply` decodes the observation that
ended the wait.

**A reply correlates the request, and that is all** — use `send_and_await_reply`,
and assert only on the reply itself. It resolves on the matching correlation id
and waits for nothing else, so work the handler kicked off may still be in
flight. Asserting past that reply is asserting on the runner's speed.

What none of them license is an arbitrary sleep, or an extra round trip added
until the assertion passes. Both hold only while the box is fast enough, and a
label like `"process child monitor notice"` on a spare `send_and_await_reply` is
the tell that a round-trip count is standing in for an ordering the test could
not express. Reach for `poll_until` there instead.

## Declarative operation sequences

`SubstrateHarness::execute` runs labelled `HarnessOp`s and centralizes the settlement
discipline:

- `Advance` drives complete frames. `HarnessOp::advance(n)` represents
  16,667 µs per frame; use `HarnessOp::advance_by(n, duration)` when elapsed
  time is part of the behavior under test;
- `SendAndSettle` sends typed mail and waits for its whole causal chain to
  settle — the strongest barrier;
- `SendAndAwaitReply` stores a typed reply for later decode, and waits for
  nothing beyond that correlation — the weakest;
- `PollUntil` re-probes to a wall-clock budget for an effect no chain here can
  settle;
- `Capture` reads the current frame;
- `CaptureWithMails` atomically applies pre-mail, captures, and performs
  after-mail cleanup.

The three that wait are covered above under [Choosing a wait](#choosing-a-wait).

`ExecutionResult` retrieves output by label. This is the typed-Rust successor to
the retired YAML scenario runner: the compiler checks kind construction, while
the harness owns ordering.

Component-composition tests can place a loaded actor beneath any already-live
logical parent with `HarnessOp::load_component_under`:

```rust,ignore
let operation = HarnessOp::load_component_under(
    parent_name,
    LoadComponent {
        wasm,
        name: Some("worker".to_owned()),
        config: Vec::new(),
        export: Some("example.worker".to_owned()),
    },
);
let result = harness.execute(vec![("load-worker", operation)])?;
let loaded = result.reply::<LoadResult>("load-worker")?;
```

The component host resolves `parent_name` through the live registry, uses its
canonical path as the new actor's lineage, and returns the ordinary
`LoadResult`. On success, `LoadResult::Ok.name` is the canonical child address,
for example `PARENT/aether.embedded:worker`; an unknown parent produces
`LoadResult::Err`. This makes parent-relative `PeerCtxExt::peer` and
`peer_named` routes testable across explicit and nested component scopes.
Ordinary `LoadComponent` mail still loads beneath `aether.component`, and this
harness constructor does not add an MCP or production-hub load mode.

Use `CaptureWithMails` when geometry must land in the same frame as readback;
separate send/capture steps describe a different temporal contract.

## Measuring GPU program cost

Do not measure GPU work by differencing the wall clock of two captures.
Capture maps the frame and encodes PNG synchronously; deflate cost follows image
entropy, so two frames with identical draw work but different pixels can report
wildly different "GPU" costs.

For authored render programs, opt into timestamp queries and read the folded
per-pass table instead:

```rust,ignore
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::{
    ProgramTimingsResult, RenderHarnessBuilderExt, RenderHarnessExt,
};

let mut harness = SubstrateHarness::builder()
    .size(900, 1200)
    .with_render_pass_timings()
    .build()?;

// Register the program, then dispatch it over consecutive advance frames.
// Keep capture out of this run; timestamp readback resolves asynchronously.
for _ in 0..40 {
    harness.execute(vec![("frame", HarnessOp::advance(1))])?;
}

match harness.program_gpu_timings(program_id)? {
    ProgramTimingsResult::Ok { rows, .. } => report(rows),
    ProgramTimingsResult::Absent { reason } => eprintln!("GPU timings unavailable: {reason}"),
    ProgramTimingsResult::Err { reason } => return Err(reason.into()),
}

// If visual evidence is also needed, capture it only after the timing run.
```

Each row is one declared pass and carries marginal `mean_nanos`,
`mad_nanos`, and `samples`; the row means add up to that program's share of
the GPU frame envelope. `Absent` is not a zero measurement: it explains that
timing was disabled, no frame has met the device yet, or the adapter lacks
timestamp-query support.

For a whole-frame wall-clock comparison rather than a program's GPU share,
time consecutive `Advance` runs after warm-up. The render runtime has one
submission in flight, so alternating conditions bills one frame's GPU wait to
the other condition. Capture remains a correctness/evidence operation, never
part of a timed run.

Root actors also have a typed operation sender. It resolves the recipient from
the actor identity and lets the event kind infer from `&mail`:

```rust
HarnessOp::actor::<SyntheticWindowCapability>().send(&SubscribeWindow {
    selector: WindowSelector::All,
    kind: Key::ID,
    mailbox: observer,
});
```

The constructor accepts only root identities and `send` compiles only when the
actor handles that direct kind. Once a root `CreateWindow` operation has
settled, send an id-less control to its addressed child. Derive the boundary
address from the manager identity rather than copying its namespace literal:

```rust
use aether_actor::Addressable;

let main = format!("{}://main", WindowCapability::NAMESPACE);

HarnessOp::send_and_await_reply(
    main,
    &SetWindowTitle { title: "Inspector".to_owned() },
);
```

The canonical spelling of that recipient is
`aether.window/aether.window.instance:main`; both forms resolve to the same
live child mailbox. Synthetic window events deliberately use a separate
generic convenience constructor:

```rust
HarnessOp::window_event(WindowId(2), &Key { window: WindowId(2), code: keycode });
```

`window_event` accepts any `K: Kind`, encodes it once, and hands the runtime its
`KindId`; neither the harness nor `aether-window` maintains a table of input
kinds. The synthetic actor unions and deduplicates `All` and `One(window)`
subscribers, then emits tracked descendant envelopes. When `execute` returns,
inline observers and any other descendants have settled. This is test behavior:
the production headless chassis remains fail-fast for every window request and
does not expose synthetic injection.

## Visual evidence

`ArtifactGuard` preserves PNG, check results, and optional reference evidence on
panic or explicit persistence. It avoids filling successful CI runs with images
while making a failed visual assertion inspectable.

For a declarative sequence where failure evidence is useful before any visual
assertion, opt in explicitly with `SubstrateHarness::execute_with_diagnostics`.
It returns the same typed `ExecutionError` as `execute`; on failure only, it
writes `target/substrate-harness-artifacts/execution/<id>/diagnostics.json`.
The versioned record contains the original id, failure category/message and
failing label, completed labels in order with output class and byte length, and
the oldest-first observed kind names. It intentionally excludes reply bytes and
PNG data. Diagnostic I/O is best-effort and cannot replace the primary error.
CI already uploads this artifact root on failure. Keep visual PNG evidence with
`ArtifactGuard`; the execution bundle is non-visual progress context.

Pair it with structural checks (dimensions, non-background pixels, regions,
reference relation) rather than only golden-byte equality. GPU/render changes
can be semantically correct without byte-identical PNG compression or edge
rasterization.

## FleetHarness topology

`FleetHarness` is the `aether-harness-fleet` test-support crate, taken as a
dev-dependency by the fleet scenario suites. It
starts a real hub, connects over the production RPC framing, and can fork actual
child substrate binaries — the headless chassis resolves through
`dist/manifest.json` (run `cargo xtask dist` first, or set
`AETHER_HARNESS_FLEET_HEADLESS_BIN`). It exercises the same boundary an MCP
coordinator uses without requiring an interactive MCP session.

Use it for:

- binary/component artifact store and selector behavior;
- spawn failure, heartbeat, recently-dead, and terminate semantics;
- cross-process mail/reply routing;
- component load, describe, replace, drop, and state transfer;
- inline-child addressing over the wire;
- TCP/load and handler-cost behavior at a process boundary.

`FleetHarness` owns its processes and store roots and cleans them on drop. A test
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

## LaneHarness topology

`LaneHarness` is the Bloomery scenario tier for contracts that cross the
coordinator's durable, asynchronous boundary. Use it when a test must show that
a sealed bloom is dispatched, observed, and brought to a recorded resolution or
wedge through the same coordinator path a local operator runs. It is not a
replacement for reducer unit tests or executor seam tests; those remain the
narrower choices when the coordinator boot and process boundary cannot affect
the result.

A scenario forks the production `bloomery` binary in a scratch repository and
talks to it over its RPC surface. The production chassis boots the SQLite
journal, reducer, projection, all reactors, outbox drain, polling timers, and
intake path. A dispatch uses the production `ProcessTransformRunner`: it
materializes the sealed checkout with `git worktree add`, scrubs the child
environment, spawns a subprocess, reads its exit status and `evidence.json`,
and captures a candidate worktree. The [LaneHarness module](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-chassis-bloomery/tests/lane/mod.rs)
documents the live boundary and provides the scenario API.

The substitutions are deliberately narrow. Tests set the lane-program
configuration to the repository's mock-lane binary, which accepts the real
dispatch argv and writes deterministic evidence instead of running `cargo xtask
transform`; they also use the fixture GitHub backend for aggregate-line
scenarios. Therefore this tier does not evaluate model quality, run the real
transform program, or prove live GitHub credentials and transport. Test those
contracts at their own boundary.

`LaneHarness::settle` polls the projection and checks liveness on every poll.
The [liveness source](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-chassis-bloomery/tests/lane/liveness.rs)
makes two failures universal: quiescence while work is still owed, and a
dispatched order that never completes. The
[`lane_boundary.rs` scenarios](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-chassis-bloomery/tests/lane_boundary.rs)
then add named assertions for green dispatches, repair laps, evidence failure,
and accountable wedges. Prefer this tier when the contract needs both kinds of
proof: its named outcome and the fact that the coordinator did not silently
stop short of it.

## Failure triage

| Failure | Likely layer |
|---|---|
| Pure encode/validation mismatch | unit test / kind schema |
| In-process settlement timeout | actor lineage, hold, or scheduler contract |
| SubstrateHarness unknown mailbox | load/wire/lineage name |
| Capture mismatch with correct mail | render/frame ordering |
| FleetHarness cannot spawn | dist manifest, binary selector, process boot |
| FleetHarness mail fails after spawn | RPC proxy, engine id, child registry |
| LaneHarness stalls or leaves an order outstanding | coordinator polling, outbox, reactor, intake, or local lane lifecycle |
| Only parallel CI fails | shared env/files, port allocation, timing assumption |

## Source routes

- Public SubstrateHarness API: `crates/aether-harness-substrate/src/`
- Scenario examples: the per-cap scenario suites (e.g. `crates/aether-render/tests/`, `crates/aether-text/tests/`)
- FleetHarness harness: `crates/aether-harness-fleet/src/lib.rs`
- Fleet scenarios: the per-cap `fleetharness_*.rs` suites (e.g. `crates/aether-component/tests/`, `crates/aether-fleet/tests/`)
- Fixtures: `crates/aether-test-fixtures-*/`
- LaneHarness: `crates/aether-chassis-bloomery/tests/lane/mod.rs`
- Lane liveness invariant: `crates/aether-chassis-bloomery/tests/lane/liveness.rs`
- Lane boundary scenarios: `crates/aether-chassis-bloomery/tests/lane_boundary.rs`
- Decisions: ADR-0067 and the subsystem ADR for the behavior under test
