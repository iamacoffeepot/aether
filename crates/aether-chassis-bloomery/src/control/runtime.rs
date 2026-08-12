//! The snapshot-owning runtime of the [`ControlCore`] cap.
//!
//! # The async store choreography
//!
//! A handler cannot block on a peer reply, so an admit is a two-message
//! exchange. [`on_admit`](ControlCore::on_admit) reduces the event, projects the
//! decision into a [`Commit`], stashes the pending admit (the held reply
//! obligation, the decoded event, and the decisions) keyed by idempotency key,
//! and sends the commit to `aether.store`. Only the *first* admit for a key opens
//! that entry and forwards a commit; a same-key admit arriving while the commit is
//! still outstanding gets its own entry and its own commit — the store dedups the
//! second to a [`CommitResult::Duplicate`] no-op. The store and source caps are
//! now native siblings addressed by type (`ctx.actor::<StoreCapability>()` /
//! `ctx.actor::<SourceCapability>()`); the reply routes back by kind, correlated by
//! the echoed idempotency key ([`CommitResult`]) or the dispatch correlation id
//! ([`ClaimResult`]), exactly as the former wasm actor's name-addressed sends did.
//!
//! Boot **does not** drain or ack the outbox — outbox republish belongs to the
//! reactor capabilities (#3499). This cap only *enqueues* outbox entries,
//! atomically inside the commit.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;

use aether_actor::{Manual, runtime};
use aether_data::wire::{Error as WireError, from_bytes, to_vec};
use aether_substrate::InboundMail;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

use aether_bloomery::control::{
    Admit, AdmitResult, AggregateReviewPayload, AggregateVerifyPayload, ClaimResult, ClaimSeal, Commit, CommitResult,
    CompleteReleaseResult, DispatchPayload, EnumerateClaims, EnumerateClaimsResult, HealOp, IntegratePayload,
    LandPayload, LoadConfigs, LoadConfigsResult, MembershipMutation, ObserveMainline, ObserveMainlineResult,
    OrphanClaimReleasePayload, OutboxPayload, Query, QueryResult, ReconcileOp, RedispatchPayload, ReplayJournal,
    ReplayJournalResult, ReviewPass, Topic, TransferSeal, held_to_seal_error, held_to_supersede_error, plan_heals,
    reconcile_op, release_seal_mail, seal_claim_mail, transfer_seal_mail,
};
use aether_bloomery::{
    BloomId, ClaimRefKind, ClaimRefState, Decision, Decisions, Digest, Event, Fact, IdempotencyKey, Outcome,
    ResolvedConfigs, Snapshot, Unproducible, reduce, view_of,
};

use super::ControlCore;
use crate::source::SourceCapability;
use crate::store::StoreCapability;

/// The cap on same-key admits in flight at once. A well-behaved client sends one
/// admit per key and at most a few retries; without a bound a client spamming one
/// key while its commits are outstanding would grow the per-key queue without
/// limit, pinning memory on the single snapshot owner. An admit past the cap is
/// refused rather than queued (CLAUDE.md §Runtime: error rather than grow
/// unboundedly). The claim-gated (seal/supersede) admits awaiting a `ClaimResult`
/// count toward the same per-key cap ([`ControlCoreState::inflight_for_key`]), so
/// the pre-commit ref stage cannot dodge the back-pressure.
const MAX_INFLIGHT_PER_KEY: usize = 64;

/// An admit awaiting its durable commit reply — the held reply obligation, the
/// decoded event, and the decisions to apply to the snapshot once the store
/// confirms the commit landed. Each in-flight admit owns one, held in its key's
/// FIFO queue. The held [`InboundMail`] keeps the admit's causal chain open across
/// the async round-trip; `reply` is a no-op if the admitter did not await one.
struct Pending {
    inbound: InboundMail,
    event: Event,
    decisions: Decisions,
}

/// Which claim door an admit gated on, so a [`ClaimResult::Held`] maps to the
/// right refusal vocabulary — a seal reports a `SealError`, a supersession a
/// `SupersedeError`, each reading exactly as the local reducer's own refusal.
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
/// acquired: the held reply obligation, the original event bytes (the journal
/// source), the decoded event and its decisions, and which door to name a `Held`
/// refusal after. Keyed by the dispatch correlation id.
struct PendingClaim {
    inbound: InboundMail,
    raw: Vec<u8>,
    event: Event,
    decisions: Decisions,
    kind: ClaimKind,
}

/// An admit held while the store is re-read for configuration it named but the
/// control core did not hold. Keyed by the dispatch correlation id the
/// [`LoadConfigsResult`] echoes; the raw bytes are what the resumed admit
/// re-decodes, so the retry runs the identical path rather than a second one.
struct PendingConfigs {
    inbound: InboundMail,
    raw: Vec<u8>,
}

/// The control-core state: the live [`Snapshot`] plus the in-flight admits
/// awaiting their commit replies, queued per idempotency key.
///
/// Each same-key admit gets its own `Pending` entry and its own [`Commit`], so
/// every admit's inbound chain stays open (held by its `InboundMail`) until its
/// reply is sent. The store's idempotency-key dedup collapses every same-key
/// commit after the first to a [`CommitResult::Duplicate`] no-op, so the pair
/// still yields one journal row and one applied decision. The per-key queue is
/// FIFO: the store replies in send order, so `on_commit_result` pops the front
/// entry to match each reply.
///
/// `pending_claims` holds the accepted seals/supersessions awaiting their shared
/// claim-ref replies (the pre-commit cross-instance exclusivity gate, ADR-0150),
/// keyed by the dispatch correlation id `ClaimResult` echoes.
pub struct ControlCoreState {
    snapshot: Snapshot,
    configs: ResolvedConfigs,
    pending: BTreeMap<String, VecDeque<Pending>>,
    pending_claims: BTreeMap<u64, PendingClaim>,
    pending_configs: BTreeMap<u64, PendingConfigs>,
}

#[runtime]
impl NativeActor for ControlCore {
    type State = ControlCoreState;
    type Config = ();
    const NAMESPACE: &'static str = aether_bloomery::CONTROL_CORE_NAMESPACE;

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<ControlCoreState, BootError> {
        Ok(ControlCoreState {
            snapshot: Snapshot::default(),
            configs: ResolvedConfigs::default(),
            pending: BTreeMap::new(),
            pending_claims: BTreeMap::new(),
            pending_configs: BTreeMap::new(),
        })
    }

    /// Boot: read the stored configuration first, then replay the journal from
    /// [`on_load_configs_result`](Self::on_load_configs_result). Sequenced rather
    /// than sent together because replay *reduces* — a seal it folds back may name
    /// configuration the reducer has to produce, and a replay that ran first would
    /// refuse an event the original admit accepted, rebuilding a snapshot that
    /// never existed. Lives in `wire` (post-init, mail-allowed).
    fn wire(_state: &mut ControlCoreState, ctx: &mut NativeCtx<'_>) {
        ctx.actor::<StoreCapability>().send_detached(&LoadConfigs);
    }

    /// The `aether.bloomery.admit` ingress. Decode the event, reduce it against
    /// the live snapshot, and either reply immediately (a duplicate needs no
    /// commit) or queue an in-flight entry under its idempotency key and send the
    /// combined [`Commit`] to the store, answering on its reply. A second admit
    /// for a key whose first commit is still outstanding gets its own entry and
    /// its own commit — the store dedups the second to a [`CommitResult::Duplicate`]
    /// no-op — so its inbound chain stays open (held by its `InboundMail`) until it
    /// is answered.
    #[handler::manual]
    fn on_admit(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, mail: Admit) {
        let inbound = ctx.take_inbound();
        let raw = mail.event;
        let event: Event = match from_bytes(&raw) {
            Ok(event) => event,
            Err(error) => {
                inbound.reply(&AdmitResult::Err { error: format!("admit decode failed: {error}") });
                return;
            }
        };
        let key = event.idempotency_key.0.clone();
        // Back-pressure: cap the same-key admits in flight at once (CLAUDE.md
        // §Runtime: error rather than grow unboundedly).
        if state.inflight_for_key(&key) >= MAX_INFLIGHT_PER_KEY {
            inbound
                .reply(&AdmitResult::Err { error: "too many concurrent admits for this idempotency key".to_owned() });
            return;
        }
        // A seal naming configuration this core has not read yet is held, not
        // refused: the api cap writes an authored config straight to the store, so
        // the first seal after one is authored legitimately arrives ahead of the
        // content (ADR-0174). One re-read closes that gap. A second miss on the
        // same admit is a real absence and falls through to the reducer's refusal,
        // so a bad address cannot loop the store.
        if state.awaits_configs(&event) {
            let mail_id = ctx.actor::<StoreCapability>().send_detached_tracked(&LoadConfigs);
            state.pending_configs.insert(mail_id.correlation_id, PendingConfigs { inbound, raw });
            return;
        }
        let decisions = reduce(&state.snapshot, &event, &state.configs);
        // A duplicate key is already applied (in this process's life or rebuilt by
        // replay), so it needs no durable commit — reply immediately.
        if matches!(decisions.outcome, Outcome::Duplicate) {
            inbound.reply(&admit_duplicate(&event.idempotency_key.0));
            return;
        }
        state.gate_or_commit(ctx, inbound, raw, event, decisions);
    }

    /// Re-run a held admit now that the configuration set is refilled.
    ///
    /// The same path [`on_admit`](Self::on_admit) takes past its gate, minus the
    /// gate: a second deferral would loop the store on an address the re-read
    /// already declined to produce. The back-pressure check is not re-run either
    /// — this admit was already counted when it arrived.
    fn resume_admit(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, inbound: InboundMail, raw: Vec<u8>) {
        let event: Event = match from_bytes(&raw) {
            Ok(event) => event,
            Err(error) => {
                inbound.reply(&AdmitResult::Err { error: format!("admit decode failed: {error}") });
                return;
            }
        };
        let decisions = reduce(&state.snapshot, &event, &state.configs);
        if matches!(decisions.outcome, Outcome::Duplicate) {
            inbound.reply(&admit_duplicate(&event.idempotency_key.0));
            return;
        }
        state.gate_or_commit(ctx, inbound, raw, event, decisions);
    }

    /// The `aether.source` claim/transfer/release reply. Correlate on the echoed
    /// dispatch correlation id. A gated admit's [`ClaimResult::Acquired`] proceeds
    /// to the durable commit; its [`ClaimResult::Held`] refuses the admit with the
    /// matching `SealError`/`SupersedeError` (never committing, so a transient
    /// foreign hold is retryable under a fresh key); a [`ClaimResult::Err`] fails
    /// it. An uncorrelated reply — a fire-and-forget release or a boot-reconcile
    /// re-assertion — has no pending entry and is ignored.
    #[handler::manual]
    fn on_claim_result(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, mail: ClaimResult) {
        let correlation = ctx.reply_target().correlation_id;
        let Some(PendingClaim { inbound, raw, event, decisions, kind }) = state.pending_claims.remove(&correlation)
        else {
            return;
        };
        match mail {
            ClaimResult::Acquired => state.commit_admit(ctx, inbound, raw, event, decisions),
            ClaimResult::Held { ref_kind, held_by } => {
                let (Ok(ref_kind), Ok(held_by)) =
                    (from_bytes::<ClaimRefKind>(&ref_kind), from_bytes::<BloomId>(&held_by))
                else {
                    inbound.reply(&AdmitResult::Err { error: "claim held-reply did not decode".to_owned() });
                    return;
                };
                let refusal = match kind {
                    ClaimKind::Seal => Some(Outcome::SealRejected(held_to_seal_error(&ref_kind, held_by))),
                    ClaimKind::Supersede => held_to_supersede_error(&ref_kind, held_by).map(Outcome::SupersedeRejected),
                };
                match refusal {
                    Some(outcome) => {
                        inbound.reply(&admit_ok(&outcome));
                    }
                    // A lost admission-ref transfer CAS is a concurrent mutation,
                    // not a clean logical supersede refusal — no `SupersedeError`
                    // names it, so surface it for retry.
                    None => {
                        inbound.reply(&AdmitResult::Err {
                            error: "supersede refused: the mainline-admission claim ref moved under a concurrent \
                                    mutation; retry"
                                .to_owned(),
                        });
                    }
                }
            }
            ClaimResult::Err { error } => {
                inbound.reply(&AdmitResult::Err { error: format!("claim op failed: {error}") });
            }
        }
    }

    /// The store's reply to a [`Commit`]. Correlate on the echoed idempotency key,
    /// pop the matching key's front in-flight entry (the store replies in send
    /// order, so the queue is FIFO), apply the decision to the snapshot only when
    /// the commit durably landed, and reply the outcome to that admitter. A
    /// same-key follow-on admit's commit lands here as [`CommitResult::Duplicate`]
    /// and answers its own admitter without re-applying.
    #[handler::manual]
    fn on_commit_result(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, mail: CommitResult) {
        let key = commit_key(&mail).to_owned();
        let Some(queue) = state.pending.get_mut(&key) else {
            // No admit is waiting on this key — a stray or double reply.
            return;
        };
        let Some(Pending { inbound, event, decisions }) = queue.pop_front() else {
            return;
        };
        // Drop the key's slot once its last in-flight entry is answered.
        if queue.is_empty() {
            state.pending.remove(&key);
        }
        let result = match mail {
            CommitResult::Applied { .. } => {
                state.snapshot = state.snapshot.apply(&event, &decisions, &state.configs);
                // A durably-landed bloom frees its member + admission claim refs
                // (ADR-0150) — release with the local release, fire-and-forget: the
                // boot reconcile re-releases any ref an interrupted release stranded.
                if matches!(decisions.outcome, Outcome::Landed(_))
                    && let Some(Ok(release)) = release_seal_mail(&decisions)
                {
                    ctx.actor::<SourceCapability>().send_detached(&release);
                }
                admit_ok(&decisions.outcome)
            }
            // The store already held this key durably though our snapshot did not —
            // a rare divergence (a reply racing a concurrent replay). Reply
            // Duplicate and do not double-apply.
            CommitResult::Duplicate { .. } => admit_duplicate(&key),
            // The durable uniqueness backstop refused a claim the reducer's snapshot
            // screen missed — do not apply; report the conflict.
            CommitResult::Conflict { workpiece, .. } => {
                AdmitResult::Err { error: format!("store membership conflict on {workpiece}") }
            }
            CommitResult::Err { error, .. } => AdmitResult::Err { error },
        };
        inbound.reply(&result);
    }

    /// Boot journal replay reply: fold each record (decode the event, `reduce`,
    /// The stored-configuration read (ADR-0174). Two arrivals reach here and they
    /// resolve differently.
    ///
    /// The boot read (no held admit) fills the set and *then* sends
    /// [`ReplayJournal`], which is the ordering that makes replay reduce against
    /// the same content the original admits did. A read failure at boot is
    /// unrecoverable for the same reason a failed replay is — the snapshot it
    /// would rebuild is not the one that existed — so it fail-fasts (ADR-0063).
    ///
    /// A re-read for a held admit refills the set and re-enters
    /// [`on_admit`](Self::on_admit) with the original bytes. The retry is not
    /// gated again: `awaits_configs` is false once the content arrives, and if it
    /// did not arrive the address is genuinely absent and the reducer's own
    /// refusal names it. A failed re-read answers that admit rather than aborting
    /// the process — one operator request fails, the core stays up.
    #[handler::manual]
    fn on_load_configs_result(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, mail: LoadConfigsResult) {
        let held = state.pending_configs.remove(&ctx.reply_target().correlation_id);
        let records = match mail {
            LoadConfigsResult::Ok { records } => records,
            LoadConfigsResult::Err { error } => match held {
                Some(PendingConfigs { inbound, .. }) => {
                    inbound.reply(&AdmitResult::Err { error: format!("configuration read failed: {error}") });
                    return;
                }
                None => ctx.fatal_abort(format!("boot configuration read failed: {error}")),
            },
        };
        for record in records {
            let Some(address) = Digest::from_slice(&record.digest) else {
                ctx.fatal_abort(format!("stored configuration `{}` has a malformed address", record.kind));
            };
            state.configs.insert(address, record.kind, record.bytes);
        }

        match held {
            Some(PendingConfigs { inbound, raw }) => Self::resume_admit(state, ctx, inbound, raw),
            None => ctx.actor::<StoreCapability>().send_detached(&ReplayJournal),
        }
    }

    /// `apply`) to rebuild the snapshot. No outbox drain/ack — republish is
    /// #3499's. A read or a corrupt record at boot is unrecoverable, so it
    /// fail-fasts (ADR-0063) rather than coming up on a torn snapshot.
    #[handler::manual]
    fn on_replay_result(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, mail: ReplayJournalResult) {
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
            let decisions = reduce(&state.snapshot, &event, &state.configs);
            state.snapshot = state.snapshot.apply(&event, &decisions, &state.configs);
        }
        state.reconcile_claim_refs(ctx);
        // Only now is the snapshot the one the journal describes, so only now can
        // an observation be decided against it (#4677). Asked here rather than
        // from the source cap's own `wire` because that fires the moment the cap
        // mounts, which is *concurrent* with this replay: an observation decided
        // then reads an empty bloom map, finds nothing in flight, and advances
        // mainline out from under the very land the fold above is about to apply
        // — which then refuses as `BaseMismatch` and leaves a landed bloom
        // reading unlanded for the whole boot.
        ctx.actor::<SourceCapability>().send_detached(&ObserveMainline);
    }

    /// Admit the observed mainline head (#4667). The reply to the
    /// [`ObserveMainline`] the boot replay sends once its fold is complete
    /// (#4677), so the reducer decides it against the snapshot the journal
    /// describes rather than a partial one.
    ///
    /// A failed observation logs and continues rather than aborting boot: the
    /// coordinator is fully functional on a stale mainline — every bloom already
    /// sealed keeps its base, and the pointer catches up on the next restart —
    /// so an unreachable source is not the unrecoverable class the replay and
    /// claim-ref reconcile fail-fast on.
    ///
    /// The admit goes through the ordinary [`Admit`] door rather than an internal
    /// shortcut, so this fact is journaled, deduped, and committed exactly like
    /// one arriving over the wire.
    #[handler::manual]
    fn on_observe_mainline_result(
        _state: &mut ControlCoreState,
        ctx: &mut NativeCtx<'_, Manual>,
        mail: ObserveMainlineResult,
    ) {
        let head = match mail {
            ObserveMainlineResult::Ok { head } => head,
            ObserveMainlineResult::Err { error } => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::control",
                    %error,
                    "boot mainline observation failed; mainline stays where the last land left it until a restart re-observes"
                );
                return;
            }
        };
        let head: Digest = match from_bytes(&head) {
            Ok(head) => head,
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::control",
                    %error,
                    "boot mainline observation did not decode"
                );
                return;
            }
        };

        let event = Event {
            idempotency_key: IdempotencyKey(format!("observe-mainline-{}", lowercase_hex(head.as_bytes()))),
            fact: Fact::ObserveMainline { head },
        };
        match to_vec(&event) {
            Ok(bytes) => ctx.actor::<ControlCore>().send_detached(&Admit { event: bytes }),
            Err(error) => tracing::warn!(
                target: "aether_chassis_bloomery::control",
                %error,
                "boot mainline observation did not encode"
            ),
        }
    }

    /// Fold the enumerated claim refs into the boot-reconcile deep heals (ADR-0150
    /// §The claim registry, amended PR #3556). The decode-then-plan is
    /// [`plan_heals`] — pure and tested against the real source capability in the
    /// `claim_reconcile` integration test — this handler only decodes the
    /// enumeration and sends what it plans. Each heal is idempotent, so its
    /// `ClaimResult` reply is discarded (an uncorrelated arrival at
    /// [`on_claim_result`](Self::on_claim_result)). An enumeration or per-state
    /// decode failure is unrecoverable at boot, so it fail-fasts (ADR-0063).
    #[handler::manual]
    fn on_enumerate_claims_result(
        state: &mut ControlCoreState,
        ctx: &mut NativeCtx<'_, Manual>,
        mail: EnumerateClaimsResult,
    ) {
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
        for op in plan_heals(&state.snapshot, &states) {
            match op {
                Ok(HealOp::Transfer(mail)) => {
                    ctx.actor::<SourceCapability>().send_detached(&mail);
                }
                Ok(HealOp::Release(mail)) => {
                    ctx.actor::<SourceCapability>().send_detached(&mail);
                }
                Err(error) => ctx.fatal_abort(format!("boot claim-ref deep heal: ref mail did not encode: {error}")),
            }
        }
    }

    /// The boot reconcile's per-ref release reply (ADR-0179 gave this operation
    /// its own result type).
    ///
    /// The deep heals this core drives are idempotent sweeps and stranded-drop
    /// releases, so every clean variant is converged and carries nothing to act
    /// on. Only an operational fault is worth a word — the heal will be re-planned
    /// on the next boot from a fresh enumeration, so it is logged rather than
    /// retried here. Handled explicitly rather than left to warn-drop: this cap
    /// *is* the sender, and an unhandled reply from a send it made reads as a
    /// routing bug to anyone watching the logs.
    #[handler::manual]
    fn on_complete_release_result(
        _state: &mut ControlCoreState,
        _ctx: &mut NativeCtx<'_, Manual>,
        mail: CompleteReleaseResult,
    ) {
        if let CompleteReleaseResult::Err { error } = mail {
            tracing::warn!(
                target: "aether_chassis_bloomery::control",
                %error,
                "boot claim-ref deep heal release failed; the next boot re-plans it from a fresh enumeration",
            );
        }
    }

    /// The `aether.bloomery.query` read surface. With `bloom` unset, reply the
    /// whole [`ViewDocument`](aether_bloomery::ViewDocument); with `bloom` set to a
    /// digest, reply that one bloom's [`BloomView`](aether_bloomery::BloomView) (or
    /// [`QueryResult::NotFound`]). Reads off the live snapshot.
    #[handler::manual]
    fn on_query(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, mail: Query) {
        let inbound = ctx.take_inbound();
        // A release-request read is answered off the snapshot's own record map
        // rather than the view document — the document projects blooms, and an
        // orphan release belongs to no bloom by construction (ADR-0179).
        if let Some(request) = mail.release {
            inbound.reply(&release_response(&state.snapshot, &request));
            return;
        }
        // The live-read path holds no artifact access, so it resolves no question
        // bytes: a held member surfaces its pending decision only on the outward
        // mirror path. The digest-only hold still gates resolution in the reducer.
        let document = view_of(&state.snapshot, |_| None);
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
        inbound.reply(&result);
    }
}

impl ControlCoreState {
    /// The same-key admits in flight across both stages — the committed queue plus
    /// the accepted seals/supersessions still awaiting their claim reply — so the
    /// [`MAX_INFLIGHT_PER_KEY`] back-pressure cannot be dodged by piling up
    /// pre-commit ref stages for one key.
    /// Whether `event` names configuration content this core does not hold, and
    /// so cannot yet be reduced (ADR-0174).
    ///
    /// Only [`Unproducible::Absent`] defers. Content that is present but filed
    /// under another kind is not something a re-read can fix, so it falls to the
    /// reducer's refusal rather than sending the core back to the store for a row
    /// it already has.
    fn awaits_configs(&self, event: &Event) -> bool {
        event
            .fact
            .config_registries()
            .flat_map(|registry| self.configs.unproducible_in(registry))
            .any(|(_, _, reason)| reason == Unproducible::Absent)
    }

    /// Send an accepted event's decisions toward durability: through the shared
    /// claim-ref gate when it is a seal or supersession, straight to the commit
    /// otherwise.
    ///
    /// A locally-accepted seal/supersession must first win the shared claim refs
    /// (ADR-0150 §The claim registry) before its durable commit: a seal acquires
    /// its member + admission refs, a supersession transfers the predecessor's
    /// carried + admission refs and fresh-acquires net-new. The reply gates the
    /// commit in [`on_claim_result`](ControlCore::on_claim_result). A
    /// locally-rejected seal (or any non-seal fact) carries no successful outcome,
    /// so it needs no ref op and commits straight through — a rejected event still
    /// journals (bare append) so its key is durably consumed and a replay stays a
    /// no-op.
    fn gate_or_commit(
        &mut self,
        ctx: &mut NativeCtx<'_, Manual>,
        inbound: InboundMail,
        raw: Vec<u8>,
        event: Event,
        decisions: Decisions,
    ) {
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
            None => self.commit_admit(ctx, inbound, raw, event, decisions),
            Some(Err(error)) => {
                inbound.reply(&AdmitResult::Err { error: format!("admit claim encode failed: {error}") });
            }
            Some(Ok((claim, kind))) => {
                // The dispatch mints a correlation id; the eventual `ClaimResult`
                // echoes it, so `on_claim_result` recovers this exact pending admit.
                let mail_id = match &claim {
                    SourceClaim::Seal(mail) => ctx.actor::<SourceCapability>().send_detached_tracked(mail),
                    SourceClaim::Transfer(mail) => ctx.actor::<SourceCapability>().send_detached_tracked(mail),
                };
                self.pending_claims
                    .insert(mail_id.correlation_id, PendingClaim { inbound, raw, event, decisions, kind });
            }
        }
    }

    fn inflight_for_key(&self, key: &str) -> usize {
        let queued = self.pending.get(key).map_or(0, VecDeque::len);
        let claiming = self.pending_claims.values().filter(|claim| claim.event.idempotency_key.0 == key).count();
        queued + claiming
    }

    /// Project a decided event and send its combined [`Commit`] to the store — the
    /// durable-write stage every admit reaches, whether directly (a fact with no
    /// ref op, or a rejected seal) or after winning the shared claim refs (an
    /// accepted seal/supersession, gated in
    /// [`on_claim_result`](ControlCore::on_claim_result)). Queues the pending admit
    /// under its idempotency key (FIFO) so
    /// [`on_commit_result`](ControlCore::on_commit_result) can answer the admitter
    /// on the store reply.
    fn commit_admit(
        &mut self,
        ctx: &mut NativeCtx<'_, Manual>,
        inbound: InboundMail,
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
                inbound.reply(&AdmitResult::Err { error: format!("admit receipt encode failed: {error}") });
                return;
            }
        };
        // Every non-duplicate admitted event is journaled — even a rejected one, so
        // a replay stays a no-op and the key is durably consumed. A rejection
        // carries empty membership/outbox effects, so the commit is a bare append.
        let commit = Commit { idempotency_key: key.clone(), event: raw, releases, claims, outbox };
        ctx.actor::<StoreCapability>().send_detached(&commit);
        self.pending.entry(key).or_default().push_back(Pending { inbound, event, decisions });
    }

    /// Boot-time claim-ref reconcile (ADR-0150 §The claim registry). After replay
    /// rebuilds the snapshot, converge each bloom's refs to the holding its status
    /// implies. The per-record decision is [`reconcile_op`] — pure and tested
    /// against the real source capability in the `claim_reconcile` integration test
    /// — this walk only sends what it plans. Both ops are idempotent via the
    /// source's CAS read-guard, so a crash between a ref op and its local commit
    /// heals on the next boot; a ref-mail encode failure is unrecoverable at boot,
    /// so it fail-fasts (ADR-0063).
    fn reconcile_claim_refs(&mut self, ctx: &mut NativeCtx<'_, Manual>) {
        for record in self.snapshot.blooms.values() {
            match reconcile_op(record) {
                None => {}
                Some(Ok(ReconcileOp::Assert(mail))) => {
                    ctx.actor::<SourceCapability>().send_detached(&mail);
                }
                Some(Ok(ReconcileOp::Release(mail))) => {
                    ctx.actor::<SourceCapability>().send_detached(&mail);
                }
                Some(Err(error)) => ctx.fatal_abort(format!(
                    "boot claim-ref reconcile: bloom {:?} ref mail did not encode: {error}",
                    record.spec.id()
                )),
            }
        }
        // Then drive the deep heals (ADR-0150 §The claim registry, amended PR
        // #3556): enumerate every live ref so
        // [`on_enumerate_claims_result`](ControlCore::on_enumerate_claims_result)
        // can sweep tombstones and finish half-transfers the per-bloom V1 walk
        // above cannot see. Fire-and-forget: the reply routes back to that handler.
        ctx.actor::<SourceCapability>().send_detached(&EnumerateClaims);
    }
}

/// Answer an orphan-claim release-status read from the snapshot's record map
/// (ADR-0179). A digest that is not 32 bytes, or names no admitted request, is
/// [`QueryResult::ReleaseNotFound`] — a release-shaped miss, not the bloom-shaped
/// [`QueryResult::NotFound`], so the reader can name the resource the caller
/// actually asked for.
fn release_response(snapshot: &Snapshot, request: &[u8]) -> QueryResult {
    let Some(request) = Digest::from_slice(request) else {
        return QueryResult::ReleaseNotFound;
    };
    let Some(record) = snapshot.orphan_releases.get(&request) else {
        return QueryResult::ReleaseNotFound;
    };
    match to_vec(record) {
        Ok(record) => QueryResult::Release { record },
        Err(error) => QueryResult::Err { error: format!("release record encode failed: {error}") },
    }
}

/// The idempotency key echoed on every [`CommitResult`] variant (the correlation
/// axis for the commit reply).
fn commit_key(result: &CommitResult) -> &str {
    match result {
        CommitResult::Applied { idempotency_key, .. }
        | CommitResult::Duplicate { idempotency_key }
        | CommitResult::Conflict { idempotency_key, .. }
        | CommitResult::Err { idempotency_key, .. } => idempotency_key,
    }
}

/// Project a decided event's effects into the store commit's typed axes: the
/// membership releases and claims the `active_membership` table applies, and the
/// outbox payloads it enqueues. The snapshot-only effects carry no durable store
/// row — they are rebuilt on replay by `reduce` + `apply` from the journaled event.
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
            // The landing-receipt topic carries the receipt *and* the landed
            // bloom's membership: the receipt value names no members, so a
            // payload without them cannot reach the objects it belongs on after
            // a restart drains it (ADR-0149 §The receipt carries its members).
            Decision::EmitReceipt(projected) => {
                outbox.push(OutboxPayload::new(Topic::LandingReceipt, to_vec(projected)?));
            }
            Decision::RedispatchStage { bloom, question, answer, words } => {
                let payload =
                    RedispatchPayload { bloom: bloom.0, question: *question, answer: *answer, words: words.clone() };
                outbox.push(OutboxPayload::new(Topic::Redispatch, to_vec(&payload)?));
            }
            Decision::DispatchAttempt {
                bloom,
                workpiece,
                stage,
                transformation,
                scope_revision,
                candidate,
                profile,
                configs,
            } => {
                let payload = DispatchPayload {
                    bloom: bloom.0,
                    workpiece: workpiece.clone(),
                    stage: *stage,
                    transformation: transformation.clone(),
                    scope_revision: *scope_revision,
                    candidate: *candidate,
                    profile: profile.clone(),
                    configs: configs.clone(),
                };
                outbox.push(OutboxPayload::new(Topic::Dispatch, to_vec(&payload)?));
            }
            Decision::DispatchLand { bloom, expected_base, new_head } => {
                let payload = LandPayload { bloom: bloom.0, expected_base: *expected_base, new_head: *new_head };
                outbox.push(OutboxPayload::new(Topic::Land, to_vec(&payload)?));
            }
            Decision::DispatchIntegration { bloom, base, members, adopt_from } => {
                let payload = IntegratePayload {
                    bloom: bloom.0,
                    base: *base,
                    members: members.clone(),
                    adopt_from: adopt_from.map(|predecessor| predecessor.0),
                };
                outbox.push(OutboxPayload::new(Topic::Integrate, to_vec(&payload)?));
            }
            Decision::DispatchAggregateReview { bloom, transformation, roll, profile } => {
                let payload = AggregateReviewPayload {
                    profile: profile.clone(),
                    bloom: bloom.0,
                    transformation: transformation.clone(),
                    pass: ReviewPass::from_roll(*roll),
                };
                outbox.push(OutboxPayload::new(Topic::AggregateReview, to_vec(&payload)?));
            }
            Decision::DispatchAggregateVerify { bloom, transformation, profile, roll: _ } => {
                let payload = AggregateVerifyPayload {
                    profile: profile.clone(),
                    bloom: bloom.0,
                    transformation: transformation.clone(),
                };
                outbox.push(OutboxPayload::new(Topic::AggregateVerify, to_vec(&payload)?));
            }
            Decision::DispatchOrphanClaimRelease { request, target } => {
                let payload = OrphanClaimReleasePayload { request: *request, target: target.clone() };
                outbox.push(OutboxPayload::new(Topic::OrphanClaimRelease, to_vec(&payload)?));
            }
            Decision::RecordOrphanClaimRelease { .. }
            | Decision::InheritClaim { .. }
            | Decision::RecordResolution { .. }
            | Decision::RecordEvidence { .. }
            | Decision::ReleaseHold { .. }
            | Decision::AdvanceStage { .. }
            | Decision::MarkSuperseded { .. }
            | Decision::SetResolved { .. }
            | Decision::RecordIntegration { .. }
            | Decision::RecordAggregateRoll { .. }
            | Decision::RecordAggregateVerifyRoll { .. }
            | Decision::RecordLandingRoll { .. }
            | Decision::SetUnresolved { .. }
            | Decision::RecordReviewPark { .. }
            | Decision::RecordWedge { .. }
            | Decision::RevokeResolution { .. }
            | Decision::AdvanceMainline { .. }
            | Decision::RecordObservation { .. } => {}
        }
    }
    Ok((releases, claims, outbox))
}

/// Answer a duplicate admission, naming the key it discarded.
///
/// A duplicate is a *dropped fact*: the event reduces to nothing and none of the
/// effects it would have carried run. An operator reads that in the reply, but a
/// reactor admits fire-and-forget (`send_envelope_detached`) and never learns the
/// outcome — so a reactor's discarded fact is otherwise invisible at every layer
/// and the run simply stops, with no wedge and no evidence (#4722). The `warn`
/// is the one place that can see it.
fn admit_duplicate(idempotency_key: &str) -> AdmitResult {
    tracing::warn!(
        target: "aether_chassis_bloomery::control",
        idempotency_key,
        "admission reduced to a duplicate; the fact is discarded and a fire-and-forget admitter never learns it",
    );
    admit_ok(&Outcome::Duplicate)
}

/// Encode a reducer [`Outcome`] into an [`AdmitResult`], mapping an encode failure
/// to the `Err` reply rather than an empty `Ok` payload the admitter would decode
/// as a valid-but-blank outcome.
fn admit_ok(outcome: &Outcome) -> AdmitResult {
    match to_vec(outcome) {
        Ok(outcome) => AdmitResult::Ok { outcome },
        Err(error) => AdmitResult::Err { error: format!("admit outcome encode failed: {error}") },
    }
}

fn lowercase_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
        write!(&mut text, "{byte:02x}").expect("writing to String cannot fail");
        text
    })
}

#[cfg(test)]
mod tests {
    use aether_bloomery::{QueryResult, Snapshot};

    use super::{lowercase_hex, release_response};

    #[test]
    fn observe_mainline_key_uses_lowercase_hex() {
        assert_eq!(lowercase_hex(&[0, 15, 160, 255]), "000fa0ff");
    }

    // Tripwire: a release read that misses must answer the release-shaped miss,
    // never the bloom-shaped `NotFound`. The reply variant is the only thing
    // that tells the two reads apart — the route holds no correlation — so
    // collapsing them makes `GET /claims/releases/{digest}` answer "no bloom
    // with that id", naming a resource the caller never asked for on the very
    // route the REST recipe tells an operator to poll.
    #[test]
    fn a_release_read_that_misses_is_release_shaped_not_bloom_shaped() {
        let snapshot = Snapshot::default();

        assert_eq!(release_response(&snapshot, &[7; 32]), QueryResult::ReleaseNotFound);
        assert_eq!(release_response(&snapshot, b"not a digest"), QueryResult::ReleaseNotFound);
    }
}
