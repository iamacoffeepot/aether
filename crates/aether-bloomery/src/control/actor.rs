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
    Admit, AdmitResult, ClaimResult, ClaimSeal, Commit, CommitResult, MembershipMutation, OutboxPayload, Query,
    QueryResult, ReleaseSeal, ReplayJournal, ReplayJournalResult, TransferSeal,
};
use crate::digest::Digest;
use crate::ids::{BloomId, WorkpieceId};
use crate::port::ClaimRefKind;
use crate::reduce::{
    Decision, Decisions, Event, Fact, Outcome, SealConflict, SealError, Snapshot, SupersedeError, is_active_unlanded,
    reduce, view_of,
};
use crate::values::BloomSpec;
use aether_actor::{
    ActorInitError, MailSender, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_data::wire::{Error as WireError, from_bytes, to_vec};
use std::collections::{BTreeMap, BTreeSet};

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

/// The outbox topic a landing receipt enqueues under, so #3499's republisher
/// can route it.
const RECEIPT_TOPIC: &str = "aether.bloomery.landing_receipt";

/// The outbox topic a stage re-dispatch enqueues under, so the dispatch consumer
/// (#3505) re-assembles the held attempt naming both question and answer digests
/// (ADR-0151). Producer-only here, like [`RECEIPT_TOPIC`].
const REDISPATCH_TOPIC: &str = "aether.bloomery.redispatch";

/// An admit awaiting its durable commit reply — the reply handle to answer, the
/// decoded event, and the decisions to apply to the snapshot once the store
/// confirms the commit landed.
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

/// The control-core actor: the live [`Snapshot`], the in-flight admits awaiting
/// their durable commit replies, and the accepted seals/supersessions awaiting
/// their shared claim-ref replies (the pre-commit exclusivity gate, ADR-0150).
pub struct ControlCore {
    snapshot: Snapshot,
    pending: BTreeMap<String, Pending>,
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
                // The host mints a correlation id for every send (universal
                // correlation); the eventual `ClaimResult` echoes it, so the
                // reply handler recovers this exact pending admit via
                // `in_reply_to`. `ClaimResult` carries no key of its own — this
                // is the correlation axis the store's echoed idempotency key is
                // for the commit path.
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
                // A durably-landed bloom frees its member + admission claim refs
                // (ADR-0150 §The claim registry) — release with the local
                // release, fire-and-forget: the boot reconcile re-asserts any ref
                // an interrupted release stranded.
                // An encode failure or a land carrying no release effect leaves
                // the refs for the boot reconcile — never a torn reply to the
                // admitter, whose land already committed.
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
    /// Project a decided event and send its combined [`Commit`] to the store —
    /// the durable-write stage every admit reaches, whether directly (a fact
    /// with no ref op, or a rejected seal) or after winning the shared claim
    /// refs (an accepted seal/supersession, gated in [`Self::on_claim_result`]).
    /// Stashes the pending admit keyed by idempotency key so
    /// [`Self::on_commit_result`] can answer the admitter on the store reply.
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
        let commit = Commit { idempotency_key: key.clone(), event: raw, releases, claims, outbox };
        // A second admit with the same key while the first is still in flight
        // would silently displace the first's pending entry, stranding its
        // admitter with no reply — answer the displaced admitter rather than
        // dropping it.
        if let Some(displaced) = self.pending.insert(key, Pending { reply, event, decisions })
            && let Some(handle) = displaced.reply
        {
            ctx.reply_to(
                handle,
                &AdmitResult::Err {
                    error: "superseded by a concurrent admit with the same idempotency key".to_owned(),
                },
            );
        }
        ctx.send_to_named(STORE, &commit);
    }

    /// Boot-time claim-ref reconcile (ADR-0150 §The claim registry). After
    /// replay rebuilds the snapshot, re-assert every active-and-unlanded
    /// (`Sealed | Resolved`, via the [`is_active_unlanded`] predicate the seal
    /// guard shares, so the two never drift) bloom's member + admission claim
    /// refs by re-sending `claim_seal` — fire-and-forget, so a crash between a
    /// ref op and the local commit converges. `claim_seal` is the idempotent
    /// lever this mail surface offers: an intact holding rolls its own create
    /// back to a no-op (the refs already name this bloom), a fully-lost ref set
    /// is re-acquired. Deeper heals a wasm actor cannot drive over
    /// claim/transfer/release alone — reclaiming a foreign-held orphan, sweeping
    /// a tombstoned ref, healing a half-transfer — stay the deferred
    /// coordination service's job (ADR-0150 notes cross-instance staleness is
    /// not self-healing). At most one bloom is active (the V1 one-per-mainline
    /// invariant), so this is bounded.
    fn reconcile_claim_refs(&mut self, ctx: &mut WasmCtx<'_>) {
        let reassert: Vec<ClaimSeal> = self
            .snapshot
            .blooms
            .values()
            .filter(|record| is_active_unlanded(record.status))
            .filter_map(|record| seal_claim_mail(&record.spec.id(), &record.spec).ok())
            .collect();
        for mail in &reassert {
            ctx.send_to_named(SOURCE, mail);
        }
    }
}

/// Encode each workpiece to its canonical [`aether_data::wire`] bytes — the
/// per-member axis the `aether.source.*` claim mail carries (the port value
/// types are serde-encoded but not `Schema`).
fn encode_workpieces<'a>(workpieces: impl Iterator<Item = &'a WorkpieceId>) -> Result<Vec<Vec<u8>>, WireError> {
    workpieces.map(to_vec).collect()
}

/// The `claim_seal` mail for an accepted seal (also the boot-reconcile
/// re-assertion): acquire the member refs plus the mainline-admission ref (the
/// host adds the admission ref from the bloom, ADR-0150).
fn seal_claim_mail(bloom: &BloomId, spec: &BloomSpec) -> Result<ClaimSeal, WireError> {
    Ok(ClaimSeal {
        bloom: to_vec(bloom)?,
        workpieces: encode_workpieces(spec.members().iter().map(|member| &member.workpiece))?,
    })
}

/// The `transfer_seal` mail for an accepted supersession: partition the
/// successor's members against the predecessor's (mirroring `reduce_supersede`)
/// — carried = shared, net-new = successor-only, dropped = predecessor-only —
/// so the predecessor's carried + admission refs fast-forward to the successor
/// (the admission ref never momentarily free), net-new refs are fresh-acquired,
/// and dropped refs released. The predecessor is in the snapshot (the reducer
/// accepted the supersession), so a missing record yields empty carried/dropped
/// sets rather than a panic.
fn transfer_seal_mail(
    snapshot: &Snapshot,
    predecessor: &BloomId,
    successor: &BloomSpec,
) -> Result<TransferSeal, WireError> {
    let predecessor_members: BTreeSet<&WorkpieceId> = snapshot
        .blooms
        .get(predecessor)
        .map(|record| record.spec.members().iter().map(|member| &member.workpiece).collect())
        .unwrap_or_default();
    let successor_members: BTreeSet<&WorkpieceId> =
        successor.members().iter().map(|member| &member.workpiece).collect();
    let (carried, net_new, dropped) = partition_members(&predecessor_members, &successor_members);
    Ok(TransferSeal {
        predecessor: to_vec(predecessor)?,
        successor: to_vec(&successor.id())?,
        carried: encode_workpieces(carried.into_iter())?,
        net_new: encode_workpieces(net_new.into_iter())?,
        dropped: encode_workpieces(dropped.into_iter())?,
    })
}

/// Partition a supersession's members into the transfer axes, mirroring
/// `reduce_supersede`'s membership handling: `carried` = shared (fast-forwarded
/// predecessor→successor), `net_new` = successor-only (fresh-acquired),
/// `dropped` = predecessor-only (released). Sorted by the `BTreeSet` iteration,
/// so the mail is deterministic.
fn partition_members<'a>(
    predecessor: &BTreeSet<&'a WorkpieceId>,
    successor: &BTreeSet<&'a WorkpieceId>,
) -> (Vec<&'a WorkpieceId>, Vec<&'a WorkpieceId>, Vec<&'a WorkpieceId>) {
    let carried = successor.iter().copied().filter(|w| predecessor.contains(w)).collect();
    let net_new = successor.iter().copied().filter(|w| !predecessor.contains(w)).collect();
    let dropped = predecessor.iter().copied().filter(|w| !successor.contains(w)).collect();
    (carried, net_new, dropped)
}

/// The `release_seal` mail for a durably-landed bloom: release the member refs
/// the land freed (the [`Decision::ReleaseMembership`] effects) plus the
/// admission ref (the host adds it). A land carries a release per member, all
/// naming the landed bloom; `None` if a land somehow carries none.
fn release_seal_mail(decisions: &Decisions) -> Option<Result<ReleaseSeal, WireError>> {
    let mut bloom = None;
    let mut workpieces = Vec::new();
    for effect in &decisions.effects {
        if let Decision::ReleaseMembership { workpiece, bloom: released_from } = effect {
            bloom = Some(*released_from);
            workpieces.push(workpiece);
        }
    }
    let bloom = bloom?;
    Some((|| Ok(ReleaseSeal { bloom: to_vec(&bloom)?, workpieces: encode_workpieces(workpieces.into_iter())? }))())
}

/// Map a seal's [`ClaimResult::Held`] to the matching [`SealError`] — a
/// per-workpiece ref is a membership conflict, the mainline-admission ref is
/// the one-active-bloom-per-mainline conflict — so a cross-instance refusal
/// reads exactly as the local reducer's own (ADR-0150).
fn held_to_seal_error(ref_kind: &ClaimRefKind, held_by: BloomId) -> SealError {
    match ref_kind {
        ClaimRefKind::Workpiece(workpiece) => {
            SealError::MembershipConflict(SealConflict { workpiece: workpiece.clone(), held_by })
        }
        ClaimRefKind::MainlineAdmission => SealError::ActiveBloomExists(held_by),
    }
}

/// Map a supersession's [`ClaimResult::Held`] to a [`SupersedeError`]. A
/// foreign hold on a net-new member is a membership conflict (mirroring
/// `reduce_supersede`'s `membership_conflict`); a lost admission-ref transfer
/// CAS has no clean `SupersedeError` (it is a concurrent mutation, not a
/// logical refusal), so it returns `None` and the caller surfaces a retryable
/// error instead.
fn held_to_supersede_error(ref_kind: &ClaimRefKind, held_by: BloomId) -> Option<SupersedeError> {
    match ref_kind {
        ClaimRefKind::Workpiece(workpiece) => {
            Some(SupersedeError::MembershipConflict(SealConflict { workpiece: workpiece.clone(), held_by }))
        }
        ClaimRefKind::MainlineAdmission => None,
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

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeSet;

    use crate::digest::Digest;
    use crate::ids::{BloomId, WorkpieceId};
    use crate::port::ClaimRefKind;
    use crate::reduce::{SealConflict, SealError, SupersedeError};

    use super::{held_to_seal_error, held_to_supersede_error, partition_members};

    fn workpiece(name: &str) -> WorkpieceId {
        WorkpieceId(name.into())
    }

    #[test]
    fn partition_splits_supersede_members_into_carried_net_new_and_dropped() {
        // A supersession keeping w2, adding w3, dropping w1: the transfer must
        // carry the shared ref, fresh-acquire the successor-only one, and release
        // the predecessor-only one — the axes `transfer_seal` fast-forwards,
        // creates, and releases respectively (ADR-0150). A swap here would free a
        // still-claimed workpiece or leave a dropped one claimed.
        let (w1, w2, w3) = (workpiece("w1"), workpiece("w2"), workpiece("w3"));
        let predecessor: BTreeSet<&WorkpieceId> = [&w1, &w2].into_iter().collect();
        let successor: BTreeSet<&WorkpieceId> = [&w2, &w3].into_iter().collect();
        let (carried, net_new, dropped) = partition_members(&predecessor, &successor);
        assert_eq!(carried, vec![&w2]);
        assert_eq!(net_new, vec![&w3]);
        assert_eq!(dropped, vec![&w1]);
    }

    #[test]
    fn held_maps_to_the_matching_seal_refusal_vocabulary() {
        // The cross-instance refusal must read exactly as the local reducer's: a
        // per-workpiece hold is a membership conflict, the mainline-admission hold
        // is the one-active-bloom refusal (ADR-0150). Mapping admission to a
        // membership conflict (or vice versa) would misreport the reason.
        let held_by = BloomId(Digest::of_wire_bytes(b"holder"));
        let wp = workpiece("w1");
        assert_eq!(
            held_to_seal_error(&ClaimRefKind::Workpiece(wp.clone()), held_by),
            SealError::MembershipConflict(SealConflict { workpiece: wp, held_by }),
        );
        assert_eq!(
            held_to_seal_error(&ClaimRefKind::MainlineAdmission, held_by),
            SealError::ActiveBloomExists(held_by)
        );
    }

    #[test]
    fn held_maps_to_the_matching_supersede_refusal_or_a_retryable_none() {
        // A foreign net-new hold is a supersede membership conflict; a lost
        // admission-ref transfer CAS has no clean SupersedeError, so it is `None`
        // (the caller surfaces a retryable error rather than forcing a wrong
        // variant).
        let held_by = BloomId(Digest::of_wire_bytes(b"holder"));
        let wp = workpiece("w1");
        assert_eq!(
            held_to_supersede_error(&ClaimRefKind::Workpiece(wp.clone()), held_by),
            Some(SupersedeError::MembershipConflict(SealConflict { workpiece: wp, held_by })),
        );
        assert_eq!(held_to_supersede_error(&ClaimRefKind::MainlineAdmission, held_by), None);
    }
}
