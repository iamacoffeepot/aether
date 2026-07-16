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
//! into a [`Commit`], stashes the pending admit (its reply handle, the decoded
//! event, and the decisions) keyed by idempotency key, and sends the commit to
//! `aether.store`. The store is addressed by runtime name (`send_to_named`) —
//! its capability type lives in `aether-bloomery-host`, which depends on this
//! crate, so a typed `resolve_actor` mailbox would be a package cycle. That
//! escape hatch carries no typed reply context, so [`CommitResult`] echoes the
//! idempotency key and [`ControlCore::on_commit_result`] correlates on it,
//! applies the decision to the snapshot (only on `Applied`), and replies the
//! outcome to the original admitter.
//!
//! Boot **does not** drain or ack the outbox — outbox republish belongs to the
//! consumer capabilities (#3499). This actor only *enqueues* outbox entries,
//! atomically inside the commit.

use super::{
    Admit, AdmitResult, Commit, CommitResult, MembershipMutation, OutboxPayload, Query, QueryResult, ReplayJournal,
    ReplayJournalResult,
};
use crate::reduce::{Decision, Decisions, Event, Outcome, Snapshot, reduce, view_of};
use aether_actor::{
    ActorInitError, MailSender, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_data::wire::{from_bytes, to_vec};
use std::collections::BTreeMap;

/// The runtime name of the store capability the control core drives.
const STORE: &str = "aether.store";

/// The outbox topic a landing receipt enqueues under, so #3499's republisher
/// can route it.
const RECEIPT_TOPIC: &str = "aether.bloomery.landing_receipt";

/// An admit awaiting its durable commit reply — the reply handle to answer, the
/// decoded event, and the decisions to apply to the snapshot once the store
/// confirms the commit landed.
struct Pending {
    reply: Option<ReplyHandle>,
    event: Event,
    decisions: Decisions,
}

/// The control-core actor: the live [`Snapshot`] plus the in-flight admits
/// awaiting their commit replies.
pub struct ControlCore {
    snapshot: Snapshot,
    pending: BTreeMap<String, Pending>,
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
    /// commit) or send the combined [`Commit`] to the store and answer on its
    /// reply. Manual class: it issues its own replies (the decode-error and
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
        let decisions = reduce(&self.snapshot, &event);
        // A duplicate key is already in `seen` (applied in this process's life or
        // rebuilt by replay), so it needs no durable commit — reply immediately.
        if matches!(decisions.outcome, Outcome::Duplicate) {
            if let Some(handle) = reply {
                ctx.reply_to(handle, &AdmitResult::Ok { outcome: to_vec(&decisions.outcome).unwrap_or_default() });
            }
            return;
        }
        let key = event.idempotency_key.0.clone();
        let (releases, claims, outbox) = project(&decisions);
        // Every non-duplicate admitted event is journaled — even a rejected one,
        // so a replay stays a no-op and the key is durably consumed (the reducer's
        // `apply` records the key for a rejected outcome too). A rejection carries
        // empty membership/outbox effects, so the commit is a bare journal append.
        let commit = Commit { idempotency_key: key.clone(), event: raw, releases, claims, outbox };
        self.pending.insert(key, Pending { reply, event, decisions });
        ctx.send_to_named(STORE, &commit);
    }

    /// The store's reply to a [`Commit`]. Correlate on the echoed idempotency
    /// key, apply the decision to the snapshot only when the commit durably
    /// landed, and reply the reducer outcome to the original admitter.
    #[handler::manual]
    fn on_commit_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: CommitResult) {
        let key = commit_key(&mail).to_owned();
        let Some(Pending { reply, event, decisions }) = self.pending.remove(&key) else {
            // No admit is waiting on this key — a stray or double reply.
            return;
        };
        let result = match mail {
            CommitResult::Applied { .. } => {
                self.snapshot = self.snapshot.apply(&event, &decisions);
                AdmitResult::Ok { outcome: to_vec(&decisions.outcome).unwrap_or_default() }
            }
            // The store already held this key durably though our snapshot did not
            // — a rare divergence (a reply racing a concurrent replay). Reply
            // Duplicate and do not double-apply.
            CommitResult::Duplicate { .. } => {
                AdmitResult::Ok { outcome: to_vec(&Outcome::Duplicate).unwrap_or_default() }
            }
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
        let document = view_of(&self.snapshot);
        let result = match mail.bloom {
            None => QueryResult::Document { document: to_vec(&document).unwrap_or_default() },
            Some(bytes) => {
                match document.blooms.iter().find(|view| view.id.0.as_bytes().as_slice() == bytes.as_slice()) {
                    Some(view) => QueryResult::Bloom { view: to_vec(view).unwrap_or_default() },
                    None => QueryResult::NotFound,
                }
            }
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
/// resolution / mark superseded / set resolved / advance mainline) carry no
/// durable store row — they are rebuilt on replay by `reduce` + `apply` from
/// the journaled event.
fn project(decisions: &Decisions) -> (Vec<MembershipMutation>, Vec<MembershipMutation>, Vec<OutboxPayload>) {
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
                outbox.push(OutboxPayload {
                    topic: RECEIPT_TOPIC.to_owned(),
                    payload: to_vec(receipt).unwrap_or_default(),
                });
            }
            Decision::InheritClaim { .. }
            | Decision::RecordResolution { .. }
            | Decision::MarkSuperseded { .. }
            | Decision::SetResolved { .. }
            | Decision::AdvanceMainline { .. } => {}
        }
    }
    (releases, claims, outbox)
}

aether_actor::export!(ControlCore);
