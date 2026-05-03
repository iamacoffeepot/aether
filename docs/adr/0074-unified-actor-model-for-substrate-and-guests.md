# ADR-0074: Unified actor model for substrate and guests

- **Status:** Accepted
- **Date:** 2026-05-03

## Context

The substrate has two long-lived entity classes that own state and respond to mail:

- **Components** — wasm guests, sandboxed, dispatched from `ComponentTable`. ADR-0038 settled their concurrency model: one OS thread per component, mpsc inbox, lifecycle is channel-drop + join. The guest SDK (`aether-component`) ships `Sink<K>` typed sends, `Mail<'_>` typed receives, `Ctx::reply`, `wait_reply` for sync request/reply, the `Component` trait, and the `#[handlers]` macro for declarative dispatch.
- **Capabilities** — native Rust services owned by the chassis. Today: `render`, `audio`, `io`, `net`, `log`, `handle`. Each implements `Capability` / `RunningCapability` from `aether-substrate::capability` and hand-rolls its own dispatch loop.

These two evolved independently and the divergences add up:

- **No shared SDK.** Capabilities recreate dispatch boilerplate. Each writes its own `loop { recv_timeout(); match kind_id { ... } }`, builds mail envelopes by hand via `mail_send_handle().send(...)`, and has no declarative handler macro. The component SDK's primitives (`Sink<K>`, `Mail<'_>`, `Ctx`, `#[handlers]`, `wait_reply`) are the more mature actor library and don't reach native code.
- **Inconsistent thread topology.** Five capabilities have one OS thread each; `render` has two (one per mailbox: `aether.sink.render` and `aether.sink.camera`). Components have one each by ADR-0038. The capability trait surface declares nothing about threading, so a reader has to read each implementation.
- **Polled shutdown.** Every capability uses an `Arc<AtomicBool>` flag polled every 100 ms by `recv_timeout(SHUTDOWN_POLL_INTERVAL)`. Six capabilities × 100 ms is up to 600 ms of clean-exit latency. Components retired this years ago — they shut down by dropping the inbox sender, which causes `recv()` to return `Disconnected`, the actor exits, and `join` completes.
- **No host-side `wait_reply`.** Guests have a sync send-and-await primitive via the `wait_reply_p32` host fn. Native capabilities have no equivalent — capability-to-capability communication is undefined. A future Gemini capability that needs to call `net` has no documented pattern.
- **Sinks as a special-case dispatch path.** When a component sends mail to a capability mailbox today, the capability's handler runs *synchronously on the calling component's dispatcher thread* (`mailer.rs:233`). This is the "sink" mechanism. Components-to-components, by contrast, is async via mpsc. The same `send_mail` API maps to two different concurrency semantics depending on the recipient class — and the differentiation is invisible to the sender.

The sink-as-sync mechanism exists for a real reason: per-frame consistency. The desktop chassis frame loop is:

1. Push `Tick` mail to subscribers.
2. `frame_loop::drain_or_abort` calls `Mailer::drain_all_with_budget(5s)` — a synchronous barrier that waits until every component dispatcher's inbox is empty AND no `deliver` is in flight, re-checking until quiescence (`mailer.rs:202–211`, `frame_loop.rs:67–72`).
3. Chassis driver submits the frame (`gpu.render()`).
4. Vsync → repeat.

Sinks running synchronously on the caller's thread means that when the barrier reports component quiescence, every sink call has also completed. Render's `frame_vertices` is fully populated; `gpu.render()` reads complete state; no race.

If we naively make capabilities into proper actors with their own mpsc inboxes (the obvious end state for unifying with components), this property breaks. Render's inbox isn't in the barrier, so frame submission can race with un-processed `DrawTriangle` mail. The fix is structural: render's inbox joins the per-frame drain barrier alongside component inboxes. The barrier extends from "all components quiescent" to "all components ∪ frame-bound capabilities quiescent." Render is the one capability that needs this; the rest (audio, io, net, log, handle) run on their own clocks and shouldn't be tied to the frame schedule.

With the frame barrier providing the consistency property structurally, sinks-as-sync no longer earn their place in the model. Every recipient becomes a real actor with its own thread; per-frame consistency comes from the barrier; the substrate has one fewer concept.

This ADR settles the unified model.

## Decision

Ten sub-decisions, all of one piece. Implementation phases follow in *Consequences*; the model itself is non-divisible.

### 1. Actor as the unit of substrate execution

Components and capabilities collapse into one model. Every long-lived substrate-side entity that owns state and responds to mail is an *actor*:

- Owns one mpsc inbox.
- Owns one OS thread that loops `inbox.recv() → dispatch(envelope)`.
- Identified by one `MailboxId`.
- Communicates with the rest of the system exclusively via mail.

Components and capabilities are *deployment targets* for actors: components are wasm-compiled actors run inside a sandbox; capabilities are native-Rust-compiled actors linked into the chassis. The conceptual model is identical; the build profile differs.

### 2. Shared actor SDK with two transport implementations

The actor primitives — `Mailbox<K>`, `Mail<'_>`, `Ctx`, `wait_reply`, the `Actor` trait, and the `#[handlers]` attribute macro — extract into a shared crate (working name `aether-actor`). The SDK is `no_std + alloc` (already a constraint from the guest side) and target-agnostic.

Transport is abstracted behind a trait:

```rust
pub trait MailTransport {
    fn send(&self, recipient: MailboxId, kind: KindId, payload: &[u8], correlation: u64);
    fn wait_reply(&self, kind: KindId, capacity: usize, timeout_ms: u32, correlation: u64)
        -> Result<Vec<u8>, WaitError>;
}
```

Two implementations:

- **`GuestTransport`** (in `aether-component`): forwards to the existing `_p32` host fns.
- **`NativeTransport`** (in `aether-substrate`): forwards to the in-process mail dispatcher and, for `wait_reply`, the new host-side correlation/oneshot mechanism (decision 9).

Authoring a native capability becomes shape-identical to authoring a guest component:

```rust
impl Actor for GeminiCapability {
    const FRAME_BARRIER: bool = false;
}

#[handlers]
impl Actor for GeminiCapability {
    #[handler] fn on_prompt(&mut self, ctx: &mut Ctx<'_, NativeTransport>, mail: GeminiPrompt) { ... }
    #[handler] fn on_net_reply(&mut self, ctx: &mut Ctx<'_, NativeTransport>, mail: NetFetchResult) { ... }
}
```

The `#[handlers]` macro emits the same dispatch table either way; the only difference at the use site is the `Ctx` transport parameter.

### 3. Lifecycle: channel-drop + join, uniformly

`Arc<AtomicBool>` shutdown polling retires across every capability. The chassis-side handle holds the inbox sender; dropping it during shutdown causes `recv()` to return `Disconnected`; the actor exits its loop; `join` completes. Identical to ADR-0038 components.

Worst-case clean-exit latency drops from ~600 ms (six capabilities × 100 ms poll) to "the time the slowest actor needs to drain its current envelope and return."

### 4. One actor = one OS thread

No `THREADS` const. One actor maps to exactly one OS thread for its lifetime. If an actor needs to do work concurrently with its inbox dispatch, the right shape is *composition*: the actor receives mail, dispatches work to a worker-queue actor or a pool of helper actors. Concurrency becomes a topology decision (more actors), not a per-actor knob.

This keeps the mental model crisp ("N actors = N threads") and removes the temptation to use a knob to paper over what should be an actor-topology decision.

### 5. Two scheduling classes via `FRAME_BARRIER`

The `Actor` trait declares one boolean property:

```rust
pub trait Actor {
    /// If true, this actor's inbox must drain (empty + no handler
    /// in flight) before the per-frame render submission. Components
    /// default true; capabilities default false; render overrides
    /// to true.
    const FRAME_BARRIER: bool;
}
```

Every component has `FRAME_BARRIER = true` (defaulted by the `Component` blanket impl). Every capability defaults `false` (`Capability` blanket impl). `render` overrides to `true`. Every other capability today (`audio`, `io`, `net`, `log`, `handle`) keeps the default.

The per-frame barrier extends from "drain all component dispatchers" to "drain all actors with `FRAME_BARRIER = true`." Mechanically the same `drain_all_with_budget` machinery — the set of inboxes to wait on grows by one entry. ADR-0063 fail-fast applies uniformly: a wedged frame-bound actor (component or capability) past `DRAIN_BUDGET = 5s` triggers `lifecycle::fatal_abort`.

### 6. Cross-class mail rules

The frame barrier converges as long as the cascade within the frame-bound set is finite. The four cross-class send patterns:

| Sender | Recipient | Pattern | Allowed? |
|---|---|---|---|
| frame-bound | frame-bound | any (send / wait_reply) | yes — barrier re-checks |
| frame-bound | free-running | fire-and-forget | yes |
| free-running | frame-bound | fire-and-forget | yes — lands in next-frame barrier |
| frame-bound | free-running | `wait_reply` | **no** — runtime-guarded fatal_abort |
| free-running | free-running | any | yes |

The forbidden case is the only one that breaks barrier convergence: a frame-bound actor calling `wait_reply` on a free-running actor parks the frame-bound thread on a free-running clock, blocking the barrier indefinitely. Under ADR-0063 the chassis would `fatal_abort` after `DRAIN_BUDGET`, but the rule is statically determinable from actor classes — better to abort at the call site with a clear diagnostic than to wait 5 s and abort with a "wedged dispatcher" message.

The check goes in `NativeTransport::wait_reply`: look up the recipient's class via the registry; if caller is frame-bound and recipient is free-running, `fatal_abort` immediately with a "frame-bound actor X attempted wait_reply on free-running actor Y — forbidden by ADR-0074" reason. Guest-side `wait_reply` does the same check on the host-fn side.

Frame-bound actors emitting observation/broadcast mail (e.g., render emitting a `FrameSubmitted` observation) is fine — observation routes outbound to the hub, doesn't loop back to actors, doesn't extend the barrier.

### 7. Render absorbs camera

Render becomes one actor with one mailbox. The `aether.camera` kind addresses render directly. The actor's dispatch table matches on kind id and routes to per-kind handlers. The `aether.sink.camera` mailbox name retires.

This collapses render's two-thread structure to one thread (consistent with decision 4) and removes the implementation-detail second mailbox name from the public substrate vocabulary.

### 8. Sinks retire (concept and name)

The "sink" concept — synchronous-on-caller dispatch as a special-case path — disappears. Every recipient is an actor with an mpsc inbox. Every send is async at the dispatch layer. Per-frame consistency that sinks used to provide is recovered structurally by the frame barrier (decision 5) plus FIFO ordering inside each actor's inbox.

The `Sink<K>` SDK type renames to `Mailbox<K>`. The `aether.sink.*` namespace retires from the well-known mailboxes registry. Capability mailbox names move to the `aether.<name>` form:

- `aether.sink.render` → `aether.render`
- `aether.sink.audio` → `aether.audio`
- `aether.sink.io` → `aether.io`
- `aether.sink.net` → `aether.net`
- `aether.sink.handle` → `aether.handle`
- `aether.sink.log` → `aether.log`
- (camera collapses into `aether.render` per decision 7)

ADR-0058's disambiguation principle still holds — the `aether.` prefix carries the load now, not the `sink.` infix. User-space components named `"render"`, `"camera"`, etc. continue to coexist without collision because the chassis-owned actors live under the `aether.` prefix.

### 9. Capability-to-capability via mail with host-side `wait_reply`

Capabilities communicate with each other the same way components communicate with capabilities and with each other: mail, with `wait_reply` for synchronous request/reply.

`NativeTransport::wait_reply` builds on the same correlation-id mechanism the hub's pending-replies machinery uses for `capture_frame`:

1. Generate a correlation id.
2. Register a `oneshot::Receiver` in a chassis-wide pending-replies map keyed by correlation id.
3. Send the mail with the correlation id.
4. Block on the oneshot with `timeout_ms`.
5. The dispatcher's reply path (`aether.control.reply` or kind-typed reply) checks the pending-replies map; matching correlation id routes the reply payload to the oneshot.
6. Return the decoded reply (or `WaitError::Timeout` / `WaitError::Cancelled`).

The frame-bound→free-running guard from decision 6 lives at the top of this function.

### 10. Scoping: runtime contact surface vs chassis-driver-internal

"Everything is mail" applies to the **runtime contact surface** — the surface reachable by components, the hub, peer capabilities, and any other actor in the system. Through that surface, every actor is reached only via its mailbox.

The chassis driver — which constructs capabilities and owns their handles for the chassis lifetime — may hold direct method handles for orchestration concerns the runtime contact surface doesn't model. Today this means render's wgpu lifecycle: `install_gpu`, `record_frame`, `record_capture_copy`, `finish_capture`, `resize`, `device()`, `queue()`, `with_color_texture()`, `gpu()` — direct methods the desktop driver calls on its owned `RenderRunning`. These are not model violations because no actor in the system has access to them; they're chassis-internal orchestration.

The `RunningCapability::shutdown(self: Box<Self>)` trait method is the existing precedent — every capability already exposes a chassis-internal direct call. Render just has more.

A future capability that needs a similar carve-out documents it the same way: declare in the capability's documentation what the chassis-driver-internal API is and why mail wouldn't suffice. The default expectation is that mail does suffice — direct-method APIs are exceptions, named.

## Consequences

### Positive

- **One mental model.** Actor primitives, lifecycle, and threading work the same for components and capabilities. Reading any actor in the codebase requires learning the pattern once.
- **One SDK.** Authoring a native capability becomes shape-identical to authoring a wasm component. `#[handlers]` works in both. `wait_reply` works in both. Future SDK improvements (better error types, observability hooks, better macro diagnostics) ship to both surfaces in one PR.
- **Sink concept retires.** The substrate has one fewer special case. The "is this an async send or a sync handler call?" question disappears — it's always an async send to an actor's mpsc inbox.
- **Per-frame barrier extends cleanly.** Render's inbox joins the existing `drain_all_with_budget` machinery. Frame submission still happens after barrier convergence. Existing 1-frame-cascade semantics within the frame-bound set are preserved.
- **Capability-to-capability communication formalized.** A future Gemini capability has a documented pattern for calling `net`. No need to invent it ad-hoc.
- **Shutdown latency drops.** Up to ~600 ms saved on clean exit (six capabilities × 100 ms poll interval each).
- **Wire vocabulary collapses.** `aether.sink.*` namespace retires; one less convention to teach. The `aether.` prefix-as-disambiguator (ADR-0058) becomes the sole rule.
- **Render thread count drops 2 → 1.** One fewer OS thread, one fewer shutdown handle, one fewer mailbox in the registry.
- **Cross-class mail rules are statically determinable.** The forbidden case (frame-bound → free-running `wait_reply`) is caught at the call site with a clear diagnostic, not via a 5-second wedge timeout.

### Negative

- **Substantial cross-cutting refactor.** The SDK extraction touches `aether-component`, `aether-substrate`, every capability, every component, the `#[handlers]` proc-macro crate, and the chassis drivers. Phasing is mandatory — see implementation plan below.
- **Wire change.** `aether.sink.*` mailbox names retire. Components that send to `aether.sink.render`, `aether.sink.audio`, etc. need to update to `aether.render`, `aether.audio`, etc. The reference in-tree components are updated as part of the implementation; external components built against pre-this-ADR substrate need to retarget.
- **SDK rename.** `Sink<K>` → `Mailbox<K>`, `SinkHandler` → `MailboxHandler`. Mechanical rename across guest code (in-tree components and any external components).
- **`aether.sink.camera` mailbox retires.** Components addressing camera control via `aether.sink.camera` need to retarget to `aether.render` (the kind name `aether.camera` is unchanged; only the recipient mailbox changes).
- **CLAUDE.md updates.** The recipient-name convention paragraph, the well-known mailbox list, the chassis sink callout. ADR-0058 gets superseded in part (the `aether.sink.*` namespace half).
- **Issue 509's scope grows substantially.** What was framed as "extract a polling-shutdown helper" becomes the implementation tracker for the full unified model rollout. Worth re-naming the issue to reflect this.

### Neutral / forward

Implementation lands in phases, each shippable independently:

1. **Extract actor SDK.** Move `Mail`, `Ctx`, `wait_reply`, `WaitError`, `#[handlers]` into `aether-actor`. Define the `MailTransport` trait. `aether-component` re-exports for source compatibility during transition. No behavior change.
2. **Native transport + capability migration.** Implement `NativeTransport`. Migrate one capability (probably `log` — smallest surface) to use the shared SDK end-to-end. Validate. Migrate the rest one PR at a time: `handle`, `audio`, `io`, `net`. Each retires its own `Arc<AtomicBool>` polling and adopts channel-drop + join.
3. **Render collapse.** Render becomes one actor; camera mailbox folds in; render opts into `FRAME_BARRIER`. Per-frame drain barrier extends. The reference camera-component updates its addressing.
4. **Host-side `wait_reply` + cross-class guard.** Build the chassis-side correlation/oneshot mechanism. Add the frame-bound → free-running `wait_reply` guard. This unblocks capability-to-capability work.
5. **Wire rename.** `aether.sink.*` → `aether.<name>`. SDK rename `Sink` → `Mailbox`. CLAUDE.md sweep. Single PR with migration notes — the most externally visible piece.

`Capability::THREADS` reservation retires (the prior ADR-0074 draft proposed it; this rewrite drops it). Future tunability — multi-threaded actors, work-stealing pools — is a composition pattern using the actor primitives, not a trait property.

The `Arc<Mutex<...>>` patterns inside actor state (handle store's refcounted byte cache, audio's mixing pipeline, render's frame_vertices) are unchanged. This ADR is about thread topology, scheduling class, and dispatch shape — not about how each actor structures its internal mutability.

Hub-as-substrate (ADR-0034) and the test-bench chassis (ADR-0067) inherit the model unchanged. The hub chassis runs no frame-bound actors today (no render), so its drain story is trivially "nothing to drain"; if it ever hosts a frame-bound capability, the same machinery applies. The test-bench's `advance(ticks)` already drains components per frame; extending to frame-bound capabilities is the same one-line change as the desktop chassis.

## Alternatives considered

- **Status quo + extract the polling-shutdown helper.** The original framing of issue 509. Dedupes the lines but leaves the conceptual gap (no formal model, render still has two threads, sink-as-sync still a special case, capability-to-capability still undefined). Rejected as patching a symptom.
- **Per-capability concurrency model only (the prior ADR-0074 draft).** Bundled actor-per-capability + render absorbs camera + channel-drop shutdown + a `THREADS` const. Settled the capability side without unifying with components or addressing the sink-as-sync question. Rejected mid-draft because the dedup case is so strong that shipping the smaller ADR first creates rename churn for the larger one immediately after.
- **Render-submission as mail (`Submit` mail through render's inbox).** Considered as an alternative to the `FRAME_BARRIER` classification: chassis sends a `Submit` mail to render after Tick; render processes it FIFO so prior `DrawTriangle` mail is integrated; chassis waits for `SubmitResult` reply before vsync. Rejected because it round-trips through the inbox just to encode "submit now," which is bureaucracy for a per-frame chassis operation. The `FRAME_BARRIER` classification keeps submission where it belongs (chassis driver thread, after barrier) without round-tripping.
- **Type-state `Ctx` for frame-bound vs free-running.** Considered: frame-bound actors get a `FrameCtx` whose `wait_reply` is restricted at compile time to other frame-bound recipients. Rejected for SDK ergonomics — splits the `Ctx` surface, requires generic juggling for any code that wants to live in both worlds, and the runtime-guard alternative (decision 6) is cheap and fails just as loudly.
- **`THREADS` const for tunability.** Considered as a future-proofing seam: a capability could declare `THREADS = N` to opt into multi-threaded dispatch. Rejected as YAGNI and as the wrong shape — concurrency for an actor's work is better expressed as composition (helper actors, worker-queue actors) than as a per-actor knob. If the const ever earns its keep, a future ADR can add it.
- **Async (tokio) for IO-bound capabilities.** Considered. Log/io/net are naturally async. Rejected: forces tokio into the substrate runtime as a dep, propagates `async fn` through the actor trait surface, and brings all the lifetime/`Send` constraints async brings. The current envelope rates don't justify the complexity. If the hub server (which already uses tokio for `axum`) ever shares a runtime with substrate-side IO, that's a separate ADR.
- **Worker pool for IO-bound capabilities multiplexed onto K threads.** Considered. Lets the operator tune K. Rejected for v1: low envelope volume in current workloads doesn't pay for the multiplexing complexity, and audio's `!Send` cpal stream constraints don't fit a pool model. Single-thread-per-actor stays the default; multi-actor topologies are the escape hatch when concurrency matters.
- **Keep sinks-as-sync as a separate dispatch class.** Considered as a hybrid: actors are async, sinks are sync, both addressable by the same `send_mail` API. Preserves today's race-freedom for draws without the frame-barrier extension. Rejected because it carries the two-class fiction forward indefinitely, and the conceptual cost (every contributor learns the special case) outweighs the implementation simplicity.
