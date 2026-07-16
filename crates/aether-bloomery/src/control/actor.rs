//! The control-core wasm actor (ADR-0149 §The control core, §Migration step 1).
//!
//! [`ControlCore`] is the single owner of the live [`Snapshot`]: it drives
//! [`reduce`] on every admitted event, commits the decision through the
//! `aether.store` capability in one atomic transaction, applies the decision to
//! its in-memory snapshot on the commit reply, and serves reads off that
//! snapshot. At boot it replays the journal to rebuild the snapshot, so a
//! `kill -9` + restart converges through the reducer — the migration step 1
//! exit gate.
//!
//! # The async store choreography
//!
//! A wasm actor cannot block on a host reply, so an admit is a two-message
//! exchange. [`ControlCore::on_admit`] reduces the event, projects the decision
//! into a [`Commit`], stashes the pending admit (the reply handles awaiting it,
//! the decoded event, and the decisions) keyed by idempotency key, and sends the
//! commit to `aether.store`. Only the *first* admit for a key opens that entry
//! and forwards a commit; a same-key admit arriving while the commit is still
//! outstanding attaches its reply to the entry's waiters rather than reducing or
//! committing again — a resent idempotency key is the same operation and gets the
//! same answer. The store is addressed by runtime name (`send_to_named`) — its
//! capability type lives in `aether-bloomery-host`, which depends on this crate,
//! so a typed `resolve_actor` mailbox would be a package cycle. That escape hatch
//! carries no typed reply context, so [`CommitResult`] echoes the idempotency key
//! and [`ControlCore::on_commit_result`] correlates on it, applies the decision
//! to the snapshot (only on `Applied`), and fans the one outcome out to every
//! waiting admitter.
//!
//! Boot **does not** drain or ack the outbox — outbox republish belongs to the
//! consumer capabilities (#3499). This actor only *enqueues* outbox entries,
//! atomically inside the commit.

use super::{
    Admit, AdmitResult, Commit, CommitResult, MembershipMutation, OutboxPayload, Query, QueryResult, ReplayJournal,
    ReplayJournalResult,
};
use crate::digest::Digest;
use crate::reduce::{Decision, Decisions, Event, Outcome, Snapshot, reduce, view_of};
use aether_actor::{
    ActorInitError, MailSender, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_data::wire::{Error as WireError, from_bytes, to_vec};
use std::collections::{BTreeMap, VecDeque};

/// The runtime name of the store capability the control core drives.
const STORE: &str = "aether.store";

/// The cap on same-key admits in flight at once. A well-behaved client sends one
/// admit per key and at most a few retries; without a bound a client spamming one
/// key while its commits are outstanding would grow the per-key queue without
/// limit, pinning memory on the single snapshot owner. An admit past the cap is
/// refused rather than queued (CLAUDE.md §Runtime: error rather than grow
/// unboundedly).
const MAX_INFLIGHT_PER_KEY: usize = 64;

/// The outbox topic a landing receipt enqueues under, so #3499's republisher
/// can route it.
const RECEIPT_TOPIC: &str = "aether.bloomery.landing_receipt";

/// The outbox topic a stage re-dispatch enqueues under, so the dispatch consumer
/// (#3505) re-assembles the held attempt naming both question and answer digests
/// (ADR-0151). Producer-only here, like [`RECEIPT_TOPIC`].
const REDISPATCH_TOPIC: &str = "aether.bloomery.redispatch";

/// An admit awaiting its durable commit reply — the reply handle to answer, the
/// decoded event, and the decisions to apply to the snapshot once the store
/// confirms the commit landed. Each in-flight admit owns one, held in its key's
/// FIFO queue.
struct Pending {
    reply: Option<ReplyHandle>,
    event: Event,
    decisions: Decisions,
}

/// The control-core actor: the live [`Snapshot`] plus the in-flight admits
/// awaiting their commit replies, queued per idempotency key.
///
/// Each same-key admit gets its own `Pending` entry and its own [`Commit`], so
/// every admit's inbound chain stays open (extended by that commit) until its
/// reply is sent — a second admit that merely stashed a reply handle and returned
/// would let its chain settle at handler return, closing its reply stream before
/// the answer could be delivered (ADR-0080 chain settlement). The store's
/// idempotency-key dedup collapses every same-key commit after the first to a
/// [`CommitResult::Duplicate`] no-op, so the pair still yields one journal row and
/// one applied decision. The per-key queue is FIFO: the store replies in
/// send order, so `on_commit_result` pops the front entry to match each reply.
pub struct ControlCore {
    snapshot: Snapshot,
    pending: BTreeMap<String, VecDeque<Pending>>,
}

#[actor]
impl WasmActor for ControlCore {
    const NAMESPACE: &'static str = "aether.bloomery.control";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { snapshot: Snapshot::default(), pending: BTreeMap::new() })
    }

    /// Boot replay: ask the store for the whole journal; [`Self::on_replay_result`]
    /// folds it back into the snapshot. Lives in `wire` (post-init, mail-allowed).
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.send_to_named(STORE, &ReplayJournal);
    }

    /// The `aether.bloomery.admit` ingress. Decode the event, reduce it against
    /// the live snapshot, and either reply immediately (a duplicate needs no
    /// commit) or queue an in-flight entry under its idempotency key and send the
    /// combined [`Commit`] to the store, answering on its reply. A second admit
    /// for a key whose first commit is still outstanding gets its own entry and
    /// its own commit — the store dedups the second to a
    /// [`CommitResult::Duplicate`] no-op — rather than sharing the first's, so its
    /// inbound chain stays open (extended by its commit) until it is answered.
    /// Manual class: it issues its own replies (the decode-error, cap, and
    /// duplicate paths reply here; the committed path replies in
    /// [`Self::on_commit_result`]).
    #[handler::manual]
    fn on_admit(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: Admit) {
        let reply = ctx.reply_target();
        let raw = mail.event;
        let event: Event = match from_bytes(&raw) {
            Ok(event) => event,
            Err(error) => {
                if let Some(handle) = reply {
                    ctx.reply_to(handle, &AdmitResult::Err { error: format!("admit decode failed: {error}") });
                }
                return;
            }
        };
        let key = event.idempotency_key.0.clone();
        // Back-pressure: cap the same-key admits in flight at once. A well-behaved
        // client sends one admit per key plus a few retries; a client spamming one
        // key while its commits are outstanding would grow the per-key queue
        // without bound. Refuse past the cap rather than queue (CLAUDE.md §Runtime:
        // error rather than grow unboundedly).
        if self.pending.get(&key).is_some_and(|queue| queue.len() >= MAX_INFLIGHT_PER_KEY) {
            if let Some(handle) = reply {
                ctx.reply_to(
                    handle,
                    &AdmitResult::Err { error: "too many concurrent admits for this idempotency key".to_owned() },
                );
            }
            return;
        }
        let decisions = reduce(&self.snapshot, &event);
        // A duplicate key is already in `seen` (applied in this process's life or
        // rebuilt by replay), so it needs no durable commit — reply immediately.
        // A same-key admit whose predecessor is still in flight has not applied
        // yet, so it does not land here; it commits and the store's dedup answers
        // it Duplicate on the reply.
        if matches!(decisions.outcome, Outcome::Duplicate) {
            if let Some(handle) = reply {
                ctx.reply_to(handle, &admit_ok(&decisions.outcome));
            }
            return;
        }
        // Projecting the decision encodes each outbox receipt; a receipt-encode
        // failure must reject the admit, not commit an empty payload the
        // republisher would later route as a valid-but-blank receipt.
        let (releases, claims, outbox) = match project(&decisions) {
            Ok(effects) => effects,
            Err(error) => {
                if let Some(handle) = reply {
                    ctx.reply_to(handle, &AdmitResult::Err { error: format!("admit receipt encode failed: {error}") });
                }
                return;
            }
        };
        // Every non-duplicate admitted event is journaled — even a rejected one,
        // so a replay stays a no-op and the key is durably consumed (the reducer's
        // `apply` records the key for a rejected outcome too). A rejection carries
        // empty membership/outbox effects, so the commit is a bare journal append.
        let commit = Commit { idempotency_key: key.clone(), event: raw, releases, claims, outbox };
        // Queue this admit's entry under its key (FIFO) and forward its Commit. A
        // same-key admit already in flight enqueues behind its predecessor rather
        // than displacing it, so no admitter is ever stranded without a reply.
        self.pending.entry(key).or_default().push_back(Pending { reply, event, decisions });
        ctx.send_to_named(STORE, &commit);
    }

    /// The store's reply to a [`Commit`]. Correlate on the echoed idempotency
    /// key, pop the matching key's front in-flight entry (the store replies in
    /// send order, so the queue is FIFO), apply the decision to the snapshot only
    /// when the commit durably landed, and reply the outcome to that admitter. A
    /// same-key follow-on admit's commit lands here as [`CommitResult::Duplicate`]
    /// and answers its own admitter without re-applying.
    #[handler::manual]
    fn on_commit_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: CommitResult) {
        let key = commit_key(&mail).to_owned();
        let Some(queue) = self.pending.get_mut(&key) else {
            // No admit is waiting on this key — a stray or double reply.
            return;
        };
        let Some(Pending { reply, event, decisions }) = queue.pop_front() else {
            return;
        };
        // Drop the key's slot once its last in-flight entry is answered, so the
        // map does not retain empty queues.
        if queue.is_empty() {
            self.pending.remove(&key);
        }
        let result = match mail {
            CommitResult::Applied { .. } => {
                self.snapshot = self.snapshot.apply(&event, &decisions);
                admit_ok(&decisions.outcome)
            }
            // The store already held this key durably though our snapshot did not
            // — a rare divergence (a reply racing a concurrent replay). Reply
            // Duplicate and do not double-apply.
            CommitResult::Duplicate { .. } => admit_ok(&Outcome::Duplicate),
            // The durable uniqueness backstop refused a claim the reducer's
            // snapshot screen missed — do not apply; report the conflict.
            CommitResult::Conflict { workpiece, .. } => {
                AdmitResult::Err { error: format!("store membership conflict on {workpiece}") }
            }
            CommitResult::Err { error, .. } => AdmitResult::Err { error },
        };
        if let Some(handle) = reply {
            ctx.reply_to(handle, &result);
        }
    }

    /// Boot journal replay reply: fold each record (decode the event, `reduce`,
    /// `apply`) to rebuild the snapshot. No outbox drain/ack — republish is
    /// #3499's. A read or a corrupt record at boot is unrecoverable, so it
    /// fail-fasts (ADR-0063) rather than coming up on a torn snapshot.
    #[handler::single]
    fn on_replay_result(&mut self, ctx: &mut WasmCtx<'_>, mail: ReplayJournalResult) {
        let records = match mail {
            ReplayJournalResult::Ok { records } => records,
            ReplayJournalResult::Err { error } => {
                ctx.fatal_abort(format!("boot journal replay failed: {error}"));
            }
        };
        for record in records {
            let event: Event = match from_bytes(&record.event) {
                Ok(event) => event,
                Err(error) => ctx.fatal_abort(format!(
                    "boot journal replay: record {} ({}) did not decode: {error}",
                    record.sequence, record.idempotency_key
                )),
            };
            let decisions = reduce(&self.snapshot, &event);
            self.snapshot = self.snapshot.apply(&event, &decisions);
        }
    }

    /// The `aether.bloomery.query` read surface. With `bloom` unset, reply the
    /// whole [`ViewDocument`](crate::port::ViewDocument); with `bloom` set to a
    /// digest, reply that one bloom's [`BloomView`](crate::port::BloomView) (or
    /// [`QueryResult::NotFound`]). Reads off the live snapshot — the single
    /// snapshot owner, so #3498's REST reads never rebuild one per request.
    #[handler::manual]
    fn on_query(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: Query) {
        let Some(handle) = ctx.reply_target() else {
            return;
        };
        // The live-read path holds no artifact access, so it resolves no question
        // bytes: a held member surfaces its pending decision only on the outward
        // mirror path, which resolves the Question artifact. The digest-only hold
        // still gates resolution in the reducer regardless (ADR-0151).
        let document = view_of(&self.snapshot, |_| None);
        // On an encode failure reply the error path rather than substituting an
        // empty payload a reader would decode as a valid-but-blank projection.
        let result = match mail.bloom {
            None => match to_vec(&document) {
                Ok(document) => QueryResult::Document { document },
                Err(error) => QueryResult::Err { error: format!("document encode failed: {error}") },
            },
            Some(bytes) => document
                .blooms
                .iter()
                .find(|view| view.id.0.as_bytes().as_slice() == bytes.as_slice())
                .map_or(QueryResult::NotFound, |view| match to_vec(view) {
                    Ok(view) => QueryResult::Bloom { view },
                    Err(error) => QueryResult::Err { error: format!("bloom view encode failed: {error}") },
                }),
        };
        ctx.reply_to(handle, &result);
    }
}

/// The idempotency key echoed on every [`CommitResult`] variant (the
/// correlation axis for a name-addressed commit reply).
fn commit_key(result: &CommitResult) -> &str {
    match result {
        CommitResult::Applied { idempotency_key, .. }
        | CommitResult::Duplicate { idempotency_key }
        | CommitResult::Conflict { idempotency_key, .. }
        | CommitResult::Err { idempotency_key, .. } => idempotency_key,
    }
}

/// Project a decided event's effects into the store commit's typed axes: the
/// membership releases and claims the `active_membership` table applies, and
/// the outbox payloads it enqueues. The snapshot-only effects (inherit / record
/// resolution / record evidence / mark superseded / set resolved / advance
/// mainline) carry no durable store row — they are rebuilt on replay by
/// `reduce` + `apply` from the journaled event.
#[allow(clippy::type_complexity)]
fn project(
    decisions: &Decisions,
) -> Result<(Vec<MembershipMutation>, Vec<MembershipMutation>, Vec<OutboxPayload>), WireError> {
    let mut releases = Vec::new();
    let mut claims = Vec::new();
    let mut outbox = Vec::new();
    for effect in &decisions.effects {
        match effect {
            Decision::ClaimMembership { workpiece, bloom } => {
                claims.push(MembershipMutation { workpiece: workpiece.0.clone(), bloom: bloom.0.as_bytes().to_vec() });
            }
            Decision::ReleaseMembership { workpiece, bloom } => {
                releases
                    .push(MembershipMutation { workpiece: workpiece.0.clone(), bloom: bloom.0.as_bytes().to_vec() });
            }
            Decision::EmitReceipt(receipt) => {
                outbox.push(OutboxPayload { topic: RECEIPT_TOPIC.to_owned(), payload: to_vec(receipt)? });
            }
            Decision::RedispatchStage { bloom, question, answer } => {
                let payload = RedispatchPayload { bloom: bloom.0, question: *question, answer: *answer };
                outbox.push(OutboxPayload { topic: REDISPATCH_TOPIC.to_owned(), payload: to_vec(&payload)? });
            }
            Decision::InheritClaim { .. }
            | Decision::RecordResolution { .. }
            | Decision::RecordEvidence { .. }
            | Decision::ReleaseHold { .. }
            | Decision::MarkSuperseded { .. }
            | Decision::SetResolved { .. }
            | Decision::AdvanceMainline { .. } => {}
        }
    }
    Ok((releases, claims, outbox))
}

/// The re-dispatch outbox payload: the bloom, the released question, and the
/// adopting answer, each by digest. Opaque bytes the dispatch consumer (#3505)
/// decodes to re-assemble the held attempt naming both digests (ADR-0151).
#[derive(serde::Serialize, serde::Deserialize)]
struct RedispatchPayload {
    bloom: Digest,
    question: Digest,
    answer: Digest,
}

/// Encode a reducer [`Outcome`] into an [`AdmitResult`], mapping an encode
/// failure to the `Err` reply rather than an empty `Ok` payload the admitter
/// would decode as a valid-but-blank outcome.
fn admit_ok(outcome: &Outcome) -> AdmitResult {
    match to_vec(outcome) {
        Ok(outcome) => AdmitResult::Ok { outcome },
        Err(error) => AdmitResult::Err { error: format!("admit outcome encode failed: {error}") },
    }
}

aether_actor::export!(ControlCore);
