//! Boot-time recovery of dispatches that were acknowledged and then lost
//! (issue #4956) — the third leg of restart recovery, beside `seed_tracked`
//! (#3641) and the lane reconciliation (#4847).
//!
//! # What the other two legs cannot see
//!
//! Both existing legs are scoped to `outstanding_orders`: `seed_tracked` re-polls
//! every row still there, and the lane reconciliation re-adopts the local runs
//! behind them. That makes an empty table indistinguishable from an idle
//! coordinator — which is exactly what a boot reported (`retracked=0 readopted=0
//! reclaimed=0`) while the fold believed a repair lap was in flight and nothing
//! would ever move again.
//!
//! An order can leave that table without anything folding, because the two
//! writes that end a dispatch are not one commit. [`admit_uploaded`] deletes the
//! order the moment its admission is constructed, and the reactor then mails the
//! resulting `Admit` to the control core fire-and-forget; the fact becomes
//! durable only when *that* actor commits it. A process that stops inside that
//! window has spent the order and journaled nothing. Nothing re-derives the
//! dispatch afterwards either: the reducer emits a
//! [`DispatchAttempt`](aether_bloomery::Decision::DispatchAttempt) only in
//! response to a fact, so with no fact there is no re-decision — the member sits
//! at the cursor its dispatch advanced it to, unwedged, holding no question, and
//! permanently parked.
//!
//! # The four questions that identify one
//!
//! The recovery reads the acknowledged dispatch outbox rows — the durable record
//! of every dispatch the reducer ever decided, payload intact — and asks, for
//! each one's [`dispatch_nonce`]:
//!
//! - **Is the order still outstanding?** Then it is in flight and the other two
//!   legs own it.
//! - **Was an order ever recorded for it?** `dispatch_owners` outlives the
//!   consume, so a nonce absent from it never reached a worker lane: the entry
//!   was acked by one of the deliberate park paths (a retired plan, a permanent
//!   submit refusal, a sealed configuration that would not resolve), each of
//!   which logged its reason at the moment it acked. Re-driving one would just
//!   re-run the decision that parked it.
//! - **Does the journal hold any admission keyed to it?** Every
//!   [`AdmissionKey`] is nonce-keyed, so a row under one of them is the durable
//!   statement that this dispatch was accounted for.
//! - **Does its bloom still hold an active membership?** A superseded plan's
//!   dispatch is dead, not stranded, and the drain would retire it anyway.
//!
//! What survives all four is a dispatch whose acknowledgement was never earned.
//!
//! # Readopt, not fault
//!
//! The recovery returns the entry to the undelivered queue and lets the ordinary
//! drain dispatch it again. Re-dispatch is what the pre-restart coordinator
//! would have arrived at on its own — the lane child died with its parent, so
//! there is no run to re-attach to and no evidence to recover — and routing it
//! back through the one path that knows how to dispatch keeps the retired-plan
//! check, the sealed-configuration overlay, and the park semantics in a single
//! place. The nonce is a pure function of the outbox sequence, so the re-drive
//! submits under the same nonce, records the same order, and admits under the
//! same idempotency key; the fold's `attempts` cursor never moved, so nothing is
//! double-counted against the stage's retry budget.
//!
//! A fault decision was the alternative, and it is the weaker answer here: it
//! would need new journal vocabulary to say something the pipeline can already
//! act on, and it would stop a member the coordinator is perfectly able to
//! resume. Where re-dispatch cannot proceed the drain's existing park paths take
//! over, each of which logs its reason.
//!
//! Two residual behaviours, both deliberate. A re-driven dispatch that parks on
//! a permanent refusal leaves the same shape behind, so the next boot re-drives
//! it once more — bounded by boots, loud each time, and self-healing the moment
//! the refusal clears. And a member whose cursor moved on some *other* fact
//! while its own was lost re-runs a stage it has left; the returning evidence
//! then admits against a cursor that disagrees, which the reducer already
//! refuses as stale rather than folding.
//!
//! [`admit_uploaded`]: crate::bloomery::intake::admit_uploaded

use aether_bloomery::{Nonce, Topic};

use crate::bloomery::intake::{AdmissionKey, dispatch_nonce};
use crate::bloomery::outbox::TopicOutbox;
use crate::store::StoreBackend;

/// The outbox topics whose entries dispatch under a [`dispatch_nonce`] and
/// admit under an [`AdmissionKey`] — every topic the executor reactor drains
/// into a work order.
///
/// The other reducer topics (land, integration, receipts, claim releases) do not
/// mint orders, so they have no nonce to strand.
const ORDER_BEARING_TOPICS: [Topic; 5] =
    [Topic::Dispatch, Topic::AggregateReview, Topic::AggregateVerify, Topic::ScopeDispatch, Topic::BaseVerify];

/// Re-queue every acknowledged dispatch whose order was spent without its fact
/// reaching the journal, returning the nonces put back in flight.
///
/// # Errors
/// Propagates any store read/write fault. The caller fails boot on one for the
/// same reason `seed_tracked` does: a recovery pass that silently recovers
/// nothing is the bug it exists to close.
pub(super) fn readopt_stranded_dispatches(store: &mut dyn StoreBackend) -> rusqlite::Result<Vec<Nonce>> {
    let mut readopted = Vec::new();
    for topic in ORDER_BEARING_TOPICS {
        for entry in store.delivered_topic(topic)? {
            let nonce = dispatch_nonce(entry.sequence);
            if !is_stranded(store, &nonce)? || !store.redeliver_topic(topic, entry.sequence)? {
                continue;
            }
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                nonce = %nonce.0,
                ?topic,
                "dispatch was acknowledged but its order was spent without reaching the journal; re-queueing it",
            );
            readopted.push(nonce);
        }
    }
    Ok(readopted)
}

/// Whether `nonce` names a dispatch that was recorded, spent, and never
/// accounted for — the four questions in the module docs, cheapest first.
fn is_stranded(store: &mut dyn StoreBackend, nonce: &Nonce) -> rusqlite::Result<bool> {
    if store.lookup_order(&nonce.0)?.is_some() {
        return Ok(false);
    }
    // A pre-bloom scoping run is accounted for by its own ledger, never by the
    // journal (ADR-0208, #5304): its verdict produces no `Fact`, because there
    // is no bloom for one to be about. Neither question below can answer for it
    // — `journal_holds_any` would call every completed run stranded, and the
    // membership read at the end would call every run *not* stranded, because
    // the reserved scope-run digest holds no membership by construction. The
    // `verdict` row is the statement that this dispatch reached the
    // coordinator, so it is the one this check asks for.
    if let Some((commission, ordinal)) = store.lookup_scope_run(&nonce.0)? {
        let answered =
            store.list_scope_runs(&commission)?.iter().any(|row| row.ordinal == ordinal && row.kind == "verdict");
        return Ok(!answered);
    }
    let Some(bloom) = store.lookup_dispatch_owner(&nonce.0)? else {
        return Ok(false);
    };
    if store.journal_holds_any(&AdmissionKey::every_key_for(&nonce.0))? {
        return Ok(false);
    }
    store.holds_active_membership(&bloom)
}
