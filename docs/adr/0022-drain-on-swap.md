# ADR-0022: Drain in-flight mail before component swap

- **Status:** Accepted
- **Date:** 2026-04-17

## Context

ADR-0010 §5 specified `replace_component` as an atomic swap that "drops in-flight mail" addressed to the recipient mailbox. The implementation that landed is more nuanced: `Scheduler::replace_component` takes the per-mailbox write lock on the components table, fires `on_replace` and `on_drop` (ADR-0015) on the old instance, instantiates the new one, and swaps under the same lock. Workers calling `Component::deliver` hold the *read* lock, so:

- Mail already being delivered to the old instance finishes (the swap waits on the read lock).
- Mail already in the queue at swap time is *not* drained — it stays in the queue and is delivered to the new instance after the swap.
- New mail that arrives during the swap window queues normally.

In practice this is "carry over" rather than "drop." ADR-0015's notes call this out as "loose but harmless for V0." It was harmless when components were stateless renderers and the only mail in flight was `aether.tick`. Two recent shifts make it less harmless:

- **ADR-0016 stateful components.** State migrates across replace via `save_state` / `on_rehydrate`. The new instance reasonably expects the snapshot to reflect "everything the old instance saw" — but mail queued before the swap and delivered after it gets processed by the new instance against rehydrated state, not against the state the old instance was building toward. The split is invisible to either instance.
- **ADR-0013 reply-to-sender.** A request-reply mail in flight at swap time can land on the new instance, which has no record of the request. The new instance either replies anyway (with potentially wrong content) or doesn't reply (the agent waits forever). Both are silent failures.

The core property that's missing is: **at the moment the new instance is instantiated, no mail addressed to the recipient is in flight from the old instance's perspective.** Either the old instance saw it (and any reply has been generated) or it hasn't arrived yet (and the new instance will handle it from a clean slate).

## Decision

`replace_component` quiesces the recipient before swapping: stop routing new mail to the recipient, wait for the in-flight mail to drain through the old instance, then swap. New mail that arrives during the freeze is enqueued and delivered to the new instance after swap.

### 1. Per-recipient pending counter

Each entry in the scheduler component table gains a `pending: AtomicU32` count of mail that has been routed to this recipient but not yet handed to `Component::deliver`. Worker dispatch increments on dequeue-for-recipient, decrements when `deliver` returns (success or trap).

This is cheap — one atomic per dispatch, no contention with the existing read/write lock.

### 2. Replace becomes freeze-drain-swap

`Scheduler::replace_component` runs in three phases:

1. **Freeze.** Mark the recipient `frozen: true` (a flag on the component table entry). The router's deliver path checks this before dispatching: frozen recipients keep new mail in the shared queue *without incrementing `pending`* — the mail isn't dispatched, just held.
2. **Drain.** Wait for `pending.load() == 0` with a bounded timeout (default 5 seconds, configurable per-replace). Workers finishing `deliver` calls naturally drive the count to zero.
3. **Swap.** Take the write lock, fire `on_replace` and `on_drop` on the old instance, instantiate the new one, swap, clear `frozen`, release the lock.

Timeout behavior: if the drain doesn't complete in time, the replace fails with `ReplaceResult::Err { reason: "drain timeout" }` and the old instance stays bound. This is the right failure mode — silently dropping in-flight mail to force a replace through is exactly the behavior we're removing.

### 3. Frozen mail flushes after swap

Mail that arrived during the freeze sits in the shared queue. Once the swap completes and `frozen` clears, the next worker dispatch picks it up and delivers it to the new instance. From the agent's perspective, replace is sequenced: every mail sent before `replace_component` either reaches the old instance or fails the replace; every mail sent after reaches the new instance.

State migration (ADR-0016) is unaffected mechanically — `save_state` / `on_rehydrate` already run between the on_replace and the new instance's first `deliver`. The new property is just that the snapshot reflects everything the old instance saw, not "everything except the queue tail."

### 4. Drop is unchanged for now

`drop_component` (ADR-0010) doesn't have a successor instance to hand mail to. Today it tears down the old instance with its queue tail intact (mail to a dropped mailbox effectively vanishes). This ADR doesn't change that — the freeze-drain pattern only makes sense when there's a new instance to deliver to. A future ADR could add freeze-drain-then-drop if "drop didn't process all my pending mail" becomes a real problem; today it isn't.

## Consequences

### Positive

- **State migration is coherent.** The snapshot the new instance rehydrates from reflects every mail the old instance saw. No invisible split between "old saw it" and "new will see it."
- **Reply-to-sender survives replace.** A request mail processed by the old instance gets its reply out (the worker holds the dispatch through `deliver`); the new instance starts with no in-flight requests it can't answer.
- **`replace_component` becomes a real synchronization point.** Agents can rely on the ordering: send → replace → send establishes a happens-before edge. Today it doesn't.
- **Failure mode is loud.** A drain timeout returns `ReplaceResult::Err`, not a silent partial replace. Agents see the failure and can retry or investigate.
- **Cheap on the hot path.** One atomic increment/decrement per dispatch. No contention with the existing locks.

### Negative

- **Replace latency grows by up to the in-flight processing time.** A recipient with a slow `deliver` (e.g. heavy frame processing) takes that long to drain. Bounded by the timeout. In practice the work tied to a single mail is small; this is a worst-case regression of milliseconds, not seconds.
- **Drain timeout is a knob with a default.** 5 seconds is a guess. If real workloads need longer routinely, the timeout becomes a tuning failure mode. Mitigated: per-replace override via the `ReplaceComponent` payload (additive field).
- **A `deliver` that hangs prevents replace forever (until timeout).** Today's behavior is the same — `replace_component` can't get the write lock either. New behavior at least surfaces the hang as a timeout instead of an unbounded wait. Net improvement.
- **Frozen-mail flush ordering is "in queue order," not "in arrival-during-freeze order" — they're the same in the SPMC queue we have today, but a future scheduler change could make them diverge.** Worth noting; not a current bug.

### Neutral

- **Drop semantics are unchanged.** `drop_component` still tears down with queue tail intact. If this becomes an issue, freeze-drain-then-drop is a follow-on.
- **Workers don't change.** The `pending` counter and `frozen` flag are in the dispatch path, not in `Component::deliver` — guests are unaffected.
- **Externally observable from the hub.** The replace freeze is invisible to agents on the happy path; on the error path, `ReplaceResult::Err { reason: "drain timeout" }` is the new failure shape.

## Alternatives considered

- **Status quo: carry-over semantics, document the invariant break.** ADR-0015's "loose but harmless" position. Rejected now that statefulness and reply-to-sender both depend on the invariant.
- **Buffer in-flight mail and replay against the new instance.** Capture every mail dispatched-but-not-yet-delivered; after swap, redeliver them to the new instance. Rejected: doubles the per-mail bookkeeping (need to retain the bytes, not just count), and the replay-after-rehydrate semantics are subtle (was the mail seen by the old instance or not?). Drain is simpler and gives a stronger invariant.
- **Drop in-flight mail (literal ADR-0010 §5 wording).** Honest about what happens, simpler than drain. Rejected: silent loss is the worst failure mode for both stateful migration and reply-to-sender.
- **Per-mail "saw it" acknowledgment from the guest.** Guest signals "I'm done with this mail" and the dispatcher swaps when all are acked. Rejected: requires guest-side cooperation we don't currently demand, and it's exactly what the dispatcher's `pending` counter measures from the outside without involving the guest.
- **Replace-during-quiescence-only (require zero pending before replace is even accepted).** Reject any replace that arrives while mail is in flight. Rejected: agents would have to drain externally before each replace, pushing the same complexity outward without the timeout safety net.

## Follow-up work

- **`aether-substrate`**: add `pending: AtomicU32` and `frozen: bool` per component table entry; thread them through dispatch.
- **`aether-substrate`**: rewrite `Scheduler::replace_component` as freeze-drain-swap; extract the timeout into a method parameter with a default.
- **`aether-substrate-mail`**: optional `drain_timeout_ms: Option<u32>` field on `ReplaceComponent` (additive, defaults to the global default).
- **Tests**: scenario tests for (a) replace under load drains successfully, (b) replace times out and old instance survives, (c) post-swap mail reaches the new instance, (d) reply-to-sender request issued before replace gets its reply from the old instance.
- **ADR-0010 / ADR-0015 cross-references**: amend the §5 / lifecycle notes to point at this ADR for the actual swap semantics.
- **Parked, not committed:**
  - Freeze-drain-then-drop for `drop_component` (only if "drop ate my pending mail" surfaces as a problem).
  - Per-recipient drain telemetry (how long replaces are spending in the drain phase) — useful when tuning the default timeout, not needed yet.
