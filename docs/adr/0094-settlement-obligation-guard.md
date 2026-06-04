# ADR-0094: Settlement-obligation guard for owned dispatch

- **Status:** Proposed
- **Date:** 2026-06-04

Amends **ADR-0080 §2 / §12** (and the post-ADR-0086 settlement model). It does not change the settlement *semantics* — `(in_flight == 0 && held_open == 0)` is still the exact settled signal, the producer-site `Sent`/`Finished` brackets are unchanged — it adds a **debug-build runtime check** that enforces the §2 "every `OwnedDispatch` must have `Finished` recorded" obligation at its weakest seam, and names the **transfer-vs-discharge** vocabulary every hand-rolled drain must now follow.

## Context

ADR-0080 §2 requires that every mail's `Sent` is eventually balanced by a `Finished` for the same `root`, or the emit-time `SettlementCounter` (ADR-0086) never reaches zero and every settlement subscriber on that chain hangs. For the two synchronous mailbox variants the substrate brackets this itself: `MailboxEntry::Inline` records `Finished` right after the inline call (`mail/mailer.rs`), and the standard actor dispatcher (`actor/native/dispatcher_slot.rs::dispatch_one`) records it at handler exit. The third variant — `MailboxEntry::Inbox`, the actor-enqueue handler that receives an **owned** `OwnedDispatch` and is expected to move it onto a downstream channel — places the obligation on *whoever eventually drains that channel*. The substrate cannot bracket it at the `route_mail` site (that would double-count and fire settlement prematurely, the inverse of the #846 failure).

Today the only thing steering an author toward discharging that obligation is a **type-shape nudge**: `InlineHandler` receives a borrowed `MailDispatch<'_>` (do-the-work-here shape), `InboxHandler` receives an owned `OwnedDispatch` (move-it-onward shape). The doc comment in `mail/registry.rs` calls this "a structural nudge but not a hard guarantee." It has been violated twice:

- **#846** — a synchronous closure installed on `Inbox` captured fields off the dispatch but had no downstream owner of the bracket; `TestBench::send_bytes` timed out at 5s once strict settlement propagation landed.
- **#1325** (open at time of writing) — the desktop `aether.window` drain (`aether-substrate-bundle::desktop::driver::dispatch_window_envelope`) consumes the `Envelope`, applies the window op, and sends a reply, but never records `Finished` for the inbound `mail_id`. Blocking window ops hang to the MCP 600s timeout.

Both are silent multi-second hangs with **no actor or mail named** — the worst diagnostic shape the runtime offers. The structural nudge is necessary (it documents intent at the type level) but demonstrably insufficient. We want the leak converted into an immediate, located failure during any debug/test run that exercises the path, at zero release cost.

A complication: not every `OwnedDispatch` that is dropped without a local `record_finished` is a leak. The legitimate non-discharge paths are:

- **Relay / move-onward** — a handler re-enqueues the payload to *another* mailbox (building a fresh downstream `Envelope`). The obligation moves with the work; the downstream dispatcher will discharge it.
- **Park** — `mail/mailer.rs`'s `WalkOutcome::Parked` arm holds mail in the handle store until a handle resolves; ADR-0080 §12 deliberately does *not* finish parked mail (it replays later).
- **Fan-out** — one inbound dispatch produces several downstream sends, each its own chain root; the inbound is discharged once, the children are independent.
- **Conversion** — `Envelope` is a type alias for `OwnedDispatch` (`actor/native/envelope.rs`), so the historical `Envelope::from(OwnedDispatch)` "hop" is now a no-op move; the obligation rides the same value across the alias boundary with no seam to lose it.

The guard must recognise these as legitimate and not false-positive.

## Decision

Add a **debug-only obligation guard** to `OwnedDispatch`. Under `#[cfg(debug_assertions)]` the struct carries one extra field — an `ObligationGuard` holding the dispatch's `mail_id`, `kind_name`, and recipient mailbox, plus an armed/satisfied flag. Its `Drop` impl **panics** if the guard is dropped while still *armed* (neither discharged nor transferred), reporting `mail_id` + `kind_name` + mailbox so the offending seam is named at the point of leak. Under `cfg(not(debug_assertions))` the field does not exist and there is no `Drop` impl — the type is byte-identical to today and zero-cost.

The guard is **armed at the obligation-creation site**: the `MailboxEntry::Inbox` arm of `route_mail` (`mail/mailer.rs`) and `ComponentCtx::send`'s inline `Inbox` arm (`actor/wasm/component.rs`) are the only two production sites that mint an `OwnedDispatch` for delivery to an `InboxHandler`; both arm the guard as they construct it. (Test/helper constructors mint disarmed dispatches — see Consequences.)

The guard is disarmed by exactly two affordances, both no-ops in release:

- **`discharge()`** — "the obligation ends here; I am recording `Finished` for this `mail_id`." Called at every site that already calls `Mailer::record_finished(env.mail_id, env.root)` for a consumed envelope: `dispatcher_slot::dispatch_one`, the `dispatch_one` finalized-slot seed path, the wasm trampoline drain (`actor/wasm/component.rs`), and the desktop window drain (the #1325 fix site). The call sits adjacent to the existing `record_finished` so the two cannot drift.
- **`mark_transferred()`** — "the obligation moves with the work onto a downstream envelope / into the park store; the downstream owner will discharge it." Called at the relay / park / fan-out seams enumerated above. The newly-built downstream `OwnedDispatch` arms its own guard, so the obligation count across the hop is conserved.

Mechanically the guard is **decoupled from the `SettlementCounter`**: the counter is keyed on `root` and counts `Sent`/`Finished` in aggregate; the guard is per-`mail_id` per-`OwnedDispatch` and only asks "did *this owned value* get explicitly discharged or transferred before it dropped." The guard never reads or mutates the counter — it is a pure local liveness assertion on the owned value's lifecycle. This keeps it correct under the striped, lock-per-root counter without any new cross-thread coupling.

`Clone`: `OwnedDispatch` derives `Clone` for surface completeness but no production path clones a whole dispatch (verified across `aether-substrate` + `aether-substrate-bundle`). The guard's `Clone` produces a **disarmed** token (a clone is for inspection, not a second live obligation), so an accidental future clone cannot manufacture a phantom obligation. `Debug` skips the guard field.

The §2 contract text in `mail/registry.rs`'s `InboxHandler` doc is updated to cite this ADR and name the `discharge()` / `mark_transferred()` rule, so the next hand-rolled drain author reads the obligation, not just the nudge.

## Consequences

- **Positive** — the #846 and #1325 classes fail loudly at the leaking seam in any debug/test run, naming `mail_id` + `kind_name` + mailbox, instead of hanging anonymously for seconds. The transfer-vs-discharge vocabulary is now explicit, so future capabilities that drain their own mailbox (the desktop window driver is the precedent) have a checked contract rather than a prose hope.
- **Neutral / cost** — release builds are byte-identical (no field, no `Drop`); debug builds add one bool-flag check per dropped `OwnedDispatch`. The guard requires a one-line `discharge()` / `mark_transferred()` annotation at each consumer/relay site — a small, mechanical, audited surface (a handful of sites in `aether-substrate`, one in `aether-substrate-bundle`).
- **Negative / risk** — a *missed* annotation at a legitimate transfer site becomes a debug-build false-positive panic. Mitigated by auditing every `OwnedDispatch` consumer/relay site as part of the change (the implementation issue enumerates them) and by a test that both leaks deliberately (asserts panic) and runs the standard dispatchers (asserts no panic). Test/helper constructors (`mail/registry.rs` test builders, the `noop` handler) mint **disarmed** dispatches so unit tests that never intend to settle do not trip the guard.
- **Follow-on** — complements, does not replace, the #1305 `GateWedge` runtime wedge (frame-gate-only today): that turns a stuck root into a named *runtime* abort; this guard turns a dropped obligation into a *compile-time-class* (debug-build) panic at the seam. Broadening `GateWedge` to general inbound-`Call` settlement remains a separate path.

## Alternatives considered

- **Keep the type-shape nudge only** — rejected: demonstrably insufficient (#846, #1325), and the nudge gives no signal when violated.
- **A separate guard token threaded into `record_finished`** — rejected: `Mailer::record_finished(mail_id, root)` is called from many sites with only the ids in hand, and threading a token through the `Mailer` API would widen a hot, widely-called signature for a debug-only concern. Putting the guard on the owned value keeps the check local to the value whose lifecycle it polices.
- **A release-build runtime leak detector (periodic `in_flight` sweep)** — rejected for this issue: it names a *root* that never settled, not the *seam* that dropped the obligation, and pays a release-build cost. It is a plausible complement (cf. broadening `GateWedge`) but a different mechanism.
- **A brand-new contract rather than an ADR-0080 amendment** — rejected: this hardens an existing, documented invariant (§2/§12) and introduces no new settlement semantics; recording it as an amendment keeps the settlement model in one lineage (as ADR-0086 did).
