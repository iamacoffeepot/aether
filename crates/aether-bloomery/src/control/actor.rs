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
    Admit, AdmitResult, ClaimSeal, ClaimSealResult, Commit, CommitResult, MembershipMutation, OutboxPayload, Query,
    QueryResult, ReleaseSeal, ReplayJournal, ReplayJournalResult,
};
use crate::ids::{BloomId, WorkpieceId};
use crate::port::{ClaimOutcome, ClaimRefKind};
use crate::reduce::{
    BloomStatus, Decision, Decisions, Event, Fact, Outcome, SealConflict, SealError, Snapshot, reduce, view_of,
};
use aether_actor::{
    ActorInitError, MailSender, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_data::wire::{Error as WireError, from_bytes, to_vec};
use alloc::vec::Vec;
use std::collections::BTreeMap;

/// The runtime name of the store capability the control core drives.
const STORE: &str = "aether.store";

/// The runtime name of the native source capability the control core reaches
/// for the shared seal-claim refs (ADR-0150, #3513). Addressed by name for the
/// same reason as [`STORE`]: its capability type lives in `aether-bloomery-host`
/// (which depends on this crate), so a typed mailbox would be a package cycle.
const SOURCE: &str = "aether.source";

/// The outbox topic a landing receipt enqueues under, so #3499's republisher
/// can route it.
const RECEIPT_TOPIC: &str = "aether.bloomery.landing_receipt";

/// The correlation key the boot ref-reconcile stamps on its re-assert
/// [`ClaimSeal`]s. NUL-prefixed so it can never equal a real admit's
/// [`IdempotencyKey`](crate::ids::IdempotencyKey): the reconcile reply lands at
/// [`ControlCore::on_claim_result`], finds no waiting seal under this key, and
/// is ignored — the re-assert is fire-and-forget.
const RECONCILE_KEY: &str = "\u{0}bloomery.reconcile";

/// What to do with the shared claim refs once a commit settles (ADR-0150).
enum PostCommit {
    /// No ref side effect (integrate / resolve / a rejected admit).
    None,
    /// A land or supersede: release these `(bloom, workpieces)` refs once the
    /// commit durably lands, freeing the mainline / the predecessor's claims.
    ReleaseOnApplied(Vec<(BloomId, Vec<WorkpieceId>)>),
    /// A seal whose refs were already acquired: roll them back only if the
    /// commit does *not* apply, so a store-backstop conflict leaves no dangling
    /// ref (a crash between acquire and commit is healed by the boot reconcile).
    RollbackSealOnFailure(BloomId, Vec<WorkpieceId>),
}

/// An admit awaiting its durable commit reply — the reply handle to answer, the
/// decoded event, the decisions to apply to the snapshot once the store
/// confirms the commit landed, and the claim-ref side effect to run on the
/// reply.
struct Pending {
    reply: Option<ReplyHandle>,
    event: Event,
    decisions: Decisions,
    post_commit: PostCommit,
}

/// A seal awaiting its shared-claim acquire reply (ADR-0150) — the admit's
/// reply handle, its event/decisions, and the bloom + workpieces the acquire
/// took, held until [`ControlCore::on_claim_result`] either proceeds to the
/// commit (on `Acquired`) or refuses the seal (on `Held`).
struct Claiming {
    reply: Option<ReplyHandle>,
    event: Event,
    decisions: Decisions,
    bloom: BloomId,
    workpieces: Vec<WorkpieceId>,
}

/// The control-core actor: the live [`Snapshot`], the in-flight admits awaiting
/// their commit replies, and the seals awaiting their shared-claim acquire
/// replies.
pub struct ControlCore {
    snapshot: Snapshot,
    pending: BTreeMap<String, Pending>,
    claiming: BTreeMap<String, Claiming>,
}

#[actor]
impl WasmActor for ControlCore {
    const NAMESPACE: &'static str = "aether.bloomery.control";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { snapshot: Snapshot::default(), pending: BTreeMap::new(), claiming: BTreeMap::new() })
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
    #[allow(clippy::needless_pass_by_value)]
    #[handler::manual]
    fn on_admit(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: Admit) {
        let reply = ctx.reply_target();
        let event: Event = match from_bytes(&mail.event) {
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
                ctx.reply_to(handle, &admit_ok(&decisions.outcome));
            }
            return;
        }
        // A seal that passed the local admission screen acquires the shared
        // exclusivity refs *before* it commits (ADR-0150): the per-member claim
        // refs and the mainline-admission ref. On the acquire reply the seal
        // either proceeds to the commit (`Acquired`) or is refused with the
        // cross-instance form of its `SealError` (`Held`). Every other fact —
        // and a seal the local screen already rejected — commits directly.
        if let (Fact::Seal(spec), Outcome::Sealed(bloom)) = (&event.fact, &decisions.outcome) {
            let bloom = *bloom;
            let workpieces: Vec<WorkpieceId> = spec.members().iter().map(|member| member.workpiece.clone()).collect();
            let key = event.idempotency_key.0.clone();
            let request = match encode_claim(&key, &bloom, &workpieces) {
                Ok(request) => request,
                Err(error) => {
                    if let Some(handle) = reply {
                        ctx.reply_to(handle, &AdmitResult::Err { error: format!("claim encode failed: {error}") });
                    }
                    return;
                }
            };
            if let Some(displaced) = self.claiming.insert(key, Claiming { reply, event, decisions, bloom, workpieces })
                && let Some(handle) = displaced.reply
            {
                ctx.reply_to(handle, &superseded());
            }
            ctx.send_to_named(SOURCE, &request);
            return;
        }
        // A land / supersede releases its freed refs once the commit lands.
        let post_commit = match (&event.fact, &decisions.outcome) {
            (Fact::Land { .. }, Outcome::Landed(_)) | (Fact::Supersede { .. }, Outcome::Superseded { .. }) => {
                PostCommit::ReleaseOnApplied(release_targets(&decisions))
            }
            _ => PostCommit::None,
        };
        self.commit(ctx, reply, event, decisions, post_commit);
    }

    /// The source cap's reply to a [`ClaimSeal`]. Correlate on the echoed key,
    /// and either proceed to the commit (`Acquired`) — rolling the acquired refs
    /// back if that commit later fails — or refuse the seal with the
    /// cross-instance `SealError` a `Held` names, without committing anything.
    #[handler::manual]
    fn on_claim_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: ClaimSealResult) {
        let key = claim_key(&mail).to_owned();
        let Some(Claiming { reply, event, decisions, bloom, workpieces }) = self.claiming.remove(&key) else {
            return;
        };
        match mail {
            ClaimSealResult::Err { error, .. } => {
                if let Some(handle) = reply {
                    ctx.reply_to(handle, &AdmitResult::Err { error: format!("seal claim acquire failed: {error}") });
                }
            }
            ClaimSealResult::Ok { outcome, .. } => match from_bytes::<ClaimOutcome>(&outcome) {
                Err(error) => {
                    if let Some(handle) = reply {
                        ctx.reply_to(
                            handle,
                            &AdmitResult::Err { error: format!("claim outcome decode failed: {error}") },
                        );
                    }
                }
                Ok(ClaimOutcome::Held { ref_kind, held_by }) => {
                    // A ref another instance holds → the seal is refused with the
                    // exact `SealError` the local screen would raise, so a
                    // cross-instance refusal reads like a local one.
                    let outcome = Outcome::SealRejected(seal_error_from(ref_kind, held_by));
                    if let Some(handle) = reply {
                        ctx.reply_to(handle, &admit_ok(&outcome));
                    }
                }
                Ok(ClaimOutcome::Acquired) => {
                    self.commit(ctx, reply, event, decisions, PostCommit::RollbackSealOnFailure(bloom, workpieces));
                }
            },
        }
    }

    /// The store's reply to a [`Commit`]. Correlate on the echoed idempotency
    /// key, apply the decision to the snapshot only when the commit durably
    /// landed, and reply the reducer outcome to the original admitter.
    #[handler::manual]
    fn on_commit_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: CommitResult) {
        let key = commit_key(&mail).to_owned();
        let Some(Pending { reply, event, decisions, post_commit }) = self.pending.remove(&key) else {
            // No admit is waiting on this key — a stray or double reply.
            return;
        };
        let applied = matches!(mail, CommitResult::Applied { .. });
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
        // Run the claim-ref side effect the fact earned. A land / supersede that
        // durably landed releases its freed refs; a seal whose commit did *not*
        // apply rolls back the refs it acquired so no ref outlives its bloom.
        match post_commit {
            PostCommit::ReleaseOnApplied(targets) if applied => {
                for (bloom, workpieces) in targets {
                    send_release(ctx, &bloom, &workpieces);
                }
            }
            PostCommit::RollbackSealOnFailure(bloom, workpieces) if !applied => {
                send_release(ctx, &bloom, &workpieces);
            }
            _ => {}
        }
        if let Some(handle) = reply {
            ctx.reply_to(handle, &result);
        }
    }

    /// The source cap's reply to a [`ReleaseSeal`]. Nothing to do — a release is
    /// best-effort and idempotent; a failure only means the ref re-drives on the
    /// next release or is reclaimed by the boot reconcile. (The guest crate
    /// carries no `tracing`, so the error is not surfaced here; the ref-level
    /// truth is auditable via the bloom id the ref carries.)
    #[allow(clippy::needless_pass_by_value, clippy::unused_self)]
    #[handler::single]
    fn on_release_result(&mut self, _ctx: &mut WasmCtx<'_>, _mail: super::ReleaseSealResult) {}

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
    /// Re-assert the shared claim refs of every active (sealed-unlanded) bloom
    /// at boot (ADR-0150 §The ref ↔ local-store split): after journal replay
    /// rebuilds the snapshot, each active bloom idempotently re-creates its
    /// per-member + admission refs, healing any a crash dropped between the
    /// acquire and the commit — so no owned bloom lacks its refs. Fire-and-
    /// forget: the re-assert reply lands at [`Self::on_claim_result`] under
    /// [`RECONCILE_KEY`], matches no waiting seal, and is ignored. Reclaiming an
    /// *orphan* ref — one no active bloom owns — needs a claim-ref enumeration
    /// the source port does not yet expose; ADR-0150 leaves that cross-instance
    /// staleness to the deferred coordination service.
    fn reconcile_claim_refs(&self, ctx: &mut WasmCtx<'_>) {
        for (bloom, record) in &self.snapshot.blooms {
            if record.status != BloomStatus::Sealed {
                continue;
            }
            let workpieces: Vec<WorkpieceId> =
                record.spec.members().iter().map(|member| member.workpiece.clone()).collect();
            if let Ok(request) = encode_claim(RECONCILE_KEY, bloom, &workpieces) {
                ctx.send_to_named(SOURCE, &request);
            }
        }
    }

    /// Project the decisions, stash the pending admit, and send the combined
    /// [`Commit`] to the store — the shared tail for a direct admit and for a
    /// seal that has just acquired its shared claim refs. `post_commit` is the
    /// claim-ref side effect [`Self::on_commit_result`] runs on the reply.
    fn commit(
        &mut self,
        ctx: &mut WasmCtx<'_, Manual>,
        reply: Option<ReplyHandle>,
        event: Event,
        decisions: Decisions,
        post_commit: PostCommit,
    ) {
        let key = event.idempotency_key.0.clone();
        // Re-encode the event to its canonical wire bytes (the durable replay
        // source). Canonical encoding is deterministic, so this reproduces the
        // admitted bytes; an encode failure rolls back an acquired seal claim.
        let raw = match to_vec(&event) {
            Ok(raw) => raw,
            Err(error) => return fail_commit(ctx, reply, post_commit, format!("admit encode failed: {error}")),
        };
        // Projecting the decision encodes each outbox receipt; a receipt-encode
        // failure must reject the admit, not commit an empty payload the
        // republisher would later route as a valid-but-blank receipt.
        let (releases, claims, outbox) = match project(&decisions) {
            Ok(effects) => effects,
            Err(error) => {
                return fail_commit(ctx, reply, post_commit, format!("admit receipt encode failed: {error}"));
            }
        };
        let commit = Commit { idempotency_key: key.clone(), event: raw, releases, claims, outbox };
        // A second admit with the same key while the first is still in flight
        // would silently displace the first's pending entry — answer the
        // displaced admitter rather than dropping it.
        if let Some(displaced) = self.pending.insert(key, Pending { reply, event, decisions, post_commit })
            && let Some(handle) = displaced.reply
        {
            ctx.reply_to(handle, &superseded());
        }
        ctx.send_to_named(STORE, &commit);
    }
}

/// Reply the commit-setup failure and roll back an acquired seal claim so no
/// ref is stranded (a land / supersede release has not run yet, so there is
/// nothing to undo for those).
fn fail_commit(ctx: &mut WasmCtx<'_, Manual>, reply: Option<ReplyHandle>, post_commit: PostCommit, error: String) {
    if let Some(handle) = reply {
        ctx.reply_to(handle, &AdmitResult::Err { error });
    }
    if let PostCommit::RollbackSealOnFailure(bloom, workpieces) = post_commit {
        send_release(ctx, &bloom, &workpieces);
    }
}

/// Send a best-effort [`ReleaseSeal`] for `(bloom, workpieces)`. A wire-encode
/// failure is dropped rather than surfaced (the guest carries no `tracing`):
/// the ref it would have released is reclaimed by the boot reconcile, so a lost
/// release cannot strand a claim permanently.
fn send_release(ctx: &mut WasmCtx<'_, Manual>, bloom: &BloomId, workpieces: &[WorkpieceId]) {
    if let Ok(request) = encode_release(bloom, workpieces) {
        ctx.send_to_named(SOURCE, &request);
    }
}

/// The idempotency key echoed on every [`ClaimSealResult`] variant (the
/// correlation axis for a name-addressed claim reply).
fn claim_key(result: &ClaimSealResult) -> &str {
    match result {
        ClaimSealResult::Ok { idempotency_key, .. } | ClaimSealResult::Err { idempotency_key, .. } => idempotency_key,
    }
}

/// Map a [`ClaimOutcome::Held`] conflict into the local [`SealError`] the admit
/// reports, so a cross-instance refusal reads exactly as a local one: a
/// per-workpiece ref → `MembershipConflict`, the mainline-admission ref →
/// `ActiveBloomExists`.
fn seal_error_from(ref_kind: ClaimRefKind, held_by: BloomId) -> SealError {
    match ref_kind {
        ClaimRefKind::Workpiece(workpiece) => SealError::MembershipConflict(SealConflict { workpiece, held_by }),
        ClaimRefKind::MainlineAdmission => SealError::ActiveBloomExists(held_by),
    }
}

/// Group a decision's `ReleaseMembership` effects into per-bloom workpiece lists
/// — the refs a land / supersede frees.
fn release_targets(decisions: &Decisions) -> Vec<(BloomId, Vec<WorkpieceId>)> {
    let mut by_bloom: BTreeMap<BloomId, Vec<WorkpieceId>> = BTreeMap::new();
    for effect in &decisions.effects {
        if let Decision::ReleaseMembership { workpiece, bloom } = effect {
            by_bloom.entry(*bloom).or_default().push(workpiece.clone());
        }
    }
    by_bloom.into_iter().collect()
}

/// Encode a [`ClaimSeal`] request: the correlation key plus the wire bloom id
/// and one wire workpiece id per member.
fn encode_claim(key: &str, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<ClaimSeal, WireError> {
    Ok(ClaimSeal {
        idempotency_key: key.to_owned(),
        bloom: to_vec(bloom)?,
        workpieces: workpieces.iter().map(to_vec).collect::<Result<Vec<_>, _>>()?,
    })
}

/// Encode a [`ReleaseSeal`] request.
fn encode_release(bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<ReleaseSeal, WireError> {
    Ok(ReleaseSeal { bloom: to_vec(bloom)?, workpieces: workpieces.iter().map(to_vec).collect::<Result<Vec<_>, _>>()? })
}

/// The reply for an admit displaced by a concurrent admit with the same key.
fn superseded() -> AdmitResult {
    AdmitResult::Err { error: "superseded by a concurrent admit with the same idempotency key".to_owned() }
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
            Decision::InheritClaim { .. }
            | Decision::RecordResolution { .. }
            | Decision::MarkSuperseded { .. }
            | Decision::SetResolved { .. }
            | Decision::AdvanceMainline { .. } => {}
        }
    }
    Ok((releases, claims, outbox))
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::digest::Digest;
    use crate::ids::{BloomId, WorkpieceId};
    use crate::port::ClaimRefKind;
    use crate::reduce::{Decision, Decisions, Outcome, SealError};

    use super::{release_targets, seal_error_from};

    fn bloom(seed: u8) -> BloomId {
        BloomId(Digest::from_bytes([seed; 32]))
    }

    fn workpiece(name: &str) -> WorkpieceId {
        WorkpieceId(name.to_owned())
    }

    #[test]
    fn a_workpiece_ref_conflict_maps_to_membership_conflict() {
        // The cross-instance refusal must read exactly as the local one: a held
        // per-workpiece claim ref is a `MembershipConflict` naming the workpiece
        // and the holding bloom.
        let held_by = bloom(9);
        let error = seal_error_from(ClaimRefKind::Workpiece(workpiece("reactor-core")), held_by);
        match error {
            SealError::MembershipConflict(conflict) => {
                assert_eq!(conflict.workpiece, workpiece("reactor-core"));
                assert_eq!(conflict.held_by, held_by);
            }
            other => panic!("expected MembershipConflict, got {other:?}"),
        }
    }

    #[test]
    fn an_admission_ref_conflict_maps_to_active_bloom_exists() {
        let held_by = bloom(9);
        assert_eq!(
            seal_error_from(ClaimRefKind::MainlineAdmission, held_by),
            SealError::ActiveBloomExists(held_by),
            "a held mainline-admission ref is the cross-instance one-bloom-per-mainline refusal",
        );
    }

    #[test]
    fn release_targets_group_release_membership_effects_by_bloom() {
        // A land / supersede releases exactly the workpieces its
        // `ReleaseMembership` effects name, grouped per bloom — the refs
        // `send_release` then deletes. Non-release effects contribute nothing.
        let landed = bloom(1);
        let decisions = Decisions {
            outcome: Outcome::Duplicate,
            effects: vec![
                Decision::ReleaseMembership { workpiece: workpiece("a"), bloom: landed },
                Decision::ReleaseMembership { workpiece: workpiece("b"), bloom: landed },
                Decision::AdvanceMainline { from: Digest::from_bytes([0; 32]), to: Digest::from_bytes([1; 32]) },
            ],
        };

        let targets = release_targets(&decisions);

        assert_eq!(targets, vec![(landed, vec![workpiece("a"), workpiece("b")])]);
    }
}
