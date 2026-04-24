# ADR-0042: Synchronous mail wait primitive

- **Status:** Proposed
- **Date:** 2026-04-23
- **Amended:** 2026-04-24 — retired the filter-slot + oneshot mechanism in §1 / §4 / §6 in favour of a drain-and-buffer loop over the component's mpsc inbox.
- **Amended:** 2026-04-24 — added per-component correlation ids on `ReplyTo` + `prev_correlation_p32` host fn, so the drain loop can filter out stale replies of the same kind. See _Amendment history_ below.

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

### 1. Host fns: `wait_reply_p32` + `prev_correlation_p32`

Two new imports on the `aether` module, siblings to `send_mail_p32` / `reply_mail_p32`:

```rust
// Guest signatures (Rust-side in aether-component::raw)
pub fn wait_reply_p32(
    expected_kind: u64,             // K::ID to wait for
    out_ptr: u32,                   // component-memory buffer to write the reply payload into
    out_cap: u32,                   // buffer capacity
    timeout_ms: u32,                // 0 = poll (try_recv only); max clamped below
    expected_correlation: u64,      // 0 = kind-only match; nonzero = filter on reply_to.correlation_id
) -> i32;

pub fn prev_correlation_p32() -> u64;
```

Return value encoding for `wait_reply_p32`:

- `>= 0` — bytes written to `out_ptr`. Reply decoded against `expected_kind`.
- `-1` — timeout elapsed with no matching reply.
- `-2` — reply matched but the payload exceeded `out_cap` (bytes were dropped; caller retries with a larger buffer).
- `-3` — substrate tore the component down while it was waiting (drop/replace during a wait); the guest treats this as "abort whatever you were doing."

`prev_correlation_p32` returns the correlation id the substrate auto-minted for this component's most recent `send_mail`. `0` before any send. Sync wrappers call it immediately after a send to capture the id, then pass it as `expected_correlation` on the matching `wait_reply_p32` — `(kind, correlation)` uniquely picks *our* reply out of the inbox rather than whatever `kind`-matching mail happens to be queued.

Substrate side (**drain + buffer + correlation**): the component's mpsc Receiver moves onto `SubstrateCtx` so the host fn can drive it directly. `wait_reply_p32` loops:

1. Pop a mail from the inbox mpsc (`recv_timeout(remaining)` or `try_recv` when `timeout_ms == 0`).
2. Match: `m.kind == expected_kind && (expected_correlation == 0 || m.reply_to.correlation_id == expected_correlation)`. On match, decode into the guest buffer and return the byte count.
3. Otherwise, push the mail onto a per-component FIFO **overflow buffer** and loop again.
4. On timeout, return `-1`. Buffered non-matching mail stays in overflow — the dispatcher drains it before pulling anything new from the mpsc on its next iteration.

This means **no re-entrant deliver calls**. The component is one thread; that thread is parked inside the host fn's drain loop; non-match mail accumulates in the overflow buffer while the wait is live. When the wait returns (match, timeout, or disconnect), the dispatcher loop resumes, drains the overflow first (so FIFO order is preserved across the wait), and only then pulls new mail from the mpsc.

Nothing new is needed on the send side from the guest's perspective: `raw::send_mail` keeps its 5-arg signature. What *did* change under the hood: `SubstrateCtx::send` now mints a fresh `correlation_id` on every call via a per-component `Cell<u64>` counter (single-threaded per ADR-0038, so no atomic), attaches it to the outgoing `ReplyTo`, and stores the value where `prev_correlation_p32` can read it. `Mailer::send_reply` auto-echoes the incoming correlation on the reply envelope. Sink authors never touch correlation — the mailer does it. A reply to a sink-bound request arrives in the same mpsc that carries every other piece of mail; the host fn's drain loop picks it up by matching the stashed correlation. That removes both the ordering trap an earlier version of this ADR introduced with a separate filter slot *and* the stale-reply trap where a sync call could consume a prior async reply of the same kind.

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

While a component is parked in `wait_reply_p32`, every mail pushed at its mpsc gets consumed by the drain loop. Matches become the return value; non-matches land in the overflow buffer and feed the dispatcher ahead of the mpsc after the wait returns. Specifically:

- **Tick mail** lands in overflow during a wait. A 500ms sync wait at 60Hz buffers ~30 Ticks; the component drains them one-by-one from overflow after unparking. If the component cares about tick freshness it coalesces in its handler (`if self.last_tick_frame == tick.frame { return; }`). v1 does not add a substrate-side "transient kind" flag — components that need coalescing implement it themselves.
- **Input mail** (key, mouse) buffers in FIFO order in overflow. Losing input during I/O is worse than replaying stale events; ordering is load-bearing, which is why overflow is a FIFO that dispatcher consults ahead of the mpsc.
- **Replies addressed at the same component but not matching `expected_kind`** go to overflow like any other non-match. They dispatch through `deliver` when the wait returns.

**Only one sync wait in flight per component.** The component is single-threaded (ADR-0038); a second wait would require a second thread making a host call, which can't happen. Re-entrant calls from the same thread (a handler invoked during overflow drain that itself calls `wait_reply_p32`) are theoretically legal but out of scope — the drain loop is serial; nested waits compose without deadlock as long as the inner wait's match actually exists in mpsc or arrives during its own timeout window.

### 5. Interaction with `replace_component`

ADR-0022's freeze-drain-swap: the substrate freezes the target mailbox, waits for in-flight `deliver` calls to complete, then swaps. A sync wait is "in flight" — the thread is parked inside a host fn called from inside `deliver`. The drain blocks until the wait returns (reply arrives, times out, or the substrate cancels — see next paragraph).

**Drop/replace cancellation.** When the substrate tears down the component, the parked wait needs to unpark promptly rather than hang the drain out to the timeout. `splice_inbox` / `close_and_join` already drop the old mpsc `Sender`. The host fn's `recv_timeout` on the `Receiver` wakes with `RecvTimeoutError::Disconnected`; the host fn returns `-3`. The guest sees "your wait was cancelled, abort." SDK wrappers propagate this as `SyncIoError::Cancelled`. No separate signaling channel is needed — the mpsc's existing disconnect semantics carry the teardown notification.

The overflow buffer is dropped with the `SubstrateCtx` at teardown, so any mail that accumulated during the aborted wait goes away with the instance. That's the right behavior: the new instance (under `replace_component`) starts fresh; the old instance isn't going to get a chance to drain.

### 6. Parking implementation

The host fn borrows the `Receiver<Mail>` out of `SubstrateCtx` through a `Mutex` (required because `std::sync::mpsc::Receiver` isn't `Sync`; the lock is uncontended because the component is single-threaded). It calls `recv_timeout(remaining)` in a loop, where `remaining` is re-computed from a deadline so non-matching mail consumes the same budget.

On `timeout_ms = 0`, the guest uses `try_recv` instead of `recv_timeout` — semantically "drain whatever's already queued, match if anything fits, else return." Useful for checking-without-blocking patterns.

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
- **Only one sync-wait in flight per component.** The component is single-threaded, so this is structural rather than enforced. A second wait would need a second thread making host calls.
- **Raw host fn is a footgun for non-substrate-sink callers.** A component author can call `raw::wait_reply_p32` against a reply kind another component sends — and if that component also sync-waits on this one, both threads deadlock. SDK guidance is the mitigation; the raw fn stays available because locking it down would also block legitimate uses (a component built against known-non-circular sibling sinks).
- **New wasm import name.** `wait_reply_p32` joins `send_mail_p32` / `reply_mail_p32` / `save_state_p32` on the `aether` module — one more thing every component build has to link against. Marginal, but worth naming.
- **Overflow buffer grows with traffic during the wait.** A 1s wait on a component receiving 10k mails/s builds a 10k-mail buffer in `VecDeque`. Each entry is a `Mail` struct (payload Vec plus a few u64s), so tens of MB on extreme traffic. Acceptable — components that need to survive storms either shorten their waits or use the async path; no backpressure mechanism is added in v1.

### Neutral

- **Wire unchanged.** No new kind, no schema change, no sink registration. The substrate internally moves the per-component `Receiver<Mail>` into `SubstrateCtx` so the host fn can drain it, and adds a `VecDeque<Mail>` overflow buffer; the FFI grows by one import; mail bytes on the wire look identical to today.
- **Existing SDK surface preserved.** `io::read` / `io::write` / `Ctx::send` / `Sink::send` all keep their current semantics. Sync variants are additive.
- **`_p32` suffix.** ADR-0024's wasm32/wasm64 naming convention applies: `wait_reply_p32` because `out_ptr` is pointer-typed. Non-pointer args (kind, cap, timeout) don't contribute to the suffix.

## Alternatives considered

- **Do nothing.** Keep the async/handler-only shape and let components build their own state machines. Rejected: real multi-step I/O is painful enough that a sufficiently motivated component author writes their own parking scheme on top of `wait_reply_p32`-equivalents (bolting on manually-generated request ids, in-memory reply tables). Better to provide the primitive once, correctly.
- **Fibers / stackful coroutines in the guest.** wasmtime supports stack switching via `typed-funcref`. Could park a wasm call stack as first-class state. Rejected — substantial runtime complexity (shadow stacks, stack-copy semantics, debugger interaction), and wasmtime's stable API for this is not yet where it needs to be for us to bet on it. Revisit if the single-in-flight restriction becomes a real ceiling.
- **Async/await on the guest side.** Expose an async runtime inside the component, let handlers be `async fn`, desugar `read_sync(...).await` into a state machine via the compiler. Rejected: drags an async runtime into every component (dep graph cost, binary size), and the ergonomic win over this ADR is mostly just the `.await` suffix. The scheduler we already have (actor-per-component) is a far simpler foundation than async tasks.
- **Per-kind sync-wait only.** A different host fn per reply kind (e.g. `wait_read_result_p32`). Rejected as gratuitous surface area — the one `wait_reply_p32` with a kind filter argument covers every substrate sink without wire-specific code in the substrate.
- **Global sync-wait ceiling instead of per-call.** Every wait uses the substrate's clamp default, no caller `timeout_ms` arg. Rejected — callers have real information about how long their I/O should take (a config file read is 10ms; a cloud fetch is 10s), and one ceiling doesn't cover both without wasting every caller's time in the short-timeout case.
- **Allow arbitrary component-to-component sync-wait in the SDK.** Rejected for deadlock surface. Pairs of components can recover the pattern via substrate-owned sinks as intermediaries (component A mails a sink; sink handler synthesizes a reply from B's observations; A sync-waits on the sink), which keeps the cycle-free invariant the scope restriction encodes.
- **Filter slot + oneshot channel (retired in the 2026-04-24 amendment).** The original §1 of this ADR proposed a per-component filter slot (`FilterSlot`): `wait_reply_p32` would install a sender keyed on `expected_kind` onto the slot, `ComponentEntry::send` would consult the slot at send-time and hand matching mail to a oneshot channel instead of the mpsc, non-matches would queue in the mpsc as normal. Shipped as PR #218 / PR #219 then retired when the SDK ran into an ordering trap: the io sink dispatches synchronously on the same thread as the guest's `send_mail`, so a request's reply lands in the mpsc *before* `send_mail` returns — and the filter slot hasn't been installed yet. Splitting install and wait into two host fns would work but expanded the surface; draining the mpsc from the host fn is both simpler (no separate oneshot, no install-before-send ordering) and robust to that timing (the reply is already sitting where the host fn reads from by the time the drain loop starts).

## Follow-up work

- **PR (shipped 2026-04-23, superseded 2026-04-24 amendment)**: substrate runtime — per-component `FilterSlot` + oneshot, dispatch-path diversion. Retired with the amendment; see _Amendment history_.
- **PR (shipped 2026-04-23, rewritten by the 2026-04-24 amendment)**: `wait_reply_p32` on the `aether` module. Now backed by the drain+buffer implementation.
- **PR**: refactor — move `Receiver<Mail>` into `SubstrateCtx`, add the overflow `VecDeque`, rewrite `wait_reply_p32` as a drain loop, retire `FilterSlot`.
- **PR**: guest SDK — `aether-component::raw::wait_reply_p32`, plus typed wrappers for each substrate-owned sink: `io::{read,write,delete,list}_sync`, `audio::set_master_gain_sync`, etc.
- **Parked, not committed**: `IS_TRANSIENT` kind flag that lets the substrate coalesce overflow during a sync wait. Pulled in if the tick-replay pattern actually hurts a real component.
- **Parked, not committed**: `wait_any` primitive for fanning out N requests and returning on the first reply. The drain loop shape generalizes (set of expected kinds, return the first match), but no component has asked for it yet.
- **Parked, not committed**: lift the "only substrate-owned sinks" SDK scope once ADR-0043-ish work (cached byte handles, or more generally a clear dependency graph for non-circular component sinks) gives us a way to prove a sink can't cycle.

## Amendment history

### 2026-04-24 — drain+buffer replaces filter slot + oneshot

**What changed.** §1 rewritten. §4 rewritten. §5's cancellation paragraph simplified (uses existing mpsc `Sender` disconnect; no separate signaling channel). §6 rewritten. Consequences (Neutral) updated. A new Negative-consequences bullet added for overflow buffer growth. A new Alternatives bullet captures the retired filter-slot design and the reason for the switch.

**Why.** The filter-slot design's ordering invariant — filter must be installed before the mail it's meant to match arrives — didn't survive contact with ADR-0041's synchronous sink dispatch. A component calling `io::read` via `send_mail` dispatches the io sink inline on the dispatcher thread; the sink replies before `send_mail` returns, landing the reply in the mpsc. Any subsequent `wait_reply_p32` that both installs the filter and waits is too late — the match is already past. The clean fixes were (a) split install and wait into two host fns, or (b) drain the mpsc from the host fn. (b) is simpler and has no ordering trap: the host fn reads exactly where mail lives, so there's no "install before the race starts" step to get wrong.

**Backward compatibility.** No guest SDK had shipped a `wait_reply` caller yet; no components were using the FFI. The retired `FilterSlot` type, `ComponentEntry.filter_slot` field, and `SubstrateCtx::with_filter_slot` builder are removed. `wait_reply_p32`'s wasm import name and return encoding are unchanged — guest code compiled against its signature still links correctly, only the body changed.

### 2026-04-24 — per-component correlation ids on ReplyTo

**What changed.** `ReplyTo` refactored from an enum to `struct ReplyTo { target: ReplyTarget, correlation_id: u64 }`. Every reply-bearing wire shape grows a `correlation_id: u64`: `EngineMailFrame`, `MailFrame`, `MailToEngineMailboxFrame`, `EngineMailToHubSubstrateFrame`, `MailByIdFrame`. Substrate-side, `SubstrateCtx` gains a `Cell<u64>` per-component counter minted on every `send` and auto-echoed on replies by `Mailer::send_reply`. `wait_reply_p32` grows a 5th arg `expected_correlation: u64` (0 = kind-only). New host fn `prev_correlation_p32() -> u64` reads the last-minted id.

**Why.** The original drain+buffer design matched incoming mail by kind only. A component that fires an async `io::read` and *then* calls `io::read_sync` (of the same kind) would have the sync wait consume the prior async reply — silent cross-wiring. No queue-draining scheme fixes this in general: a prior async reply can land during the wait, and a kind-only match can't distinguish it from "ours." Correlating each request to its own reply fixes it cleanly. Making the substrate mint correlations automatically (rather than pushing the responsibility to the SDK) keeps the Rust-level `SubstrateCtx::send` signature unchanged — guests opt into filtering by calling `prev_correlation` + `wait_reply` with the returned id.

**Per-component vs global.** Counter lives on `SubstrateCtx`, which is rebuilt per instance. Two components' correlation ids don't collide by accident (mail routes to separate inboxes) but more importantly the id-space is clean — each component starts at 1. On `replace_component`, the counter resets because the `SubstrateCtx` is rebuilt; in-flight correlations belong to the old instance, not the mailbox.

**Backward compatibility.** Wire-level: all `correlation_id` fields derive `#[serde(default)]` so deserializing a pre-amendment frame yields `0` (the "no correlation" sentinel), matching kind-only behavior. `ReplyTo` API: enum variant matches become struct-with-target matches; a dozen call sites swept in the same PR. `ReplyEntry` (in the reply table) likewise grew a `correlation_id` field so `reply_mail_p32` echoes it on the outgoing reply. No components had shipped against the pre-correlation SDK; this amendment lands alongside the `*_sync` wrappers that first use it.
