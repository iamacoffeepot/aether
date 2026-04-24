# ADR-0042: Synchronous mail wait primitive

- **Status:** Proposed
- **Date:** 2026-04-23

## Context

ADR-0041 shipped the substrate's I/O sink with mail-based request/reply semantics: a component sends `aether.io.read`, the substrate dispatches through an adapter, reply arrives asynchronously as `aether.io.read_result` on the component's mailbox. ADR-0016 `DropCtx::reply` and ADR-0013 reply-to-sender work the same way — anything request/response in aether today rides this split-handler shape.

For a one-shot read (boot, load a save, proceed), this is fine. The painful case is **multi-step I/O**: read A, parse, read B derived from A's contents, write C based on both. Today the component has to shred its logic across handler boundaries:

```rust
enum LoadPhase { ReadingA, ReadingB(ParsedA), WritingC }

#[handler]
fn on_tick(&mut self, ctx: &mut Ctx<'_>, _: Tick) {
    if self.phase == LoadPhase::Idle {
        io::read("save", "slot1.a");
        self.phase = LoadPhase::ReadingA;
    }
}

#[handler]
fn on_read_result(&mut self, _: &mut Ctx<'_>, r: ReadResult) {
    match (&self.phase, r) {
        (LoadPhase::ReadingA, ReadResult::Ok { bytes, .. }) => {
            let parsed = parse(&bytes);
            io::read("save", &parsed.next_file);
            self.phase = LoadPhase::ReadingB(parsed);
        }
        (LoadPhase::ReadingB(parsed_a), ReadResult::Ok { bytes, .. }) => {
            let combined = combine(parsed_a, &bytes);
            io::write("save", "c.bin", &combined);
            self.phase = LoadPhase::WritingC;
        }
        // ... more arms
    }
}
```

Every logical step becomes a state-machine arm. Real load flows have half a dozen steps. Control flow that should read as a function body reads as a finite-state machine.

The actor-per-component scheduler (ADR-0038) made the alternative cheap. Each component already owns one OS thread, fed by an mpsc that serializes inbound mail. Parking that thread on a oneshot channel until a reply arrives — "send, then block, then return the bytes" — costs no shared-pool contention, no async runtime, no coloring. The cost model is different enough from the worker-pool era that reopening this decision now is worth doing.

This ADR commits to a synchronous-mail-wait primitive in the host FFI and the guest SDK, narrowly scoped to keep it from being the tool component authors reach for every time.

## Decision

### 1. Host fn: `wait_reply_p32`

One new import on the `aether` module, a sibling to `send_mail_p32` / `reply_mail_p32`:

```rust
// Guest signature (Rust-side in aether-component::raw)
pub fn wait_reply_p32(
    expected_kind: u64,    // K::ID to wait for
    out_ptr: u32,          // component-memory buffer to write the reply payload into
    out_cap: u32,          // buffer capacity
    timeout_ms: u32,       // 0 = return immediately if nothing matches; max clamped below
) -> i32;
```

Return value encoding:

- `>= 0` — bytes written to `out_ptr`. Reply decoded against `expected_kind`.
- `-1` — timeout elapsed with no matching reply.
- `-2` — reply matched but the payload exceeded `out_cap` (bytes were dropped; caller retries with a larger buffer).
- `-3` — substrate tore the component down while it was waiting (drop/replace during a wait); the guest treats this as "abort whatever you were doing."

Substrate side: the component's inbound mpsc grows a **filter slot** that's populated when the host fn is entered and cleared when it returns. While set, the substrate's normal deliver path checks each incoming mail's kind against the filter; matches are handed to a oneshot channel the blocked thread is reading; non-matches stay in the mpsc and get dispatched normally once the filter clears. The guest thread parks on `crossbeam_channel::recv_timeout` on the oneshot.

This means **no re-entrant deliver calls**. The component is one thread; that thread is parked; other mail queues up. When the wait returns, the queue drains through the normal `__aether_dispatch` path as if nothing happened.

### 2. SDK surface: scoped to substrate sinks

The raw `wait_reply_p32` host fn is available to any caller that wants it, but the SDK only wraps it for **substrate-owned sinks** (ADR-0041 `io`, ADR-0039 `audio`, ADR-0008 `render`, ADR-0033 `camera`):

```rust
// aether-component::io — new sync counterparts, existing fire-and-forget
// helpers stay.
pub fn read_sync(
    namespace: &str,
    path: &str,
    timeout_ms: u32,
) -> Result<Vec<u8>, SyncIoError>;

pub fn write_sync(
    namespace: &str,
    path: &str,
    bytes: &[u8],
    timeout_ms: u32,
) -> Result<(), SyncIoError>;

pub fn delete_sync(namespace: &str, path: &str, timeout_ms: u32) -> Result<(), SyncIoError>;
pub fn list_sync(namespace: &str, prefix: &str, timeout_ms: u32) -> Result<Vec<String>, SyncIoError>;
```

Equivalent helpers can ride on `aether.audio.set_master_gain`, on `capture_frame` (desktop control), etc. — any substrate-owned sink that already has a reply-required shape.

**Arbitrary component-to-component sync-wait is not exposed via the SDK.** A component could still invoke `raw::wait_reply_p32` directly, but doing so risks a deadlock: if component A waits on a reply from component B, and B also waits on A, both OS threads park forever. Substrate-owned sinks are immune — the substrate never synchronously waits on a component, so A waiting on the `io` sink can never cycle back.

### 3. Timeout required

Every SDK wrapper takes a `timeout_ms: u32`. The substrate clamps `timeout_ms` to a hard ceiling (default 30000, matching `capture_frame`'s ceiling) so a bug on either side can't produce an immortal parked thread. `-1` return on timeout gives the caller an explicit error path rather than a hang.

Callers that genuinely want "wait forever" pass the clamp value; they've made the choice loud.

### 4. Mail backlog during wait

While a component is parked in `wait_reply_p32`, other mail pushed at its mpsc **stays in the queue** and drains in FIFO order through the normal `__aether_dispatch` path once the wait returns. Specifically:

- **Tick mail** accumulates. A 500ms sync wait at 60Hz queues 30 Ticks. The component drains them one-by-one when it unparks. If the component cares about tick freshness it coalesces in its handler (`if self.last_tick_frame == tick.frame { return; }`). v1 does not add a substrate-side "transient kind" flag — components that need coalescing implement it themselves.
- **Input mail** (key, mouse) accumulates in FIFO. Losing input during I/O is worse than replaying stale ticks; ordering is load-bearing.
- **Replies addressed at the same component but not matching the filter** queue behind the waiter. If the component has two concurrent sync waits pending (which shouldn't happen — see below), only the first matches its filter; the second sits in the mpsc.

**Only one sync wait in flight per component.** The filter slot is a single slot, not a set. Calling `wait_reply_p32` while already waiting is undefined behavior on the host side — the SDK wrappers are synchronous function calls so this can't happen by accident; a component author who builds a nested wait pattern gets what they deserve.

### 5. Interaction with `replace_component`

ADR-0022's freeze-drain-swap: the substrate freezes the target mailbox, waits for in-flight `deliver` calls to complete, then swaps. A sync wait is "in flight" — the thread is parked inside a host fn called from inside `deliver`. The drain blocks until the wait returns (reply arrives, times out, or the substrate cancels — see next paragraph). That's the existing drain semantics applied correctly, not new behavior.

**Drop/replace cancellation.** When the substrate tears down the component (drop or replace), the parked wait needs to unpark promptly rather than hang the drain out to the timeout. The substrate closes the oneshot channel's sender on teardown; `recv_timeout` wakes with a disconnect error; the host fn returns `-3`. The guest sees "your wait was cancelled, abort." SDK wrappers propagate this as `SyncIoError::Cancelled`.

### 6. Parking implementation

One `crossbeam_channel::bounded(1)` oneshot per filter slot, allocated when the host fn is entered and dropped when it returns. The substrate's deliver path, seeing the filter slot populated, does a `try_send` on the oneshot for matching mail; the guest thread's `recv_timeout` wakes. Non-blocking for the deliver caller (important — deliver runs on the scheduler's dispatch thread, not the component's).

On `timeout_ms = 0`, the guest uses `try_recv` instead of `recv_timeout` — semantically "poll once and return." Useful for checking-without-blocking patterns.

## Consequences

### Positive

- **Control flow reads as control flow.** The multi-step load example collapses from a state-machine enum + two handlers to a linear function body with explicit error propagation. Easier to write, easier to review, easier to debug.
- **Zero shared-thread contention.** Each component already owns one OS thread (ADR-0038). Parking it on a oneshot is cheap — no async runtime, no worker-pool starvation, no scheduler-level coordination.
- **Composable with existing async path.** The async/handler-based path stays. A component that genuinely does want to process other mail during a long read uses `io::read` + `#[handler] fn on_read_result`; a component that wants to block until the bytes are in hand uses `io::read_sync`. Author picks per call.
- **Deadlock surface is closed by scope.** Substrate-owned sinks are non-circular — they don't wait on components, so a component waiting on a sink can't produce a cycle. Components that want arbitrary sync-wait pay the deadlock-risk cost by reaching for raw `wait_reply_p32` themselves.
- **Cancellation is explicit.** Drop/replace wakes the waiter via a channel disconnect; the `-3` return code gives the guest a chance to clean up rather than being silently torn down mid-syscall.

### Negative

- **Component is single-tracked during the wait.** While parked in `read_sync`, the component can't process `Tick` or input. For an asset-loader this is fine (that's what it's doing); for a component that also needs to render during a read, use the handler-based path. Both stay available — this ADR doesn't retire the async path.
- **Tick backlog drains all at once.** After a 500ms wait, the component may see 30 `Tick`s in quick succession. Components that care about tick freshness need to coalesce themselves. Acceptable v1 policy; a substrate-side `IS_TRANSIENT` kind flag is the natural follow-up if this pattern hurts in practice.
- **Only one sync-wait in flight per component.** Filter slot is single-valued. Components that want to fan out N reads and wait for the first to complete need the async path (or the future `wait_any` primitive, not in this ADR).
- **Raw host fn is a footgun for non-substrate-sink callers.** A component author can call `raw::wait_reply_p32` against a reply kind another component sends — and if that component also sync-waits on this one, both threads deadlock. SDK guidance is the mitigation; the raw fn stays available because locking it down would also block legitimate uses (a component built against known-non-circular sibling sinks).
- **New wasm import name.** `wait_reply_p32` joins `send_mail_p32` / `reply_mail_p32` / `save_state_p32` on the `aether` module — one more thing every component build has to link against. Marginal, but worth naming.

### Neutral

- **Wire unchanged.** No new kind, no schema change, no sink registration. The substrate internally adds a filter slot to each component's dispatch state; the FFI grows by one import; mail bytes on the wire look identical to today.
- **Existing SDK surface preserved.** `io::read` / `io::write` / `Ctx::send` / `Sink::send` all keep their current semantics. Sync variants are additive.
- **`_p32` suffix.** ADR-0024's wasm32/wasm64 naming convention applies: `wait_reply_p32` because `out_ptr` is pointer-typed. Non-pointer args (kind, cap, timeout) don't contribute to the suffix.

## Alternatives considered

- **Do nothing.** Keep the async/handler-only shape and let components build their own state machines. Rejected: real multi-step I/O is painful enough that a sufficiently motivated component author writes their own parking scheme on top of `wait_reply_p32`-equivalents (bolting on manually-generated request ids, in-memory reply tables). Better to provide the primitive once, correctly.
- **Fibers / stackful coroutines in the guest.** wasmtime supports stack switching via `typed-funcref`. Could park a wasm call stack as first-class state. Rejected — substantial runtime complexity (shadow stacks, stack-copy semantics, debugger interaction), and wasmtime's stable API for this is not yet where it needs to be for us to bet on it. Revisit if the single-in-flight restriction becomes a real ceiling.
- **Async/await on the guest side.** Expose an async runtime inside the component, let handlers be `async fn`, desugar `read_sync(...).await` into a state machine via the compiler. Rejected: drags an async runtime into every component (dep graph cost, binary size), and the ergonomic win over this ADR is mostly just the `.await` suffix. The scheduler we already have (actor-per-component) is a far simpler foundation than async tasks.
- **Per-kind sync-wait only.** A different host fn per reply kind (e.g. `wait_read_result_p32`). Rejected as gratuitous surface area — the one `wait_reply_p32` with a kind filter argument covers every substrate sink without wire-specific code in the substrate.
- **Global sync-wait ceiling instead of per-call.** Every wait uses the substrate's clamp default, no caller `timeout_ms` arg. Rejected — callers have real information about how long their I/O should take (a config file read is 10ms; a cloud fetch is 10s), and one ceiling doesn't cover both without wasting every caller's time in the short-timeout case.
- **Allow arbitrary component-to-component sync-wait in the SDK.** Rejected for deadlock surface. Pairs of components can recover the pattern via substrate-owned sinks as intermediaries (component A mails a sink; sink handler synthesizes a reply from B's observations; A sync-waits on the sink), which keeps the cycle-free invariant the scope restriction encodes.

## Follow-up work

- **PR**: substrate runtime — add the per-component filter slot + oneshot, thread it into the dispatch path so matching mail lands on the oneshot and everything else queues normally.
- **PR**: host-fn import — add `wait_reply_p32` to the `aether` module in `aether-substrate-core::host_fns`, unit-test the filter/oneshot mechanism against a synthetic inbound.
- **PR**: guest SDK — `aether-component::raw::wait_reply_p32`, plus typed wrappers for each substrate-owned sink: `io::{read,write,delete,list}_sync`, `audio::set_master_gain_sync`, etc.
- **Parked, not committed**: `IS_TRANSIENT` kind flag that lets the substrate coalesce backlog during a sync wait. Pulled in if the 30-tick-replay pattern actually hurts a real component.
- **Parked, not committed**: `wait_any` primitive for fanning out N requests and returning on the first reply. The filter slot shape generalizes (set of expected kinds, return the first match), but no component has asked for it yet.
- **Parked, not committed**: lift the "only substrate-owned sinks" SDK scope once ADR-0043-ish work (cached byte handles, or more generally a clear dependency graph for non-circular component sinks) gives us a way to prove a sink can't cycle.
