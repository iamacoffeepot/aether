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

use super::claim_plan::{
    HealOp, ReconcileOp, held_to_seal_error, held_to_supersede_error, plan_heals, reconcile_op, release_seal_mail,
    seal_claim_mail, transfer_seal_mail,
};
use super::{
    Admit, AdmitResult, ClaimResult, ClaimSeal, Commit, CommitResult, EnumerateClaims, EnumerateClaimsResult,
    MembershipMutation, OutboxPayload, Query, QueryResult, ReplayJournal, ReplayJournalResult, TransferSeal,
};
use crate::digest::Digest;
use crate::ids::BloomId;
use crate::port::{ClaimRefKind, ClaimRefState};
use crate::reduce::{Decision, Decisions, Event, Fact, Outcome, Snapshot, reduce, view_of};
use aether_actor::{
    ActorInitError, MailSender, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_data::wire::{Error as WireError, from_bytes, to_vec};
use std::collections::{BTreeMap, VecDeque};

/// The runtime name of the store capability the control core drives.
const STORE: &str = "aether.store";

/// The runtime name of the native source-port capability the control core
/// drives for the cross-instance claim-ref interposition (ADR-0150 §The claim
/// registry). Addressed by runtime name like [`STORE`] — its capability type
/// lives in `aether-bloomery-host` (a package cycle away), so the seal
/// transact-mail kinds are defined here (`super::source_mail`) and the reply
/// (`ClaimResult`) is correlated by the host-minted correlation id rather than a
/// typed reply mailbox.
const SOURCE: &str = "aether.source";

/// The cap on same-key admits in flight at once. A well-behaved client sends one
/// admit per key and at most a few retries; without a bound a client spamming one
/// key while its commits are outstanding would grow the per-key queue without
/// limit, pinning memory on the single snapshot owner. An admit past the cap is
/// refused rather than queued (CLAUDE.md §Runtime: error rather than grow
/// unboundedly). The claim-gated (seal/supersede) admits awaiting a `ClaimResult`
/// count toward the same per-key cap ([`ControlCore::inflight_for_key`]), so the
/// pre-commit ref stage cannot dodge the back-pressure.
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

/// Which claim door an admit gated on, so a [`ClaimResult::Held`] maps to the
/// right refusal vocabulary — a seal reports a [`SealError`], a supersession a
/// [`SupersedeError`], each reading exactly as the local reducer's own refusal.
enum ClaimKind {
    Seal,
    Supersede,
}

/// The pre-commit claim mail an accepted seal/supersession sends — an acquire
/// over the member + admission refs, or a predecessor→successor transfer.
enum SourceClaim {
    Seal(ClaimSeal),
    Transfer(TransferSeal),
}

/// A locally-accepted seal/supersession awaiting the shared claim-ref reply
/// (ADR-0150). Carries everything the durable commit needs once the refs are
/// acquired: the reply handle, the original event bytes (the journal source),
/// the decoded event and its decisions, and which door to name a `Held`
/// refusal after. Keyed by the host-minted correlation id.
struct PendingClaim {
    reply: Option<ReplyHandle>,
    raw: Vec<u8>,
    event: Event,
    decisions: Decisions,
    kind: ClaimKind,
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
///
/// `pending_claims` holds the accepted seals/supersessions awaiting their shared
/// claim-ref replies (the pre-commit cross-instance exclusivity gate, ADR-0150),
/// keyed by the host-minted correlation id `ClaimResult` echoes.
pub struct ControlCore {
    snapshot: Snapshot,
    pending: BTreeMap<String, VecDeque<Pending>>,
    pending_claims: BTreeMap<u64, PendingClaim>,
}

#[actor]
impl WasmActor for ControlCore {
    const NAMESPACE: &'static str = "aether.bloomery.control";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { snapshot: Snapshot::default(), pending: BTreeMap::new(), pending_claims: BTreeMap::new() })
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
        if self.inflight_for_key(&key) >= MAX_INFLIGHT_PER_KEY {
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
        // A locally-accepted seal/supersession must first win the shared claim
        // refs — the cross-instance exclusivity gate (ADR-0150 §The claim
        // registry) — before its durable commit: a seal acquires its member +
        // admission refs, a supersession transfers the predecessor's carried +
        // admission refs and fresh-acquires net-new. The reply gates the commit
        // in `on_claim_result`. A locally-rejected seal (or any non-seal fact)
        // carries no successful outcome, so it needs no ref op and commits
        // straight through — a rejected event still journals (bare append) so its
        // key is durably consumed and a replay stays a no-op.
        let claim = match (&event.fact, &decisions.outcome) {
            (Fact::Seal(spec), Outcome::Sealed(bloom)) => {
                Some(seal_claim_mail(bloom, spec).map(|mail| (SourceClaim::Seal(mail), ClaimKind::Seal)))
            }
            (Fact::Supersede { predecessor, successor }, Outcome::Superseded { .. }) => Some(
                transfer_seal_mail(&self.snapshot, predecessor, successor)
                    .map(|mail| (SourceClaim::Transfer(mail), ClaimKind::Supersede)),
            ),
            _ => None,
        };
        match claim {
            None => self.commit_admit(ctx, reply, raw, event, decisions),
            Some(Err(error)) => {
                if let Some(handle) = reply {
                    ctx.reply_to(handle, &AdmitResult::Err { error: format!("admit claim encode failed: {error}") });
                }
            }
            Some(Ok((claim, kind))) => {
                match &claim {
                    SourceClaim::Seal(mail) => ctx.send_to_named(SOURCE, mail),
                    SourceClaim::Transfer(mail) => ctx.send_to_named(SOURCE, mail),
                }
                // The host mints a correlation id for every send; the eventual
                // `ClaimResult` echoes it, so `on_claim_result` recovers this
                // exact pending admit via `in_reply_to`. `ClaimResult` carries no
                // key of its own — this is the correlation axis the store's
                // echoed idempotency key is for the commit path.
                let correlation = ctx.prev_correlation();
                self.pending_claims.insert(correlation, PendingClaim { reply, raw, event, decisions, kind });
            }
        }
    }

    /// The `aether.source` claim/transfer/release reply. Correlate on the
    /// host-echoed correlation id (`ClaimResult` carries no key of its own). A
    /// gated admit's [`ClaimResult::Acquired`] proceeds to the durable commit;
    /// its [`ClaimResult::Held`] refuses the admit with the matching
    /// `SealError`/`SupersedeError` (never committing, so a transient foreign
    /// hold is retryable under a fresh key); a [`ClaimResult::Err`] fails it.
    /// An uncorrelated reply — a fire-and-forget release or a boot-reconcile
    /// re-assertion — has no pending entry and is ignored, like the store's
    /// stray-reply path.
    #[handler::manual]
    fn on_claim_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: ClaimResult) {
        let Some(correlation) = ctx.in_reply_to().map(|request| request.0) else {
            return;
        };
        let Some(PendingClaim { reply, raw, event, decisions, kind }) = self.pending_claims.remove(&correlation) else {
            return;
        };
        match mail {
            ClaimResult::Acquired => self.commit_admit(ctx, reply, raw, event, decisions),
            ClaimResult::Held { ref_kind, held_by } => {
                let (Ok(ref_kind), Ok(held_by)) =
                    (from_bytes::<ClaimRefKind>(&ref_kind), from_bytes::<BloomId>(&held_by))
                else {
                    if let Some(handle) = reply {
                        ctx.reply_to(handle, &AdmitResult::Err { error: "claim held-reply did not decode".to_owned() });
                    }
                    return;
                };
                let refusal = match kind {
                    ClaimKind::Seal => Some(Outcome::SealRejected(held_to_seal_error(&ref_kind, held_by))),
                    ClaimKind::Supersede => held_to_supersede_error(&ref_kind, held_by).map(Outcome::SupersedeRejected),
                };
                if let Some(handle) = reply {
                    match refusal {
                        Some(outcome) => ctx.reply_to(handle, &admit_ok(&outcome)),
                        // A lost admission-ref transfer CAS is a concurrent
                        // mutation, not a clean logical supersede refusal — no
                        // `SupersedeError` names it, so surface it for retry.
                        None => ctx.reply_to(
                            handle,
                            &AdmitResult::Err {
                                error: "supersede refused: the mainline-admission claim ref moved under a \
                                        concurrent mutation; retry"
                                    .to_owned(),
                            },
                        ),
                    }
                }
            }
            ClaimResult::Err { error } => {
                if let Some(handle) = reply {
                    ctx.reply_to(handle, &AdmitResult::Err { error: format!("claim op failed: {error}") });
                }
            }
        }
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
                // A durably-landed bloom frees its member + admission claim refs
                // (ADR-0150 §The claim registry) — release with the local
                // release, fire-and-forget: the boot reconcile re-releases any ref
                // an interrupted release stranded (it re-issues `release_seal` for
                // every `Landed` bloom, idempotent via the CAS read-guard). An
                // encode failure or a land carrying no release effect leaves the
                // refs for that reconcile — never a torn reply to the admitter,
                // whose land already committed.
                if matches!(decisions.outcome, Outcome::Landed(_))
                    && let Some(Ok(release)) = release_seal_mail(&decisions)
                {
                    ctx.send_to_named(SOURCE, &release);
                }
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
        self.reconcile_claim_refs(ctx);
    }

    /// Fold the enumerated claim refs into the boot-reconcile deep heals (ADR-0150
    /// §The claim registry, amended PR #3556). The decode-then-plan is
    /// [`plan_heals`](super::claim_plan::plan_heals) — pure and tested against the
    /// real source capability in the `claim_reconcile` integration test — this
    /// handler only decodes the enumeration and sends what it plans. Each heal is
    /// idempotent, so its `ClaimResult` reply is discarded (an uncorrelated
    /// arrival at [`Self::on_claim_result`], ignored like the V1 re-release); a
    /// crash between the enumeration and a heal re-plans on the next boot. An
    /// enumeration or per-state decode failure is unrecoverable at boot, so it
    /// fail-fasts (ADR-0063), mirroring [`Self::on_replay_result`].
    #[handler::single]
    fn on_enumerate_claims_result(&mut self, ctx: &mut WasmCtx<'_>, mail: EnumerateClaimsResult) {
        let states = match mail {
            EnumerateClaimsResult::Ok { states } => states,
            EnumerateClaimsResult::Err { error } => {
                ctx.fatal_abort(format!("boot claim-ref enumeration failed: {error}"));
            }
        };
        let states: Vec<ClaimRefState> = match states.iter().map(|bytes| from_bytes(bytes)).collect() {
            Ok(states) => states,
            Err(error) => ctx.fatal_abort(format!("boot claim-ref enumeration: a ref state did not decode: {error}")),
        };
        for op in plan_heals(&self.snapshot, &states) {
            match op {
                Ok(HealOp::Transfer(mail)) => ctx.send_to_named(SOURCE, &mail),
                Ok(HealOp::Release(mail)) => ctx.send_to_named(SOURCE, &mail),
                Err(error) => ctx.fatal_abort(format!("boot claim-ref deep heal: ref mail did not encode: {error}")),
            }
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

impl ControlCore {
    /// The same-key admits in flight across both stages — the committed queue
    /// plus the accepted seals/supersessions still awaiting their claim reply —
    /// so the [`MAX_INFLIGHT_PER_KEY`] back-pressure the commit queue enforces
    /// cannot be dodged by piling up pre-commit ref stages for one key.
    fn inflight_for_key(&self, key: &str) -> usize {
        let queued = self.pending.get(key).map_or(0, VecDeque::len);
        let claiming = self.pending_claims.values().filter(|claim| claim.event.idempotency_key.0 == key).count();
        queued + claiming
    }

    /// Project a decided event and send its combined [`Commit`] to the store —
    /// the durable-write stage every admit reaches, whether directly (a fact
    /// with no ref op, or a rejected seal) or after winning the shared claim
    /// refs (an accepted seal/supersession, gated in [`Self::on_claim_result`]).
    /// Queues the pending admit under its idempotency key (FIFO) so
    /// [`Self::on_commit_result`] can answer the admitter on the store reply; a
    /// same-key admit enqueues behind its predecessor rather than displacing it.
    fn commit_admit(
        &mut self,
        ctx: &mut WasmCtx<'_, Manual>,
        reply: Option<ReplyHandle>,
        raw: Vec<u8>,
        event: Event,
        decisions: Decisions,
    ) {
        let key = event.idempotency_key.0.clone();
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
        self.pending.entry(key).or_default().push_back(Pending { reply, event, decisions });
        ctx.send_to_named(STORE, &commit);
    }

    /// Boot-time claim-ref reconcile (ADR-0150 §The claim registry). After
    /// replay rebuilds the snapshot, converge each bloom's refs to the holding
    /// its status implies. The per-record decision is
    /// [`reconcile_op`](super::claim_plan::reconcile_op) — pure and tested
    /// against the real source capability in `aether-bloomery-host`'s
    /// `claim_reconcile` integration test — this walk only sends what it plans.
    /// Both ops are idempotent via the source's CAS read-guard, so a crash
    /// between a ref op and its local commit heals on the next boot; a ref-mail
    /// encode failure is unrecoverable at boot, so it fail-fasts (ADR-0063),
    /// mirroring the record-decode handling [`Self::on_replay_result`] uses on
    /// the same boot path — never a silent `.ok()` drop that would strand the
    /// ref.
    fn reconcile_claim_refs(&mut self, ctx: &mut WasmCtx<'_>) {
        for record in self.snapshot.blooms.values() {
            match reconcile_op(record) {
                None => {}
                Some(Ok(ReconcileOp::Assert(mail))) => ctx.send_to_named(SOURCE, &mail),
                Some(Ok(ReconcileOp::Release(mail))) => ctx.send_to_named(SOURCE, &mail),
                Some(Err(error)) => ctx.fatal_abort(format!(
                    "boot claim-ref reconcile: bloom {:?} ref mail did not encode: {error}",
                    record.spec.id()
                )),
            }
        }
        // Then drive the deep heals (ADR-0150 §The claim registry, amended PR
        // #3556): enumerate every live ref so [`Self::on_enumerate_claims_result`]
        // can sweep tombstones and finish half-transfers the per-bloom V1 walk
        // above cannot see. Fire-and-forget: the reply routes back to that handler.
        ctx.send_to_named(SOURCE, &EnumerateClaims);
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
