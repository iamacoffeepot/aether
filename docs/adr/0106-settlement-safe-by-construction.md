# ADR-0106: Settlement safe by construction

- **Status:** Proposed
- **Date:** 2026-06-12

Amends **ADR-0094** (settlement-obligation guard) and the consumer side of **ADR-0080 §2**. The settlement semantics are unchanged — `(in_flight == 0 && held_open == 0)` is still the settled signal, and the producer-site brackets stay where they are. What changes is who implements the consumer side of the bracket for claimed mailboxes: the framework, once, instead of every hand-rolled drain.

## Context

ADR-0080 §2 obliges every `OwnedDispatch` delivered to an `InboxHandler` to eventually have `Finished` recorded. The standard actor path discharges this naturally — `DispatcherSlot::dispatch_one` records `Finished` unconditionally at its tail, so an actor author cannot leak the bracket. The non-actor path cannot say the same: `ChassisCtx::claim_mailbox` hands the capability a raw `mpsc::Receiver<Envelope>`, and every drain built on it must remember to pair `Mailer::record_finished(mail_id, root)` with the ADR-0094 `env.discharge()` on every arm — decode-error returns, unrecognised-kind drops, early returns, and teardown.

That contract has been violated three times:

- **#846** — the test-bench settlement wait silently swallowed a leak that strict propagation later surfaced.
- **#1325** — the desktop `aether.window` drain consumed envelopes, replied, and never recorded `Finished`; every blocking window op hung to the MCP 600s timeout.
- **#1704** — the desktop lifecycle-reply consume dropped an armed envelope on a teardown path; the ADR-0094 debug guard panicked inside winit's `draw_rect`, where a panic cannot unwind, and the process aborted.

ADR-0094 made each instance loud in a debug run, which is how #1704 was found in hours instead of weeks. But the guard detects; it does not prevent. Each occurrence was a *new* seam re-implementing the same bracket and missing an arm. Worse, the failure mode is escalating: #1701 (replies inherit chain lineage) re-armed a path that was previously silent because reply envelopes carried `MailId::NONE`, turning a latent omission into a debug-build abort. Every future lineage-correctness improvement risks the same conversion. The contract is manual at exactly the seams where authors are furthest from the settlement model.

The same asymmetry exists on the emission side. #1701 split `Mailer::send_reply` (mints the un-lineaged `NONE` triple) from `send_reply_with_lineage` (joins the caller's causal chain) — and left the bare form as the short, obvious name. A hand-rolled drain that replies via `send_reply` silently detaches the reply from the caller's settlement window; correctness is opt-in via the longer name and a hand-threaded lineage triple.

The audited blast radius today is small — the desktop driver's two claimed inboxes (`aether.window`, `aether.lifecycle.advance_reply`) are the only out-of-crate hand-rolled `Envelope` consumers (#1704's bundle-wide sweep) — but the surface invites more: any future driver-as-actor capability starts by claiming a mailbox and writing a drain.

## Decision

Seal the claimed-mailbox surface behind one framework drain, and fuse both obligations — settlement discharge and reply lineage — to the value the consumer is given. Three pieces:

**1. `ClaimedInbox` replaces the raw receiver.** `MailboxClaim` no longer exposes `mpsc::Receiver<Envelope>`; it carries a `ClaimedInbox` owning the receiver plus an `Arc<Mailer>` and the claim's `MailboxId`. The raw-receiver shape survives only inside `aether-substrate` (the `DropOnShutdownClaim` receiver that feeds the standard dispatcher narrows to `pub(crate)`). Outside the substrate crate it is no longer possible to obtain an armed `Envelope` from a claim.

**2. Per-mail guard settles on scope exit.** `ClaimedInbox`'s drain methods (`try_next()` for selective consumes, a closure-driven `drain()` for burst drains) yield each mail as an `InboundMail<'_>` guard. The guard exposes the envelope's fields by borrow — `kind`, `kind_name`, `sender`, `payload`, the lineage triple — and its `Drop` records `Finished(mail_id, root)` and disarms the ADR-0094 obligation in one motion, mirroring `dispatch_one`'s unconditional tail. Every arm of the consumer — match, decode error, unrecognised-kind drop, early return, panic-unwind — settles, because settlement is what falling out of scope *does*. Settling on scope exit rather than on payload access is load-bearing: ADR-0080 §6 requires a reply's `Sent` to be recorded before the inbound's `Finished`, so a consume-time discharge would close the caller's chain before the reply joins it.

**3. Replies inherit the inbound's lineage through the guard.** `InboundMail::reply(&K)` routes through `send_reply_with_lineage`, minting the reply id from a drain-owned counter in the disjoint reply-lineage id space (#1701's `1 << 63` base) and stamping the inbound's `root` / `parent`. A claimed-mailbox consumer never touches the bare `send_reply` or hand-threads a triple — the correlated, chain-joined reply is the only reply the surface offers.

Teardown is the same mechanism: dropping a `ClaimedInbox` drains whatever is still queued and lets each guard settle — the #1704 shape (queued reply envelope dropped on driver teardown) becomes a settled drain instead of an armed drop. A consumer that must abandon mail early gets no bypass; abandonment *is* a settle, which matches what the manual fix sites already do.

Out of scope, deliberately: `InboxHandler` registration closures (`Registry::register_inbox`) still receive raw `OwnedDispatch` — that is the move-onward relay shape, and the three production closures already route through the shared `relay_or_transfer` core (ADR-0094 / #1564). Bespoke out-of-crate closures are test-only and mint disarmed dispatches. Flipping `Mailer::send_reply`'s default for the remaining chassis-cap callers (the render driver and friends, which manage their own settlement) is a follow-up, not this change — here the fused reply only needs to cover the surface being sealed.

## Consequences

- **Positive** — the #846/#1325/#1704 class becomes unrepresentable at the claimed-mailbox seam rather than detectable: the consumer cannot reach the payload without holding the value whose drop settles. The bracket has one implementation; the desktop driver's per-arm `discharge_settlement` + `env.discharge()` pairs (six sites across two drains) delete. New driver-as-actor capabilities inherit the safe shape for free.
- **Positive** — reply correlation carries by construction: a reply sent through the guard is always chain-joined, so the #1701 conformance holds at this seam without per-consumer lineage threading.
- **Neutral / cost** — `claim_mailbox`'s return type changes; the two existing consumers migrate in the same PR. The guard adds one `record_finished` call per drained mail, which the correct manual code already paid.
- **Negative / risk** — a consumer that genuinely needs to *hold* mail across drain calls (park-like behavior) has no surface; today none exists at this seam, and adding an explicit escape later is additive. The ADR-0094 guard stays — it still covers the in-crate relay/park/fan-out seams the drain does not subsume.
- **Follow-on** — ADR-0094's consumer-contract text and the `InboxHandler` docs in `mail/registry.rs` are updated to point hand-rolled-drain authors at `SettlingInbox`. The broader `send_reply` default-flip for self-settling chassis caps is left to a future issue.
- **Extended (#1756)** — `ClaimedInbox` is renamed `SettlingInbox` and generalized: a `pub(crate)` dispatcher face (`recv_blocking` / `try_recv`) is added so the native actor dispatcher can hold a raw `Envelope` through its explicit `record_finished` + `discharge` tail. `NativeBinding.inbox` is backed by `SettlingInbox` (via `OnceLock<Mutex<SettlingInbox>>`), so its `Drop` now settles any residue queued at binding teardown (closes #1716). The duplicated `1 << 63` reply-lineage base constants in `inbox.rs` and `binding.rs` are collapsed into one `ReplyLineage(Arc<AtomicU64>)` newtype; the `SettlingInbox` inside a `NativeBinding` shares the binding's `ReplyLineage` so both draw from one coherent disjoint id space.
- **Extended (#1802)** — with close-on-drop settling armed obligations on every consumer's drop, `ctx.actor::<R>().send()` now inherits the handler's causal chain by default (ADR-0080 §7) on both the native and FFI handles, so an outbound send arms a settlement obligation rather than truncating the trace at the send; `send_detached()` is the explicit fire-and-forget opt-out. The generalized `SettlingInbox` is what makes the now-armed obligation safe — it can no longer dangle a chain open.

## Alternatives considered

- **Consume-on-access accessor** (payload bytes only via a method that records `Finished` as it hands them out) — rejected: settles the caller's chain before the handler's reply records `Sent`, violating ADR-0080 §6 hold ordering. The fusion must be scope-exit, not access-time.
- **Keep the raw receiver and audit harder** — rejected: ADR-0094 already names every leak loudly, and the class recurred anyway; detection demonstrably does not prevent the next seam.
- **Privatize `OwnedDispatch` fields wholesale** — rejected: the in-crate dispatcher / relay / park seams legitimately need field access and `mark_transferred`; narrowing the whole type punishes the paths that are already safe. Sealing the claim surface closes the out-of-crate seam, which is where every recurrence lived.
- **Flip `send_reply` to lineage-by-default everywhere in this change** — rejected for scope: it touches every chassis capability's reply site and their settlement self-management; the guard's fused `reply()` covers the seam this decision seals, and the global flip stands alone as its own change.
