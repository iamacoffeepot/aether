# SubstrateHarness and FleetHarness

Aether has two integration harnesses because “use the real actor runtime” and
“use the real engine process boundary” answer different questions.

| Harness | Boundary crossed | Best for |
|---|---|---|
| Unit/pure test | function/module only | codecs, parsers, state machines, validation |
| `SubstrateHarness` | real substrate, scheduler, capabilities; in process | actor chains, settlement, frames, filesystem, component behavior |
| `FleetHarness` | real hub RPC plus forked child process | stores/selectors, spawn/terminate, proxy routing, cross-process load/publish |

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
- logging/tracing rings and typed replies;
- the inventory capability, so a scenario may mail `aether.inventory` or load a
  component that depends on it with no extra composition.

It is not a mock engine. The simplification is process/transport ownership.

## Builder and isolation

`SubstrateHarnessBuilder` configures the boundary a test needs: target size, namespace
roots, worker count, log/trace capacities, settlement cap, and clipboard mode.

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

On a harness built with render, `send_and_settle` drains the pumped
`aether.render` slot on its mail wake while it waits, through the same
`await_settlement_pumped` the desktop driver uses (ADR-0161
§Decision 2): a chain that reaches the render actor settles because each render
mail arrival triggers a drain, and there is no fixed drain round. The heartbeat
only logs; it never drains.

A reply wait (`advance`, `capture`, a `send_and_await_reply`) never sleeps. The
harness owns one `PumpWake` channel, and every reply source fires it after it
enqueues: the loopback recorder after each egress, the chassis event channel
after each event, and the pumped render slot after each accepted mail. A quiet
pump blocks on that channel until a source has work, and the settlement cap
bounds the block as a wedge backstop, never as a poll interval. No wake is
lost: only the pump loop empties the queue during a wait, and it always
empties the queue before it drains the sources. A wake it empties was fired
after its item was queued, so the drain finds the item, and an item queued
later fires a wake that stays queued. A render `send_and_settle` waits on the
same channel between pump waits.

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
let window = harness.actor_ref::<WindowCapability>();
HarnessOp::poll_until(&window, &ListWindows, move |reply: &ListWindowsResult| {
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
  time is part of the behavior under test. The total a `Tick` carries is the sum
  of the stated frame durations, each added whole, so a test that states elapsed
  time can assert exact steps;
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

Component-composition tests place an instanced type beneath any already-live
logical parent with `SubstrateHarness::spawn_child::<P, C>`, which takes the
parent's already-proven reference and the child's key:

```rust,ignore
let root = harness.load::<RootManager>(load)?;
let worker = harness.spawn_child::<RootManager, Worker>(&root, &LoadName::new("worker")?)?;
```

`spawn_child` compiles only when `Worker` declares `child_of` `RootManager`
(ADR-0241 §5), and a parent with no retained actor path is refused with
`SubstrateHarnessError::Spawn`. The child's canonical path, for example
`PARENT/example.worker:worker`, comes from `harness.actor_path(&worker)`.
Ordinary `aether.component.load` places its guest at the root: a parent is a
`Spawn` field, not a load field, and the hub and MCP surfaces send it the same
way `spawn`'s `parent?` argument does.

A load reply carries the component's path and no position: the loaded
trampoline sends the successful reply itself, so the reference to the loaded
actor is the reply's stamped sender. When a test needs that reference — to ask
`harness.accepts(reference, kind)` or `harness.actor_cost(reference)` — load
through the harness directly instead of through an operation:

```rust,ignore
let panel = harness.load::<WidgetPanel>(LoadComponent {
    wasm,
    name: Some("panel".to_owned()),
    config: Vec::new(),
    export: None,
})?;
```

`load::<R>` sets the export to `R::NAMESPACE` and returns `ActorRef<R>`;
`harness.actor_path(&panel)` reads its canonical path for an assertion or a
`CaptureWithMails` recipient. `load_any` sends the load as given and returns
the erased reference and its path, for a fixture actor the test cannot name,
which the test types with `SubstrateHarness::cast::<P>` before it sends.
Beside these two, `SubstrateHarness` carries the rest of the component verbs:
`publish` / `publish_configured` bind a module's namespaces without spawning
anything, `spawn::<R>` / `spawn_keyed::<R>` / `spawn_child::<P, C>` stand up an
instance of an already-published type, `spawn_any` does the same for a type
the test cannot name, and `actor_ref::<R>` / `actor_path` prove and read back
a composed capability's reference and any reference's canonical path. A
drop takes the path, `DropComponent { target: path }`; a republish names no
instance — `publish_configured(successor_code, configs)` moves every live
instance of the module's namespaces (ADR-0241 §7), so a test that swaps
identical code builds a new hash with
`aether_substrate::testing::successor_wasm(&wasm, generation)`.
FleetHarness sends the same kinds over the wire: `load(engine, &LoadComponent)`
returns the path as text — `Loaded { addr, capabilities }` —
`publish(engine, wasm)` returns the published types, and `spawn(engine, &Spawn)`
returns `SpawnResult::Spawned` or `SpawnResult::Live`; `component_wasm(selector)`
resolves a registry selector to its bytes and `@actor` export first.

`with_pumped_component_host()` composes the component host as a pumped actor:
every harness wait drains it, and between waits it holds still.
`step_component_host_through::<K>(n)` runs it one envelope at a time until it
has dispatched `n` mails of kind `K`, so a test can hold a republish after its
members answered `Prepared` and before any commit, send mail with
`send_tracked`, and then await the republish.

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
    ProgramTimingsResult::Err { error } => return Err(error.into()),
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

Every send takes a proven reference (ADR-0230), never an address. A composed
capability's comes from `SubstrateHarness::actor_ref`, and the event kind infers
from `&mail`:

```rust
let synthetic = harness.actor_ref::<WindowCapability>();
HarnessOp::send_and_settle(&synthetic, &SubscribeWindow {
    selector: WindowSelector::All,
    subscription: WindowSubscription::Key(ActorPath::<Relay>::root().narrow()),
});
```

A send has three typed doors, and the compiler picks the one the reference and
the kind fit:

- a `&ActorRef<R>` takes a kind `R` handles;
- a `&ProtocolRef<P>` takes a kind the protocol `P` lists (ADR-0231 §3);
- either takes the framework tails, `LogTail`, `TraceTail`, and `CostTail`,
  which the dispatch loop answers for every actor whether or not its type
  declares a handler for them. `log_tail` sends through any typed reference.

An `ErasedActorRef` is not a send target: an erased reference has no send
verb (ADR-0231 §4). A test holding one, such as `load_any`'s answer, casts it
once with `SubstrateHarness::cast` against a test-local `#[protocol]` and
sends through the `ProtocolRef<P>`.

A loaded wasm component's reference comes from the load itself:
`SubstrateHarness::load::<R>` types the reply's stamped sender as the export
`R`, and `load_any` returns it erased for a fixture that ships only as wasm.
The load returns only after its chain settles, so the routes the component's
`wire` staged, its inline children included, are live and `child::<P, C>`
finds them.
`load_any` also returns the canonical lineage path the host reported; a typed
reference's path comes from `SubstrateHarness::actor_path`, for an assertion
against it or a `CaptureWithMails` bundle recipient:

```rust,ignore
let camera = harness.load::<CameraComponent>(load)?;
HarnessOp::send_and_settle(&camera, &Frame { bounds });
```

A wasm-only fixture's erased reference is cast with `SubstrateHarness::cast::<P>`
against a test-local `#[protocol]` naming the rows the test sends. The cast is
the registry's guard cast (ADR-0231 §4): it mints a `ProtocolRef<P>` only when
the route is `Live` and publishes every row of `P` with its exact reply, and
otherwise returns `SubstrateHarnessError::CastRefused` naming `P` and the path.
Every later send through the reference is compile-checked against `P`:

```rust,ignore
#[protocol]
trait StatefulCounter {
    fn bump(mail: Bump);
    fn count(mail: CountQuery) -> CountReport;
}

let counter = harness.cast::<StatefulCounter>(harness.load_any(&load)?.0)?;
HarnessOp::send_and_settle(&counter, &Bump);
```

A child an actor spawned — a widget beneath a panel, a window beneath the
window capability — is reached by type and key beneath a reference already
held, with `SubstrateHarness::child`. Once a root `CreateWindow` operation has
settled, send an id-less control to the child it opened:

```rust
let window = harness.actor_ref::<WindowCapability>();
let main = harness.child::<WindowCapability, WindowInstance>(&window, LoadName::new("main")?)?;

HarnessOp::send_and_await_reply(&main, &SetWindowTitle { title: "Inspector".to_owned() });
```

The lookup proves only a `Live` child: one never spawned, still starting, or
already dropped is refused with `SubstrateHarnessError::ChildRefused`, which
names the key and the child's namespace. Synthetic window events deliberately
use a separate generic convenience constructor, sent through the synthetic
window capability's reference:

```rust
let window = window_path(&LoadName::new("main")?);
HarnessOp::window_event(&synthetic, window.clone(), &Key { window, code: keycode });
```

`window_event` accepts any `K: Kind`, encodes it once, and hands the runtime its
`KindId`; neither the harness nor `aether-window` maintains a table of input
kinds. The synthetic backend unions and deduplicates `All` and `One(window)`
subscribers, then emits tracked descendant envelopes. When `execute` returns,
inline observers and any other descendants have settled. This is test behavior:
the production headless chassis composes no window actor and does not expose
synthetic injection.

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
`dist/manifest.json` (run `cargo xtask dist` first, set
`AETHER_HARNESS_FLEET_HEADLESS_BIN` for one binary, or set
`AETHER_HARNESS_FLEET_BIN_DIR` to the directory holding the bins
`cargo test --workspace` already built). It exercises the same boundary an MCP
coordinator uses without requiring an interactive MCP session.

Use it for:

- binary/component artifact store and selector behavior;
- spawn failure, heartbeat, recently-dead, and terminate semantics;
- cross-process mail/reply routing;
- component publish, spawn, load, describe, drop, and state transfer;
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
- multi-export selection (`aether-test-fixtures-defaultless`).

Reuse these when the contract matches. A new fixture creates another build
artifact and CI cost, so it should prove a boundary the current matrix cannot.

## Failure triage

| Failure | Likely layer |
|---|---|
| Pure encode/validation mismatch | unit test / kind schema |
| In-process settlement timeout | actor lineage, hold, or scheduler contract |
| SubstrateHarness unknown mailbox | load/wire/lineage name |
| Capture mismatch with correct mail | render/frame ordering |
| FleetHarness cannot spawn | dist manifest, binary selector, process boot |
| FleetHarness mail fails after spawn | RPC proxy, engine id, child registry |
| Only parallel CI fails | shared env/files, port allocation, timing assumption |

## Source routes

- Public SubstrateHarness API: `crates/aether-harness-substrate/src/`
- Scenario examples: the per-cap scenario suites (e.g. `crates/aether-render/tests/`, `crates/aether-text/tests/`)
- FleetHarness harness: `crates/aether-harness-fleet/src/lib.rs`
- Fleet scenarios: the per-cap `fleetharness_*.rs` suites (e.g. `crates/aether-component/tests/`, `crates/aether-fleet/tests/`)
- Fixtures: `crates/aether-test-fixtures-*/`
- Decisions: ADR-0067 and the subsystem ADR for the behavior under test
