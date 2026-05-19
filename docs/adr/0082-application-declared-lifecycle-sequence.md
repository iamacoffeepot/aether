# ADR-0082: Application-declared lifecycle sequence

- **Status:** Proposed
- **Date:** 2026-05-19
- **Amends:** ADR-0074 §Decision 5 (retires `FRAME_BARRIER`)
- **Supersedes:** iamacoffeepot/aether#687 (closed)

## Context

The chassis lifecycle today — `init` → repeated (`tick` → `render` → `present`) → `shutdown` — is encoded in hand-written driver code per chassis (`crates/aether-substrate-bundle/src/{desktop,headless,hub,test_bench}/`). ADR-0074 §Decision 5 layered a single `FRAME_BARRIER: bool` const on every actor to mark "drains within the per-frame barrier vs runs free," and that const is the only first-class structure naming the frame. Everything else — what stages exist, what they emit, what order they fire — is implicit.

This shape has worn three frictions:

1. **Chassis duplication.** Desktop / headless / hub / test_bench each rebuild the same skeleton (cadence source, broadcast Tick, drain, capture/present). Adding or reordering a stage means editing four call sites with copy-paste patterns that drift.
2. **Stages aren't introspectable.** Tracing roots (ADR-0080) currently sender-stamp chassis pushes as `CHASSIS_MAILBOX_ID`. There's no labelled "this tree is the Tick at frame N" — the cause lives in `RootState.lifecycle` as a separate side-channel rather than in the actor graph itself.
3. **`FRAME_BARRIER` is a bit, not a shape.** "Frame-bound" is emergent from "participates in any per-frame stage." Today it's a manually-maintained per-actor const; one missed override (e.g. the render cap pre-ADR-0074 §Decision 7) silently breaks the drain barrier.

iamacoffeepot/aether#687 proposed a substrate-defined universal lifecycle graph with per-chassis node elision; that design drifted into dynamic readiness tables and bloom-filter membership optimisations whose complexity outran the actual problem. Closing it in favour of the simpler framing below.

ADR-0080 (substrate mail tracing + settlement) ships the primitive this design depends on: every causal chain has a `SubscribeSettlement(root, reply_to)` gate that fires `Settled { root }` when all in-flight descendants finish. A lifecycle driver can broadcast stage mail, subscribe settlement on the resulting root, and advance to the next stage on the reply — no separate drain counter, no per-actor `FRAME_BARRIER` to maintain.

## Decision

The application — desktop chassis, headless chassis, hub, test_bench, or any future binary — declares its lifecycle as an ordered directed graph of states. The substrate ships a `LifecycleDriverCapability` actor that owns the graph, broadcasts each state's kind in turn, awaits settlement, and advances on the resolved edge. Chassis `main` becomes a thin shim that builds the graph, hands it to the driver, and loops `driver.next(ctx)` until terminal.

### 1. `LifecycleGraph` builder

States are `(factory, next_kind, optional quit_kind)` triples. The state's own kind is inferred from the factory's return type — authors never write `::ID` anywhere.

```rust
LifecycleGraph::new()
    .state(|_| Init {})                          .next::<Tick>()
    .state(|ctx| Tick { dt: ctx.dt })            .next::<Render>()
    .state(|ctx| Render { vp: ctx.camera_vp() }) .next::<Present>()
    .state(|_| Present {})                       .next::<Tick>()
                                                 .quit::<Shutdown>()
    .terminal(|_| Shutdown {})
    .start::<Init>()
```

Builder type-state enforces at compile time:

- Every `.state(...)` must be followed by `.next::<K>()` before the next `.state(...)` / `.terminal(...)` / `.start(...)`.
- Exactly one `.start::<K>()` call before `.build()`.

Builder `.build()` (finalize) checks at compile-error-equivalent panic time (with a clear message — preferred over a runtime trap):

- Every `next` and `quit` target appears in the graph as a state or terminal.
- At least one terminal is reachable from `start`.
- Warn if `Quit` mail can flow in (the driver mailbox is registered) but no state declares a `quit` edge — the graph has no escape hatch.

### 2. `LifecycleDriverCapability` is a first-class actor

The driver is a `NativeActor` (passive cap shape — generics propagate through the existing `#[actor]` macro) registered at the `aether.lifecycle` mailbox. Generic over chassis context `C` so each chassis declares its own context type and the driver is concrete-per-chassis (`LifecycleDriverCapability<DesktopCtx>`, `LifecycleDriverCapability<HeadlessCtx>`, etc.). `C: 'static` matches the bound on `NativeActor` and the `DriverCapability` family; existing chassis state is already owned/`Arc`-shared, so the bound is a labelling constraint rather than a future-tax.

The driver owns the compiled `LifecycleGraph<C>`, the chassis context `C`, a subscriber table keyed by stage kind, the current state pointer, a `quit_pending: bool` flag, and a `terminal_reached: bool` flag. The chassis main loop drives cadence by mailing `Advance` to the driver per frame; the driver's `on_advance` handler:

1. Calls the current state's factory with `&C` to produce the stage payload bytes.
2. Broadcasts the bytes to every subscriber of that kind via the runtime-id envelope path (`send_envelope_traced`), so sender = `aether.lifecycle` and ADR-0080 sees the driver as the labelled root.
3. Advances the state pointer along the resolved edge — `quit` if `quit_pending && state.quit.is_some()` (consuming the flag), otherwise `next`.

`Advance` is fire-and-forget; no reply. The chassis main loop calls `advance` at its desired cadence (vsync, fixed-dt, replay-clock, test-harness step) without waiting on a synchronous reply.

The driver also handles `Quit` (sets `quit_pending = true`), `LifecycleSubscribe` (registers a mailbox for a stage; replies `LifecycleSubscribeResult::Err{stage, error}` if the stage isn't declared by the graph per §7), and `LifecycleUnsubscribe`. The quit flag persists across states with no declared `quit` edge — it's only consumed at a state where `quit` is reachable. This is the primitive for "drain frame before exit" (place `quit` on `Present`) or "save game before exit" (route `quit` to a `SaveGame` state with unconditional progression).

**Settlement gating arrives in PR 3.** PR 2 ships fire-and-advance (broadcast then advance immediately) so the core types land and the synthetic-chassis tests can exercise the state machine without the full settlement plumbing. PR 3's chassis migration adds settlement subscription — the driver waits on `Settled { root }` for the broadcast root before mutating its state pointer — so cadence couples to actual work completion. Subsequent revisions can then expose a per-state budget via the chassis's main loop without changing the driver's wire surface.

### 3. `Quit` kind, single hardcoded signal

`aether.lifecycle.quit` lands in `aether-kinds` as the one recognised lifecycle signal. No generalised signal bag — more signals → branching graphs → harder for agents to read off the topology. Anything else is application-level state inside actor handlers, not a lifecycle concern.

Chassis bridges OS signals to mail:

```rust
let quit_flag = Arc::new(AtomicBool::new(false));
ctrlc::set_handler({ let q = quit_flag.clone(); move || q.store(true, SeqCst) })?;

while !lifecycle.is_terminal() {
    if quit_flag.swap(false, SeqCst) {
        lifecycle.send_quit();
    }
    lifecycle.next(ctx);
}
```

Winit's `WindowEvent::CloseRequested` and a future `hub.shutdown` mail both fan into the same `Quit` mail. Three trigger sources, one kind, one consumption point.

### 4. `FrameContext` is per-chassis

The factory closure receives `&FrameContext`, where `FrameContext` is a chassis-defined struct that exposes whatever chassis state factories need (`dt`, `frame_no`, `camera_vp()` on desktop, the platform-time accessor on headless, recorded times on a replay chassis, etc.). The driver is generic over `Ctx`:

```rust
pub struct LifecycleDriverCapability<Ctx> { graph: LifecycleGraph<Ctx>, quit_pending: bool, ... }
```

Each chassis instantiates with its own `Ctx`. A desktop state cannot read camera_vp on headless because headless's `FrameContext` doesn't declare it — the chassis crate doesn't compile. This is the same shape ADR-0067's `TestBench` already uses for chassis-specific test surfaces.

### 5. Init sub-ordering: two consecutive states

Today the chassis boots capabilities before components (cap mailboxes need to exist before component subscribes resolve). Under this ADR, that's two consecutive lifecycle states:

```rust
.state(|_| InitCaps {})       .next::<InitComponents>()
.state(|_| InitComponents {}) .next::<Tick>()
```

`InitCaps` is broadcast to subscribers in the capability category; `InitComponents` to components. The driver doesn't enforce the category split — actors subscribe to whichever stage matches their lifecycle. Splitting into two states is purely topological; one state with internal sub-order would re-invent intra-stage ordering that the graph already expresses for free.

### 6. Per-state settlement; no separate drain barrier

Each state advances only when its broadcast root settles per ADR-0080. The per-frame `FRAME_BARRIER` const retires — frame-boundness is emergent ("the actor handles a frame-stage kind"). The `drain_frame_bound_or_abort` helper in `aether-substrate/src/chassis/frame_loop.rs` retires too; settlement is the gate.

ADR-0063 fail-fast applies via a per-state budget: if `subscribe_settlement` doesn't fire within `LIFECYCLE_STATE_BUDGET` (default 5s, matches today's `DRAIN_BUDGET`), the driver invokes `lifecycle::fatal_abort` with the state name and the in-flight chain dump from the `TraceObserver`. Same observable shape as today's wedged-frame abort, with a labelled cause.

### 7. Fail-fast subscribe (per-actor vocabulary check)

Each actor that hosts subscriptions (`LifecycleDriverCapability`, `InputCapability`, etc.) owns its own kind vocabulary and rejects unknown subscribes at wire time. Subscribing to `Render` on a headless chassis where the lifecycle graph doesn't declare a `Render` state returns `Err(UnsupportedStage)` from the driver. Actors learn at boot whether they're misconfigured for their chassis.

The lifecycle driver does not know about interrupt kinds (`Key`, `MouseMove`, `TcpReady`, etc.). Subscribing to `Key` on the lifecycle driver returns `Err(UnsupportedStage)` from the driver; subscribing on the chassis where `InputCapability` doesn't exist (e.g. headless) returns `Err(UnsupportedKind)` from `InputCapability`. Each actor is its own source of truth.

### 8. Interrupts are not part of the lifecycle graph

Input events, file-watch wakeups, TCP-ready signals, and other asynchronous sources flow through their own peer actors (`InputCapability`, future `TcpCapability`, future `FsWatchCapability`) — the same routing path as today. From a receiving actor's perspective, an interrupt is mail in the mailbox alongside `Tick`/`Render`/etc., processed in arrival order on the actor's single dispatcher thread (ADR-0038).

No special dispatch path, no cadence policy, no quiescence concerns. Concurrent access to actor state from interrupts is neither safe nor promised — all introspection goes through mail (send query, receive snapshot).

### 9. Introspection is mail-only

No direct-read quiescence guarantee in v1. Send a query mail to an actor; its single-threaded mailbox processes in turn; it replies with a snapshot. Works under any interrupt model. If a real-time profiler or hot-loop debugger appears later and a read-lock primitive becomes worthwhile, revisit then.

### 10. Trace shape: lifecycle driver is the root of every frame-stage chain

ADR-0080's `RootState.lifecycle` field carries the cause of each chain. Under this ADR, every frame-stage chain's `sender` is the lifecycle driver mailbox (`aether.lifecycle`) and its `RootState.lifecycle` is `Tick(frame_no) | Render(frame_no) | Present(frame_no) | InitCaps | InitComponents | Shutdown`. `send_mail_traced` output and Chrome trace dumps get clean per-stage subtrees; "what triggered this mail?" walks straight up to `aether.lifecycle.Tick of frame N`. Today's "parent: none on tick mail" gap (#743's symptom) disappears.

Non-lifecycle chassis sources (input fan-in, window events, hub-bridge, MCP-bridge) keep their existing `CHASSIS_MAILBOX_ID` sender. `CHASSIS_MAILBOX_ID` is not aliased to the lifecycle driver — the sentinel survives for "no actor sender" and the driver gets a normal actor mailbox.

### 11. Kind names move into the `aether.lifecycle.*` namespace

`aether.tick`, `aether.draw_triangle`-adjacent stage kinds, and other frame-stage names rename under the lifecycle namespace:

- `aether.tick` → `aether.lifecycle.tick`
- `aether.lifecycle.init_caps` (new)
- `aether.lifecycle.init_components` (new)
- `aether.lifecycle.render` (new — the existing `aether.draw_triangle` mail kind stays as a render-input kind, not a stage kind)
- `aether.lifecycle.present` (new)
- `aether.lifecycle.shutdown` (new)
- `aether.lifecycle.quit` (new — the Quit signal kind)

Stage kinds in one namespace make the lifecycle category visually distinct from interrupt kinds (`aether.key`, `aether.mouse_move`) and content kinds (`aether.draw_triangle`, `aether.audio.note_on`). The rename is the bulk of PR 4 in the migration sequence below.

### 12. Relationship to actor-framework `wire` / `unwire` hooks

The actor framework's per-actor boot sequence (`claim → init → wire → spawn`) and its `unwire` teardown counterpart sit at a layer below the lifecycle graph. They keep their existing shape under this ADR — they run once per actor instance and are not driven by stage broadcasts. Two interactions are load-bearing:

- **`wire` is where stage subscriptions install.** An actor that wants `Tick` mail subscribes during `wire` — `ctx.actor::<LifecycleDriverCapability>().subscribe(Tick::ID, my_mailbox)` — the same shape `InputCapability` already uses for `Key` / `MouseMove` subscribes (issue 640 phase 2). The fail-fast `Err(UnsupportedStage)` rejection from §7 fires at this site, so an actor authored against a stage the local chassis hasn't declared learns at boot, not at runtime.
- **`Shutdown` stage and `unwire` are not the same teardown.** The `aether.lifecycle.shutdown` broadcast arrives as mail with the actor's full mail surface still operational — cleanup that needs to talk to peers (save game state, flush a write, post a metric) belongs in a `#[handler] fn on_shutdown` body. The driver waits for the `Shutdown` root to settle before exiting its loop. *Then* the chassis tears actors down, which runs each actor's `unwire` on its own dispatcher thread — release native handles, drop wgpu resources, write the per-actor log ring per ADR-0081 §4. Two distinct phases with non-interchangeable surface: `Shutdown` is the graceful "everything still works" cleanup; `unwire` is the post-lifecycle "the world is going away" finaliser.

Initialisation has the symmetric split: actor-framework `init` is the per-actor "construct your state" callback before mail can arrive; `InitCaps` / `InitComponents` stage broadcasts (§5) are mail that arrives once the actor is fully wired and the driver enters its loop. Use `init` for "load-bearing state that must exist before any handler runs"; use an `InitCaps` / `InitComponents` handler for cross-actor wiring that depends on peers being ready (e.g. a cap that needs to send the hub its kind manifest after the hub cap has wired its receive side).

## Consequences

### Positive

- **Chassis main loops collapse.** Four chassis bodies (`desktop`, `headless`, `hub`, `test_bench`) each reduce to "build graph + loop `next` until terminal." Per-chassis differences (vsync vs fixed-dt vs synthetic clock) live in the chassis's `FrameContext` and factories, not in the loop structure.
- **`FRAME_BARRIER` retires.** Frame-boundness is emergent, not a per-actor const to maintain. The missed-override class of bug (pre-ADR-0074 §Decision 7) is structurally impossible — if you don't handle a frame stage, you don't participate.
- **Trace roots are labelled actors, not sentinels.** Every frame-stage chain's root sender is `aether.lifecycle`; ADR-0080 surfaces stage causality without a side-channel.
- **Fail-fast misconfiguration.** Subscribing to a stage that doesn't exist on this chassis errors at boot rather than silently never firing. Actors that need to be cross-chassis-portable can introspect the driver's declared kinds.
- **Cadence-agnostic.** A replay chassis, fixed-timestep physics chassis, or test-harness step-once chassis drops in without changing the driver — just the cadence loop and the `FrameContext`.
- **Quit semantics are visible.** "Drain frame before exit" / "save before exit" / "exit immediately" are topology choices, not handler-side flag-checks. The graph shape spells out the answer.

### Negative

- **Builder ergonomics are non-trivial.** Type-state enforcement for `.next` / `.start` / `.terminal` makes the builder type signatures dense; error messages on misuse may be opaque (`expected StateWithoutNext, found StateWithNext`). Acceptable for a primitive declared once per chassis; not acceptable for a per-actor surface.
- **One settlement subscription per state per frame.** ADR-0080's settlement subscription is a `HashMap<MailId, Vec<ReplyTo>>` insert + a `Settled` mail round-trip. At 60Hz × ~4 frame states = 240 round-trips/sec on the trace observer. Within the trace queue's design budget; negligible vs. the trace-event firehose itself.
- **Kind-name churn.** Renaming `aether.tick` → `aether.lifecycle.tick` and adding the other stage kinds touches every component subscriber and every test that mails Tick directly. PR 4 in the migration sequence. The new names will require a rebuild of all wasm components (prebuilt wasm carries the old kind id — `feedback_rebuild_wasm_after_sink_rename.md`).
- **Two consecutive init states.** Two broadcasts where today's chassis does one ordered boot. Acceptable cost — the topology is the order, and component init can now reasonably depend on cap init having settled.
- **Lifecycle driver is a load-bearing single actor.** A bug in the driver wedges every frame. Same property the chassis main loop has today; the surface is smaller and one-place but it's still on the critical path.

### Neutral

- **`Quit` is opt-in per state.** Chassis that never want clean shutdown (test-bench unit tests) omit `quit` edges; the driver loop terminates only when `start` reaches a `.terminal(...)` declared without a `next` (the natural `Shutdown` path). Symmetric: chassis that want forced-exit-on-second-quit (`SIGINT` twice within Ns) implement it in the chassis main shim, not in the driver.
- **No multi-sequence state machines.** Menu/Playing/Paused-style mode switches are out of scope. Migration path is clean if added later — today's single graph becomes one state's graph in the larger version.
- **CHASSIS_MAILBOX_ID survives.** ADR-0080's `MailboxId::NONE` sentinel still anchors non-lifecycle chassis sources (input, window, hub-bridge). The lifecycle driver gets a real mailbox; the sentinel is not aliased to it.

## Alternatives considered

- **Substrate-defined universal graph with per-chassis elision (iamacoffeepot/aether#687).** The predecessor. Rejected: dynamic readiness tables, bloom-filter membership optimisation, and per-chassis "this stage doesn't apply" elision logic added structure for a problem that doesn't exist. Applications already know what they do; declaring the sequence directly is simpler than declaring "the universal sequence minus these nodes."
- **Generalised signal bag instead of `Quit`-only.** A `Signal { kind: SignalKind }` mail with named signals (`Quit`, `Pause`, `Reload`, etc.). Rejected: multiple signals turn the graph into a branching state machine. Quit is the one signal that needs uniform substrate handling because cleanup invariants depend on it; everything else can live in actor-handler state without crossing the lifecycle boundary.
- **Override mechanism for transitions (state.set_next(K) mid-flight).** Allow factories to override the declared `next` at runtime. Rejected in favour of the cleaner per-state `quit` edge — `quit_pending` is the only conditional branch, and topology expresses the rest. A runtime-mutable graph re-introduces the dynamic-readiness shape we just rejected from #687.
- **Phase / `.repeating(...)` syntax.** Builder DSL with explicit "loop these states forever" syntax. Rejected: graph topology expresses the loop (cycle back from `Present` to `Tick`). One representation, not two.
- **Direct-read quiescence guarantee.** Promise that an actor's state is readable without sending a query mail, gated by some chassis-coordinated quiescence point. Rejected: ADR-0038's single-threaded actor model is incompatible with reads from another thread without locks. If a forcing function appears (real-time profiler, hot-loop debugger), revisit with a read-lock primitive then.
- **Interrupts declared in the LifecycleGraph builder.** Add `.interrupt::<Key>()` to the builder so input kinds participate in the lifecycle declaration. Rejected: interrupts route through peer caps (InputCapability, etc.) today and that surface is already first-class. Declaring them in the lifecycle graph would mean the driver intermediates input fan-out, adding a hop without changing observable behaviour.
- **Companion-thread special case for shared-state actors.** Carve out a "shared state" actor category (audio, etc.) with a coordinated read path. Rejected: all state mutation goes through mail handlers; audio's cpal callback is fed via lock-free SPSC queue (post-ADR-0039), not via direct state read. If a future actor genuinely needs shared-state semantics, an `#[actor(shared_state)]` opt-in marker is the path — separate ADR.

## Migration

Four PRs roughly:

1. **This ADR.** Self-merge OK if CI green (docs-only).
2. **Core types** (`aether-substrate` + `aether-kinds`): `LifecycleGraph` builder, `LifecycleDriverCapability`, `Quit` kind, lifecycle stage kinds (`InitCaps`, `InitComponents`, `Tick` rename, `Render`, `Present`, `Shutdown`). Synthetic-chassis tests with hand-written `FrameContext`. Does not migrate any production chassis yet.
3. **Chassis migration**: port `desktop` / `headless` / `hub` / `test_bench` chassis main loops to `LifecycleDriverCapability`. Bridge ctrlc + winit close + hub-shutdown to `Quit` mail. Retire `FRAME_BARRIER` const and `drain_frame_bound_or_abort`. Each chassis defines its own `FrameContext`.
4. **Component migration**: every actor handling `Tick` migrates to the new kind name (`aether.lifecycle.tick`). All wasm components rebuild. Scenario YAML files that mail Tick directly update their kind names.

PR 2 is the largest landing. Subsequent PRs are mechanical renames + per-chassis ports.

## Follow-up work

- **Multi-sequence state machines** (menu / playing / paused). Out of scope for v1; if added later, the v1 single graph becomes one state's nested graph.
- **Per-state budget tuning.** v1 ships one `LIFECYCLE_STATE_BUDGET` const (default 5s) for all states. If `Render` legitimately takes longer than `Tick` on heavy scenes, expose per-state overrides at builder time.
- **Trace-graph + lifecycle integration.** ADR-0080's `RootState.lifecycle` enum gains explicit `Tick(frame_no) | Render(frame_no) | Present(frame_no) | InitCaps | InitComponents | Shutdown` variants; the driver populates them at root-mint time. Today's chassis-source string labelling retires.
- **`describe_lifecycle` MCP tool.** The lifecycle graph is introspectable bytes (state names, edges, factories' kind signatures). A `describe_lifecycle(engine_id)` tool surfaces it for agents — same shape as `describe_kinds` / `describe_component`. Defer until an agent loop benefits.
