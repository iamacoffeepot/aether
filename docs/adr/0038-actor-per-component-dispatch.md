# ADR-0038: Actor-per-component dispatch

- **Status:** Accepted
- **Date:** 2026-04-21

## Context

The current scheduler (ADR-0004 worker pool, extended by ADR-0010 runtime table, ADR-0022 drain-on-swap, and an unrecorded strand-claim fix for issue 157) dispatches mail via N worker threads that pull from a shared `Mutex<VecDeque<Mail>>` and run `deliver` against an `Arc<ComponentEntry>` they cloned from a shared `RwLock<HashMap>`. Coordination lives in three loose invariants on each entry:

- `pending: AtomicU32` — in-flight `deliver` count. ADR-0022 uses it for the replace drain.
- `frozen: AtomicBool` — dispatcher parks new mail instead of delivering. ADR-0022.
- `strand_scheduled: AtomicBool` — per-mailbox FIFO claim. Added after issue 157 when `Mutex<Component>` acquisition was found to be non-FIFO under contention.

Each new lifecycle primitive has to reason about all three + the Arc refcount + the shared queue's `outstanding` counter. The composition is fragile in practice:

- **PR #166 (drop panic).** `handle_drop` extracted the `Component` via `Arc::into_inner(entry).expect(...)`, which panics when a worker's post-dispatch tail still holds an Arc clone. The window between `mark_completed` (which wakes `wait_idle`) and the worker dropping its strand Arc is narrow but real; mac GHA jitter reproduced it on 2026-04-21. Fix was to drop the `Arc::into_inner` requirement entirely and rely on the `Mutex<Component>` lock for `on_drop` serialization.
- **Mac CI flake on `batched_mail_preserves_fifo_per_mailbox` (2026-04-21).** Hung once against PR #164's commit, passed on re-run. Suspected schedule-jitter interaction with strand claim + shared queue under macOS runner VM contention. Documented in `project_mac_ci_flake_watch.md`; escalates on a second occurrence.
- **Extract asymmetry.** `handle_replace` mutates the old `Component` through the `Mutex<Component>` (safe under any refcount) while the pre-PR-#166 `handle_drop` required unique Arc ownership. The invariant the scheduler comment claimed — "pending == 0 ⇒ no worker holds a clone" — turned out to be false: the worker holds the clone for its whole post-dispatch tail (drain loop + `strand_scheduled.store(false)` + Arc drop), which runs after `pending` decrements.

The shape the system keeps gesturing toward is **one consumer per mailbox, with lifecycle driven by that consumer's existence** rather than by auxiliary counters. wasmtime components are already actor-shaped: private state, message-in/message-out, no shared memory with the host or with peers. Modeling them as actors dissolves the three invariants into one primitive (a channel + a task handle).

This ADR is motivated by the incidents above, not by a speculative rewrite. The current scheduler works; it just keeps surfacing concurrency patches that each add a little more coordination state. We'd rather land a structural form while the substrate is still pre-1.0 than keep iterating on a shape we know we'll replace.

## Decision

Replace the worker-pool + strand-claim scheduler with **one dispatch task per component**. Each `ComponentEntry` owns:

- An `mpsc::Sender<Mail>` stored in the components table, keyed by `MailboxId`.
- A dedicated OS thread (or a tokio blocking task — see §2) that runs `while let Some(mail) = rx.recv() { deliver(mail) }`.
- A `JoinHandle` held by whoever owns the entry (substrate core) for shutdown.

Sending mail is `sender.send(mail)`. Dropping a component is `drop(sender) + handle.join()`. Replacing a component is freeze-drain-swap reframed as: stop sending to the old inbox, wait for its queue to drain, send future mail to the new inbox under the same `MailboxId`.

### 1. Per-component inbox

```rust
pub struct ComponentEntry {
    sender: mpsc::Sender<Mail>,
    handle: Option<JoinHandle<Component>>,  // component returned on close
    name: String,
    // ... capabilities, kind descriptors, etc., unchanged
}
```

The `Component` (and its wasmtime `Store`) lives on the dispatch thread's stack for the component's lifetime. It never escapes to the host side. `on_drop` runs on the dispatch thread as the last act before the thread returns and the `Component` drops.

### 2. Threading model

wasmtime calls are synchronous and can block the calling thread for arbitrary guest time. Two viable shapes:

- **Thread-per-component (OS threads).** Simplest; each component is a named `std::thread::spawn` with a bounded stack. Overhead ~1–2 MB/thread on linux + darwin. Fine up to hundreds of components. Uses std primitives only — stays consistent with the "std-only for core" rule ADR-0004 set.
- **Per-component blocking task on a tokio runtime.** Lower per-unit overhead (~1 KB base + channel). Requires pulling tokio into substrate-core, which we've so far avoided. Worth it only if we cross a scale threshold that OS threads can't hit.

**We ship thread-per-component.** At current scale (single-digit to low-tens of components), the memory cost is a rounding error against a wasmtime Store's own footprint (a few MB per component), and the simpler model pays for itself in reviewability. Revisit if a workload pushes past ~100 live components, which would be a separate ADR.

### 3. Sending mail

The existing host `send_mail` host-fn and the platform-thread input publish path both route to the registry to resolve `MailboxId`, then push into a shared queue. Under this ADR they route to the `Sender` stored on the entry instead:

- Sinks stay inline (unchanged from ADR-0010 §3).
- Component recipients go through the entry's `Sender`. `send()` on an unbounded channel is non-blocking; on a bounded channel it blocks the sender when full (backpressure — see §6).
- Dropped/unknown mailboxes take the same warn-drop path as today, checked at send time against the registry.

`MailQueue::outstanding` and the whole shared-queue shape retire. `wait_idle` for frame-scale drains becomes a question the chassis asks of individual mailboxes (or of a well-known "all components" set) rather than of a global counter.

### 4. Drop becomes channel close

```rust
fn handle_drop(&self, id: MailboxId) -> DropResult {
    registry.drop_mailbox(id)?;
    input::remove_from_all(&self.input_subscribers, id);
    let Some(entry) = self.components.write().unwrap().remove(&id) else {
        return DropResult::Ok;
    };
    drop(entry.sender);  // close inbox; dispatch thread sees recv() return None
    // Bounded wait on the dispatch thread finishing (fires on_drop + returns).
    let component = entry.handle.unwrap().join_timeout(DROP_TIMEOUT)?;
    // Component drops here, wasmtime reclaims.
    DropResult::Ok
}
```

No `Arc::into_inner`. No `pending` counter. No `strand_scheduled`. The dispatch thread's existence IS the "component is alive" signal; its termination IS the quiescence signal.

`join_timeout` is not in std; we use a `parking_lot::Condvar` or a `flume` channel for the join-with-timeout, or implement it as `thread::park_timeout` + an `AtomicBool` the thread sets before exit. The PR will pick the shape.

### 5. Replace becomes splice

`handle_replace` spins up a new dispatch thread with the new `Component`, then:

1. Store new `Sender` in a local; don't publish to the table yet.
2. Swap the table entry's `Sender` under the table write lock.
3. Drop the *old* `Sender`. The old dispatch thread's `recv()` returns `None` after it finishes any currently-dispatched mail + any mail already in its inbox.
4. Call `entry.handle.join()` on the old handle to get back the old `Component`, fire `on_replace` / `on_drop`, serialize state (ADR-0016 `save_state`), `on_rehydrate` into the new component.

ADR-0022's drain semantics are preserved by construction: mail already in the old channel gets delivered before `recv()` returns `None`. Mail sent after the `Sender` swap goes to the new inbox. No `frozen` flag, no parked deque, no drain loop.

State migration (ADR-0016) shifts timing: `save_state` now runs on the old dispatch thread after it drains, then the snapshot crosses back to the control thread, then into the new dispatch thread for `on_rehydrate`. Serialization cost is unchanged; the only visible change is that `on_replace` fires *after* the old instance has seen every pre-replace mail — which is the invariant ADR-0022 was reaching for anyway.

### 6. Backpressure

Bounded channels give us an explicit knob. Default capacity TBD — probably 1024 per component, empirically tuned. When a component's inbox is full:

- Host `send_mail` host-fn: blocks the sending guest's worker thread. Since each component has its own dispatch thread, a sender component is literally itself — so the caller backpressures naturally.
- Platform input fan-out (ADR-0021 tick/key/mouse): blocks the platform thread. That's the same thread that drives winit's event loop on desktop; blocking it stalls rendering. We instead `try_send` and drop-with-warn on a full input inbox, matching the "input is lossy under extreme load" invariant desktop already has elsewhere.
- Hub-delivered mail (control plane, remote sends): blocks the hub-session reader task. That's already how the hub handles slow components today.

For the first pass we ship unbounded channels to avoid changing observable behavior, flag the backpressure design above as follow-up, and revisit when we have real data on full-inbox conditions.

### 7. Rollout — three phases

- **Phase 1: scaffolding.** Add `ComponentEntry::sender` + per-component thread alongside the existing worker pool. Workers keep dispatching; the new thread is dormant. Ensures the chassis bootstrap understands the new shape.
- **Phase 2: cut over dispatch.** Workers stop claiming strands; all component-bound mail flows through the per-component channel. Drop/replace switch to the channel-close / splice paths above. `pending`, `frozen`, `strand_scheduled`, `parked`, and the worker pool retire in this phase.
- **Phase 3: retire the shared queue.** `MailQueue`, `wait_idle`, and the outstanding counter go away. Chassis that needed frame-scale drains (capture_frame pre-dispatch bundle in particular) adopt per-mailbox or per-subscriber-set drains.

Each phase ships as its own PR with its own tests. Phase 1 is pure addition; Phase 2 flips behavior; Phase 3 is cleanup.

## Consequences

### Positive

- **Structural shutdown.** "Component is alive" = "dispatch thread is alive." Drop = close channel + join. No coordination atomics, no `Arc::into_inner` traps. The entire class of bugs behind PR #166 and the mac FIFO flake cannot be expressed in the new shape.
- **Per-mailbox FIFO is free.** One consumer per inbox; deliver order is channel order is send order. The strand-claim mechanism retires.
- **Replace becomes a splice.** Swap one `Sender` for another. Old inbox drains on its own, new inbox starts clean. ADR-0022's drain-timeout shape survives; the bookkeeping doesn't.
- **Three invariants collapse to one primitive.** `pending` / `frozen` / `strand_scheduled` all retire. The remaining primitive (channel + thread) is std-library idiom and well-understood.
- **Backpressure becomes explicit.** Bounded channels give an obvious knob; full-inbox behavior is a per-sender-kind policy instead of emergent from queue-contention dynamics.
- **Test surface shrinks.** Concurrency tests around strand claim, drain semantics, and FIFO order mostly become "the channel does this." A flaky scheduler test is a flaky standard library, not a flaky Aether primitive.

### Negative

- **Scheduler code retires, not adjusts.** `scheduler.rs` (~300 lines) is mostly deleted in Phase 2; this is a bigger blast radius than the freeze-drain or strand-claim landings. Mitigated by phased rollout.
- **Thread-per-component has a memory floor.** ~1–2 MB/thread stack. Negligible for tens of components; real at thousands. Revisit threshold is ~100 live components; past that, ADR-0038-phase-4 (tokio tasks) if we get there.
- **`wait_idle` becomes per-mailbox, not global.** Chassis code that wanted "wait for the whole engine to settle" (capture_frame does) now iterates every live mailbox or asks a well-known subscriber set. Net complexity ~neutral, but explicit.
- **State migration timing shifts.** `save_state` now runs after the old instance has drained. No semantic change but any ADR-0016 test that inspected intermediate state during replace will need updating.
- **Shipping the initial iteration with unbounded channels means we retain "slow component stalls senders" behavior indirectly via memory growth.** Same as today, just located differently. Bounded-channel follow-up tracks the eventual fix.

### Neutral

- **Guest SDK unaffected.** Host-fn surface (ADR-0024 `_p32`, ADR-0029 mailbox ids, ADR-0030 kind ids) doesn't change. Components keep emitting `send_mail` and handlers keep receiving `receive_p32`. The dispatch shape is a substrate-internal concern.
- **MCP harness unaffected.** `send_mail`, `receive_mail`, `capture_frame`, etc. keep working; `engine_logs` still captures tracing events unchanged.
- **ADR-0022 (drain-on-swap) is subsumed.** The drain invariant survives in a different mechanism. ADR-0022's `frozen` / `pending` primitives retire but the guarantee they provided stays.

## Alternatives considered

- **Status quo + tactical fixes per bug.** PR #166 (poll strong_count for drop; eventually shipped as "mutex lock is enough"). Fixes the specific panic without touching the shape. Rejected as long-term strategy because each new lifecycle primitive adds another place to remember these invariants; we've paid this cost twice (drop, replace) and ADR-0037 (mail-bubbles-up) will probably require another round.
- **`RwLock<Option<Component>>` per entry.** Worker takes read guard during dispatch, dropper takes write to `take()`. Structurally sound. Rejected: reader-writer lock held across `deliver` blocks lifecycle ops for the full guest execution time (milliseconds-to-seconds for non-trivial components). Same ergonomic regression as running replace synchronously on the work thread.
- **`DashMap` with per-shard locks.** Same hot-path cost as RwLock plus shard-level contention between unrelated mailboxes that hash-collide. Rejected.
- **Weak<ComponentEntry> in worker strand.** Workers upgrade to Arc only for the `deliver` call. Refcount-based reclamation still; just less of it leaked. Rejected: doesn't fix the core issue (a worker mid-upgrade still holds a transient strong), moves complexity without removing it.
- **Crossbeam-epoch or hazard-pointer based reclamation.** Branch-free in the hot path. Rejected: adds a library dependency for a problem that goes away entirely with actors, and epoch-based reclamation has its own subtle-bug surface.
- **Actor-per-component via async tasks (tokio) instead of OS threads.** Lower per-unit overhead. Rejected for Phase 2; wasmtime calls are blocking and the required `spawn_blocking` wrapping complicates the task lifecycle. Revisit as a Phase 4 if OS-thread memory becomes the binding constraint.

## Follow-up work

- **Phase 1**: add `ComponentEntry::sender` + per-component thread scaffolding. Existing dispatch stays authoritative. Tests cover the new plumbing but don't exercise it.
- **Phase 2**: cut dispatch over; retire `pending` / `frozen` / `strand_scheduled` / `parked` / strand drain loop / `MailQueue::outstanding` / worker pool. ADR-0022's drain timeout knob migrates to the channel-close timeout.
- **Phase 3**: retire the shared queue entirely; chassis adopts per-mailbox drains where needed.
- **Parked, not committed**: bounded channels + per-sender-kind full-inbox policy (guest send blocks, platform input drops-with-warn, hub send blocks sender task).
- **Parked, not committed**: tokio task-per-component as Phase 4 if thread-per-component memory becomes a constraint past ~100 live components.

## References

- ADR-0004 — concurrent scheduler spike. The shape this ADR retires.
- ADR-0010 — runtime component loading. Drop / replace primitives this ADR reshapes.
- ADR-0015 — lifecycle hooks. `on_replace` / `on_drop` timing preserved.
- ADR-0016 — persistent state across hot reload. `save_state` / `on_rehydrate` timing slightly shifts; semantics preserved.
- ADR-0021 — input stream subscriptions. Fan-out path changes from shared-queue push to per-mailbox channel send.
- ADR-0022 — drain-on-swap. Drain invariant preserved in a different mechanism; this ADR supersedes the `pending` / `frozen` / `parked` machinery.
- Issue 157 — strand-claim fix (unrecorded). Motivated the `strand_scheduled` flag; retired by this ADR.
- PR #166 (2026-04-21) — drop_component Arc-uniqueness panic fix. Motivating incident.
- `project_mac_ci_flake_watch.md` — macOS runner FIFO flake on 2026-04-21. Second motivating incident.
