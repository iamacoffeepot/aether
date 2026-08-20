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
//! second to a [`CommitResult::Duplicate`] no-op. An admit whose *bloom* already
//! has a commit in flight waits for that apply before reducing (#5168): two
//! set-completing claims in one poll batch would otherwise both see an incomplete
//! set and never emit [`Decision::DispatchIntegration`]. The store and source caps
//! are now native siblings addressed by type (`ctx.actor::<StoreCapability>()` /
//! `ctx.actor::<SourceCapability>()`); the reply routes back by kind, correlated by
//! the echoed idempotency key ([`CommitResult`]) or the dispatch correlation id
//! ([`ClaimResult`]), exactly as the former wasm actor's name-addressed sends did.
//!
//! Boot **does not** drain or ack the outbox — outbox republish belongs to the
//! reactor capabilities (#3499). This cap only *enqueues* outbox entries,
//! atomically inside the commit.

use std::collections::{BTreeMap, VecDeque};
use std::mem;
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use aether_actor::{Manual, runtime};
use aether_data::Kind;
use aether_data::wire::{Error as WireError, from_bytes, to_vec};
use aether_substrate::InboundMail;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
#[cfg(test)]
use aether_substrate::mail::mailer::Mailer;

use aether_bloomery::control::{
    Admit, AdmitResult, AggregateReviewPayload, AggregateVerifyPayload, ClaimResult, ClaimSeal, Commit, CommitResult,
    CompleteReleaseResult, DispatchPayload, EnumerateClaims, EnumerateClaimsResult, HealOp, IntegratePayload,
    LandPayload, LoadConfigs, LoadConfigsResult, MembershipMutation, MetricsQuery, MetricsQueryResult, MetricsView,
    ObserveMainline, ObserveMainlineResult, OrphanClaimReleasePayload, OutboxPayload, Query, QueryResult, ReconcileOp,
    RedispatchPayload, ReplayJournal, ReplayJournalResult, ReviewPass, SpendQuery, SpendQueryResult, SplicePayload,
    Topic, TransferSeal, held_to_seal_error, held_to_supersede_error, plan_heals, reconcile_op, release_seal_mail,
    seal_claim_mail, transfer_seal_mail,
};
use aether_bloomery::{
    BloomId, CalibrationDocument, CalibrationLedger, ClaimRefKind, ClaimRefState, DAYS_CAP, Decision, Decisions,
    Digest, Event, EvidenceKind, Fact, IdempotencyKey, METRICS_DEFAULT_LIMIT, METRICS_MAX_LIMIT, MetricsLedger,
    OperatorRepairError, Outcome, ResolvedConfigs, Snapshot, SpendWindow, StudyRecord, Unproducible,
    decode_recorded_decisions, grade, is_active_unlanded, measure, reduce, view_of, window_label,
};

use super::{ControlCore, ControlSetup, ObserveTick};
use crate::artifacts::{ArtifactsCapabilityState, GetResult, resolve_root};
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
#[cfg(feature = "github")]
use crate::bloomery::{LandReactorCapability, LandTick};
use crate::source::SourceCapability;
use crate::store::{StoreCapability, now_unix_millis};

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

/// An admit that arrived before boot journal replay finished. The raw bytes and
/// the inbound reply obligation are kept as they arrived — nothing is decoded
/// or reduced until [`on_replay_result`](ControlCore::on_replay_result) drains
/// the queue against the restored snapshot (issue #5066).
struct HeldAdmission {
    inbound: InboundMail,
    raw: Vec<u8>,
}

/// An admit waiting for a same-bloom commit to apply before it reduces (#5168).
///
/// Carries the idempotency key so the per-key in-flight cap still counts a
/// held entry; the raw bytes are what the resumed admit re-decodes.
struct HeldBloomAdmit {
    inbound: InboundMail,
    raw: Vec<u8>,
    key: String,
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
    /// The capability ledger, folded from the same admitted events the snapshot
    /// is (ADR-0184). It sits here rather than being computed per read because
    /// it is a fold over the whole journal and this cap is the only thing that
    /// sees every admitted event exactly once — at boot replay and at each
    /// commit — so accumulating it costs one call beside `Snapshot::apply` and
    /// re-deriving it would cost a second whole-journal read per request.
    calibration: CalibrationLedger,
    /// Cost / timing / throughput fold, beside the snapshot and the
    /// calibration ledger. Rebuilt on journal replay; extended on each admit.
    metrics: MetricsLedger,
    configs: ResolvedConfigs,
    /// The window's measured spend, refreshed whenever the snapshot's study
    /// evidence could have changed. The reducer cannot fetch study-record
    /// bytes, so this is the argument `reduce` reads the way it reads
    /// `configs` (ADR-0192).
    spend: SpendWindow,
    /// Second handle on the shared artifacts root, so a calibration or spend
    /// read can resolve study-record bytes without a mail round-trip. `None`
    /// when the store would not open — those reads then report every study
    /// as unaccounted rather than guessing a price.
    artifacts: Option<ArtifactsCapabilityState>,
    pending: BTreeMap<String, VecDeque<Pending>>,
    pending_claims: BTreeMap<u64, PendingClaim>,
    pending_configs: BTreeMap<u64, PendingConfigs>,
    /// Admits that arrived before `replayed` is true. FIFO and process-local:
    /// each entry holds its inbound reply obligation and raw event bytes, and
    /// nothing is decoded, reduced, journaled, or replied until
    /// [`on_replay_result`](ControlCore::on_replay_result) drains them against
    /// the restored snapshot (issue #5066).
    held_admissions: VecDeque<HeldAdmission>,
    /// Admits waiting because their bloom already has a commit in flight
    /// (#5168). FIFO per bloom; drained from
    /// [`on_commit_result`](ControlCore::on_commit_result) once that bloom has
    /// no remaining pending commit, so the reduce sees the applied snapshot.
    held_for_bloom: BTreeMap<BloomId, VecDeque<HeldBloomAdmit>>,
    /// Whether the boot journal replay has finished folding. The mainline
    /// observer polls off a wall-clock timer, which starts the moment this cap
    /// mounts, so it has to hold until the snapshot is the one the journal
    /// describes — see [`on_observe_tick`](ControlCore::on_observe_tick).
    /// Admission handlers consult the same flag: a re-adopted completion that
    /// arrives before the fold would otherwise reduce against the empty boot
    /// snapshot and consume its key as `UnknownOrInactiveBloom`.
    replayed: bool,
    /// The mainline observer's poll-timer sidecar, held for its `Drop` (which
    /// stops and joins the thread on teardown).
    _timer: TimerHandle,
}

#[runtime]
impl NativeActor for ControlCore {
    type State = ControlCoreState;
    type Config = ();
    type Params = ControlSetup;
    const NAMESPACE: &'static str = aether_bloomery::CONTROL_CORE_NAMESPACE;

    /// Mount the snapshot owner and start its mainline observer on the
    /// coordinator's poll cadence. No boot tick is pushed here: the one
    /// observation boot owes is the one the journal replay sends when its fold
    /// completes, and a wake fired from `init` would race it against the very
    /// snapshot #4677 sequenced it behind.
    fn init((): (), config: ControlSetup, ctx: &mut NativeInitCtx<'_>) -> Result<ControlCoreState, BootError> {
        let timer = spawn_timer(
            ctx.mailer(),
            ctx.self_id(),
            ObserveTick::ID,
            ObserveTick::default().encode_into_bytes(),
            "aether-bloomery-observe",
            Duration::from_secs(config.poll_interval_secs.max(1)),
        );
        Ok(ControlCoreState {
            snapshot: Snapshot::default(),
            calibration: CalibrationLedger::default(),
            metrics: MetricsLedger::default(),
            configs: ResolvedConfigs::default(),
            spend: SpendWindow::default(),
            artifacts: open_artifacts(config.artifacts_root.as_deref()),
            pending: BTreeMap::new(),
            pending_claims: BTreeMap::new(),
            pending_configs: BTreeMap::new(),
            held_admissions: VecDeque::new(),
            held_for_bloom: BTreeMap::new(),
            replayed: false,
            _timer: timer,
        })
    }

    /// Boot: read the stored configuration first, then replay the journal from
    /// [`on_load_configs_result`](Self::on_load_configs_result). Sequenced rather
    /// than sent together because the fold reads configuration — a seal it folds
    /// back registers the catalog content its members sealed, and a replay that
    /// ran first would register blooms against an empty set, rebuilding a
    /// snapshot that never existed. Lives in `wire` (post-init, mail-allowed).
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
    /// is answered. A different-key admit for a bloom that already has a commit
    /// in flight waits for that apply (#5168) so completeness is decided against
    /// the snapshot that includes the earlier claim.
    ///
    /// Before boot replay finishes, the raw bytes and reply obligation are
    /// enqueued rather than reduced: deciding against the empty boot snapshot
    /// would consume the key as `UnknownOrInactiveBloom` (issue #5066).
    #[handler::manual]
    fn on_admit(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, mail: Admit) {
        let inbound = ctx.take_inbound();
        if !state.replayed {
            state.held_admissions.push_back(HeldAdmission { inbound, raw: mail.event });
            return;
        }
        Self::on_admit_event(state, ctx, inbound, mail.event);
    }

    /// Reply to a fire-and-forget [`Admit`] this core sent to itself (mainline
    /// observation, host-fault resume). Ok is a no-op; Err is the refused-admission
    /// event that used to miss dispatch.
    #[handler::single]
    fn on_admit_result(_state: &mut ControlCoreState, _ctx: &mut NativeCtx<'_>, mail: AdmitResult) {
        if let AdmitResult::Err { error } = mail {
            tracing::error!(target: "aether_chassis_bloomery::control", %error, "admit refused");
        }
    }

    /// The ordinary admission path: decode, reduce against the live snapshot,
    /// and commit. Shared by a live [`on_admit`](Self::on_admit) and by the
    /// post-replay drain, so a held arrival is decided exactly as one that
    /// arrived after the snapshot was restored.
    fn on_admit_event(
        state: &mut ControlCoreState,
        ctx: &mut NativeCtx<'_, Manual>,
        inbound: InboundMail,
        raw: Vec<u8>,
    ) {
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
        if let Some(bloom) = event_bloom(&event)
            && state.bloom_has_commit_in_flight(&bloom)
        {
            state.hold_for_bloom(bloom, inbound, raw, event.idempotency_key.0);
            return;
        }
        let decisions = reduce(&state.snapshot, &event, &state.configs, &state.spend);
        finish_admit(state, ctx, inbound, raw, event, decisions);
    }

    /// Re-run a held admit now that the configuration set is refilled.
    ///
    /// The same path [`on_admit`](Self::on_admit) takes past its gate, minus the
    /// gate: a second deferral would loop the store on an address the re-read
    /// already declined to produce. The back-pressure check is not re-run either
    /// — this admit was already counted when it arrived.
    ///
    /// A config reply cannot outrun boot replay — the boot read is the one
    /// that *starts* replay — but this is still an encoded admit entry, so it
    /// holds the same way [`on_admit`](Self::on_admit) does if replay is
    /// somehow still incomplete.
    fn resume_admit(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, inbound: InboundMail, raw: Vec<u8>) {
        if !state.replayed {
            state.held_admissions.push_back(HeldAdmission { inbound, raw });
            return;
        }
        let event: Event = match from_bytes(&raw) {
            Ok(event) => event,
            Err(error) => {
                inbound.reply(&AdmitResult::Err { error: format!("admit decode failed: {error}") });
                return;
            }
        };
        if let Some(bloom) = event_bloom(&event)
            && state.bloom_has_commit_in_flight(&bloom)
        {
            state.hold_for_bloom(bloom, inbound, raw, event.idempotency_key.0);
            return;
        }
        let decisions = reduce(&state.snapshot, &event, &state.configs, &state.spend);
        finish_admit(state, ctx, inbound, raw, event, decisions);
    }

    /// The `aether.source` claim/transfer/release reply. Correlate on the echoed
    /// dispatch correlation id. A gated admit's [`ClaimResult::Acquired`] proceeds
    /// to the durable commit; its [`ClaimResult::Held`] refuses the admit with the
    /// matching `SealError`/`SupersedeError` (never committing, so a transient
    /// foreign hold is retryable under a fresh key); a [`ClaimResult::Err`] fails
    /// it. An uncorrelated reply — a fire-and-forget release or a boot-reconcile
    /// re-assertion — has no pending entry: `Acquired` / `Held` is ignored, and
    /// [`ClaimResult::Err`] is logged at warn so a silent land-time or boot
    /// release failure is visible (the next boot re-plans from a fresh
    /// enumeration).
    #[handler::manual]
    fn on_claim_result(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, mail: ClaimResult) {
        let correlation = ctx.reply_target().correlation_id;
        let Some(PendingClaim { inbound, raw, event, decisions, kind }) = state.pending_claims.remove(&correlation)
        else {
            if let ClaimResult::Err { error } = mail {
                tracing::warn!(
                    target: "aether_chassis_bloomery::control",
                    %error,
                    "uncorrelated claim result failed; a fire-and-forget land release or boot-reconcile op did not apply",
                );
            }
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
                state.calibration.observe(&event, &decisions, &state.configs);
                let sequence = state.metrics.through_sequence().saturating_add(1);
                state.metrics.observe(sequence, &event, &decisions, &state.configs, Some(now_unix_millis()));
                state.snapshot = state.snapshot.apply(&event, &decisions, &state.configs);
                state.refresh_spend();
                // A committed land decision is already in the outbox; wake the
                // reactor now rather than waiting on its next poll. The mail
                // carries no payload — the row is the authority (ADR-0149).
                // Periodic and boot ticks remain the recovery path for a missed
                // nudge or a crash between the durable write and this send.
                #[cfg(feature = "github")]
                if decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchLand { .. })) {
                    ctx.actor::<LandReactorCapability>().send_detached(&LandTick::default());
                }
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
        if let Some(bloom) = event_bloom(&event) {
            Self::drain_held_for_bloom(state, ctx, bloom);
        }
    }

    /// Reduce the next held admit for `bloom` now that its prior commit has
    /// applied. One at a time: the drained admit commits, and the rest wait
    /// for *that* apply, so completeness is never decided against a stale
    /// snapshot. Duplicates and immediate replies loop; a deferral to configs
    /// or claim-refs stops the drain until that path resumes.
    fn drain_held_for_bloom(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, bloom: BloomId) {
        loop {
            if state.bloom_has_commit_in_flight(&bloom) {
                return;
            }
            let Some(HeldBloomAdmit { inbound, raw, key: _ }) = state.pop_held_for_bloom(&bloom) else {
                return;
            };
            let configs = state.pending_configs.len();
            let claims = state.pending_claims.len();
            Self::on_admit_event(state, ctx, inbound, raw);
            if state.bloom_has_commit_in_flight(&bloom) {
                return;
            }
            if state.pending_configs.len() > configs || state.pending_claims.len() > claims {
                return;
            }
        }
    }

    /// The stored-configuration read (ADR-0174). Two arrivals reach here and they
    /// resolve differently.
    ///
    /// The boot read (no held admit) fills the set and *then* sends
    /// [`ReplayJournal`], which is the ordering that gives the fold the same
    /// configuration content the original admits sealed against
    /// ([`Snapshot::apply`] registers a sealed bloom's catalog from it). A read
    /// failure at boot is unrecoverable for the same reason a failed replay is —
    /// the snapshot it would rebuild is not the one that existed — so it
    /// fail-fasts (ADR-0063).
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

    /// Boot journal replay reply: fold each record — decode the event and its
    /// recorded decisions, then [`Snapshot::apply`] — to rebuild the snapshot.
    /// The reducer is never consulted (ADR-0190): the record is what was
    /// decided, and re-deciding under the current binary rewrites history.
    /// No outbox drain/ack — republish is #3499's. A read or a corrupt record
    /// at boot is unrecoverable, so it fail-fasts (ADR-0063) rather than
    /// coming up on a torn snapshot.
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
            // Fold the recorded decision — never re-decide (ADR-0190). The rules
            // in force today govern new admissions only; re-reducing history under
            // them resurrects rejections a looser rule now admits and re-refuses
            // admissions a stricter rule no longer would (#4937).
            let decisions: Decisions =
                match decode_recorded_decisions(&record.decisions, record.decisions_schema.as_deref()) {
                    Ok(decisions) => decisions,
                    Err(error) => ctx.fatal_abort(format!(
                        "boot journal replay: record {} ({}) {error}",
                        record.sequence, record.idempotency_key
                    )),
                };
            // The calibration ledger folds the same pair, so a replay rebuilds
            // it exactly as the live commits built it (ADR-0184).
            state.calibration.observe(&event, &decisions, &state.configs);
            state.metrics.observe(record.sequence, &event, &decisions, &state.configs, record.recorded_unix_millis);
            state.snapshot = state.snapshot.apply(&event, &decisions, &state.configs);
        }
        state.refresh_spend();
        state.reconcile_claim_refs(ctx);
        state.replayed = true;
        // Drain every admit that arrived while the snapshot was still empty,
        // in arrival order, through the ordinary path. Their reply obligations
        // stayed open; nothing was decided or committed until now (issue #5066).
        let held = mem::take(&mut state.held_admissions);
        for HeldAdmission { inbound, raw } in held {
            Self::on_admit_event(state, ctx, inbound, raw);
        }
        // Only now is the snapshot the one the journal describes, so only now can
        // an observation be decided against it (#4677). Asked here rather than
        // from the source cap's own `wire` because that fires the moment the cap
        // mounts, which is *concurrent* with this replay: an observation decided
        // then reads an empty bloom map, finds nothing in flight, and advances
        // mainline out from under the very land the fold above is about to apply
        // — which then refuses as `BaseMismatch` and leaves a landed bloom
        // reading unlanded for the whole boot. Held admissions drain first so
        // a re-adopted completion is decided against this snapshot before the
        // observer's own admit is even sent.
        ctx.actor::<SourceCapability>().send_detached(&observe_mail(&state.snapshot.mainline));
    }

    /// Poll wake: ask the source for the repository's live mainline head, so a
    /// commit a person merged reaches the snapshot on the coordinator's own
    /// cadence rather than on its next restart.
    ///
    /// Held until the replay above has folded, and only until then. #4677's
    /// constraint is an ordering one — an observation decided against a
    /// half-replayed snapshot finds nothing in flight and advances mainline out
    /// from under a land the fold has not reached yet — and past
    /// [`on_replay_result`](Self::on_replay_result) the snapshot is always fully
    /// folded, so every later observation is decided against a coherent snapshot
    /// by construction. What keeps a *continuous* observer safe is already in the
    /// reducer: the advance is held while a bloom is in flight, so an observation
    /// can never move mainline out from under an in-flight land.
    ///
    /// The observer lives here rather than in the source cap because the source
    /// is stateless between requests and holds no snapshot: which observations
    /// may be decided at all is a property of the replay state this cap owns.
    #[handler::manual]
    fn on_observe_tick(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, _mail: ObserveTick) {
        if !state.replayed {
            return;
        }
        ctx.actor::<SourceCapability>().send_detached(&observe_mail(&state.snapshot.mainline));
        // Re-probe every member held on a host-provisioning gap (#5020). The
        // same poll interval that watches mainline is the honest cadence:
        // a later tick whose preflight passes resumes Verify; one that still
        // misses holds again. An operator hold still swallows the dispatch.
        resume_host_faults(state, ctx);
    }

    /// Admit the observed mainline head (#4667). The reply to an
    /// [`ObserveMainline`] — the one the boot replay sends once its fold is
    /// complete (#4677), and each one
    /// [`on_observe_tick`](Self::on_observe_tick) sends on the poll cadence
    /// afterwards.
    ///
    /// A failed observation logs and continues rather than aborting boot: the
    /// coordinator is fully functional on a stale mainline — every bloom already
    /// sealed keeps its base, and the next poll re-observes — so an unreachable
    /// source is not the unrecoverable class the replay and claim-ref reconcile
    /// fail-fast on.
    ///
    /// The admit goes through the ordinary [`Admit`] door rather than an internal
    /// shortcut, so this fact is journaled, deduped, and committed exactly like
    /// one arriving over the wire.
    #[handler::manual]
    fn on_observe_mainline_result(
        state: &mut ControlCoreState,
        ctx: &mut NativeCtx<'_, Manual>,
        mail: ObserveMainlineResult,
    ) {
        let (head, fast_forward) = match mail {
            ObserveMainlineResult::Ok { head, fast_forward } => (head, fast_forward),
            ObserveMainlineResult::Err { error } => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::control",
                    %error,
                    "mainline observation failed; mainline stays where the last land left it until a later poll re-observes"
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
                    "mainline observation did not decode"
                );
                return;
            }
        };
        // Log hygiene: skip an admit whose (head, mainline) pair is already
        // in `seen`. That pair is the key, so a later recovery against a
        // different mainline is a new admit (#4938). Skipping only when both
        // pointers already name this head missed the MainlineHeld steady
        // state — `observed == head` while mainline sits behind — and every
        // poll re-admitted the journaled key, reduced to Duplicate, and
        // fired `admit_duplicate`'s warn for the life of the bloom.
        //
        // Compared against the snapshot rather than a private memo of the last
        // head sent: `seen` moves only once a commit durably landed, so a
        // failed commit leaves the pair out and the next poll re-admits,
        // where a memo would have recorded the send and never retried.
        if observation_already_admitted(&state.snapshot, &head) {
            return;
        }

        let event = Event {
            idempotency_key: observe_mainline_key(&head, &state.snapshot.mainline),
            fact: if fast_forward {
                Fact::ObserveMainline { head }
            } else {
                Fact::ObserveMainlineDiverged { head }
            },
        };
        match to_vec(&event) {
            Ok(bytes) => ctx.actor::<ControlCore>().send_detached(&Admit { event: bytes }),
            Err(error) => tracing::warn!(
                target: "aether_chassis_bloomery::control",
                %error,
                "mainline observation did not encode"
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
        // Calibration is a whole-fleet read over the folded ledger and every
        // bloom's grade, so it answers before the view document is projected at
        // all — the document would be built and thrown away (ADR-0184).
        if mail.calibration {
            inbound.reply(&calibration_response(&state.calibration, &state.snapshot, state.artifacts.as_mut()));
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

    /// `GET /metrics/*` — bounded reads off the folded metrics ledger.
    #[handler::manual]
    fn on_metrics_query(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, mail: MetricsQuery) {
        let inbound = ctx.take_inbound();
        inbound.reply(&metrics_response(state, mail));
    }

    /// `GET /spend` — the window `measure` already computes, previously unserved.
    #[handler::manual]
    fn on_spend_query(state: &mut ControlCoreState, ctx: &mut NativeCtx<'_, Manual>, _mail: SpendQuery) {
        let inbound = ctx.take_inbound();
        inbound.reply(&spend_response(&state.spend));
    }
}

impl ControlCoreState {
    /// Inert snapshot owner for dispatch-table tests. The timer sidecar is an
    /// hour-long no-op dropped with the state.
    #[cfg(test)]
    pub(crate) fn inert(mailer: Arc<Mailer>) -> Self {
        Self {
            snapshot: Snapshot::default(),
            calibration: CalibrationLedger::default(),
            metrics: MetricsLedger::default(),
            configs: ResolvedConfigs::default(),
            spend: SpendWindow::default(),
            artifacts: None,
            pending: BTreeMap::new(),
            pending_claims: BTreeMap::new(),
            pending_configs: BTreeMap::new(),
            held_admissions: VecDeque::new(),
            held_for_bloom: BTreeMap::new(),
            replayed: true,
            _timer: spawn_timer(
                mailer,
                aether_data::MailboxId(0),
                ObserveTick::ID,
                ObserveTick::default().encode_into_bytes(),
                "test-control-admit-result",
                Duration::from_hours(1),
            ),
        }
    }

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
    /// no-op. A rejected operator repair is the exception: it journals under a
    /// key that includes the outcome so the content-derived key stays free for a
    /// later accepted retry of the same repair (#5107).
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
            (Fact::GraphSeal { spec, predecessor: None, .. }, Outcome::Sealed(bloom)) => {
                Some(seal_claim_mail(bloom, spec).map(|mail| (SourceClaim::Seal(mail), ClaimKind::Seal)))
            }
            (Fact::Supersede { predecessor, successor }, Outcome::Superseded { .. }) => Some(
                transfer_seal_mail(&self.snapshot, predecessor, successor)
                    .map(|mail| (SourceClaim::Transfer(mail), ClaimKind::Supersede)),
            ),
            (Fact::GraphSeal { predecessor: Some(predecessor), spec, .. }, Outcome::Superseded { .. }) => Some(
                transfer_seal_mail(&self.snapshot, predecessor, spec)
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

    /// Re-measure the window from the live snapshot's study evidence.
    ///
    /// Called wherever that evidence can change — a landed commit that
    /// admitted a study record, and the boot replay that rebuilds the same
    /// log. An unresolvable artifact raises the unaccounted count rather
    /// than a guessed price.
    fn refresh_spend(&mut self) {
        let records = load_study_records(self.artifacts.as_mut(), &self.snapshot);
        self.spend = measure(&self.snapshot, |digest| records.get(digest).copied());
        if self.spend.label.is_empty() {
            self.spend.label = window_label(now_unix_millis());
        }
    }

    fn inflight_for_key(&self, key: &str) -> usize {
        let queued = self.pending.get(key).map_or(0, VecDeque::len);
        let claiming = self.pending_claims.values().filter(|claim| claim.event.idempotency_key.0 == key).count();
        let held = self.held_for_bloom.values().flatten().filter(|held| held.key == key).count();
        queued + claiming + held
    }

    /// Whether `bloom` already has a store commit outstanding — the snapshot
    /// does not yet include that admit, so a sibling claim must wait (#5168).
    fn bloom_has_commit_in_flight(&self, bloom: &BloomId) -> bool {
        self.pending.values().flatten().any(|pending| event_bloom(&pending.event).as_ref() == Some(bloom))
    }

    fn hold_for_bloom(&mut self, bloom: BloomId, inbound: InboundMail, raw: Vec<u8>, key: String) {
        self.held_for_bloom.entry(bloom).or_default().push_back(HeldBloomAdmit { inbound, raw, key });
    }

    fn pop_held_for_bloom(&mut self, bloom: &BloomId) -> Option<HeldBloomAdmit> {
        let queue = self.held_for_bloom.get_mut(bloom)?;
        let held = queue.pop_front()?;
        if queue.is_empty() {
            self.held_for_bloom.remove(bloom);
        }
        Some(held)
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
        // The decisions journal beside the event (ADR-0190): replay folds this
        // record instead of re-deciding, so a later binary's rules cannot rewrite
        // what this admission decided. An encode failure must reject the admit —
        // a row without its decision cannot be replayed.
        let recorded = match to_vec(&decisions) {
            Ok(recorded) => recorded,
            Err(error) => {
                inbound.reply(&AdmitResult::Err { error: format!("admit decision encode failed: {error}") });
                return;
            }
        };
        // Every non-duplicate admitted event is journaled — even a rejected one, so
        // a replay stays a no-op and the key is durably consumed. A rejection
        // carries empty membership/outbox effects, so the commit is a bare append.
        // Operator-repair refusals arrive here already retagged (#5107) so the
        // content-derived key they were submitted under is not the one consumed.
        let commit = Commit {
            idempotency_key: key.clone(),
            event: raw,
            decisions: recorded,
            decider: env!("AETHER_GIT_SHA").to_owned(),
            releases,
            claims,
            outbox,
        };
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

/// Answer a calibration read (ADR-0184): the folded capability ledger beside
/// [`grade`]'s report over the same snapshot — the study report's first reader
/// outside a test.
///
/// Both halves resolve study artifacts through [`load_study_records`]: the
/// fold holds digests, and this read pays the lookup against the shared
/// artifacts store. An artifact that cannot be read or decoded costs its
/// cell the cost, time, and sample columns and nothing else — never a
/// guessed price.
fn calibration_response(
    ledger: &CalibrationLedger,
    snapshot: &Snapshot,
    artifacts: Option<&mut ArtifactsCapabilityState>,
) -> QueryResult {
    let records = load_study_records(artifacts, snapshot);
    let document = CalibrationDocument {
        ledger: ledger.report(|digest| records.get(digest).copied()),
        study: grade(snapshot, |digest| records.get(digest).copied()),
    };
    match to_vec(&document) {
        Ok(document) => QueryResult::Calibration { document },
        Err(error) => QueryResult::Err { error: format!("calibration document encode failed: {error}") },
    }
}

fn metrics_response(state: &mut ControlCoreState, query: MetricsQuery) -> MetricsQueryResult {
    let MetricsQuery { view, bloom, from_sequence, limit, notice } = query;
    let clamp = |requested: Option<u64>| match requested {
        None => METRICS_DEFAULT_LIMIT,
        Some(limit) if limit > METRICS_MAX_LIMIT => METRICS_MAX_LIMIT,
        Some(limit) => limit,
    };
    match view {
        MetricsView::Summary => {
            let records = load_study_records(state.artifacts.as_mut(), &state.snapshot);
            let active = u64::try_from(
                state.snapshot.blooms.values().filter(|record| is_active_unlanded(record.status)).count(),
            )
            .unwrap_or(u64::MAX);
            encode_metrics(state.metrics.summary(active, |digest| records.get(digest).copied()), notice)
        }
        MetricsView::Days => {
            let mut rows = state.metrics.day_rows();
            let cap = usize::try_from(DAYS_CAP).unwrap_or(usize::MAX);
            if rows.len() > cap {
                let start = rows.len() - cap;
                rows = rows.split_off(start);
            }
            encode_metrics(rows, notice)
        }
        MetricsView::Blooms => {
            let limit = usize::try_from(clamp(limit)).unwrap_or(usize::MAX);
            let from = from_sequence.unwrap_or(0);
            let rows: Vec<_> =
                state.metrics.bloom_rows().into_iter().filter(|row| row.seal_sequence > from).take(limit).collect();
            encode_metrics(rows, notice)
        }
        MetricsView::Timeline => {
            let Some(bytes) = bloom.as_deref() else {
                return MetricsQueryResult::Err { error: "timeline requires a bloom id".into() };
            };
            let Some(bloom) = Digest::from_slice(bytes).map(BloomId) else {
                return MetricsQueryResult::Err { error: "bloom is not a 32-byte digest".into() };
            };
            if !state.snapshot.blooms.contains_key(&bloom) && state.metrics.timeline(bloom).spans.is_empty() {
                return MetricsQueryResult::NotFound;
            }
            encode_metrics(state.metrics.timeline(bloom), notice)
        }
        MetricsView::Seats => {
            let records = load_study_records(state.artifacts.as_mut(), &state.snapshot);
            encode_metrics(state.metrics.seats(|digest| records.get(digest).copied()), notice)
        }
        MetricsView::Dispatches => {
            let limit = usize::try_from(clamp(limit)).unwrap_or(usize::MAX);
            let from = from_sequence.unwrap_or(0);
            let rows: Vec<_> =
                state.metrics.dispatch_rows().into_iter().filter(|row| row.sequence > from).take(limit).collect();
            encode_metrics(rows, notice)
        }
    }
}

fn encode_metrics<T: serde::Serialize>(document: T, notice: Option<String>) -> MetricsQueryResult {
    match to_vec(&document) {
        Ok(document) => MetricsQueryResult::Ok { document, notice },
        Err(error) => MetricsQueryResult::Err { error: format!("metrics document encode failed: {error}") },
    }
}

fn spend_response(spend: &SpendWindow) -> SpendQueryResult {
    match to_vec(spend) {
        Ok(window) => SpendQueryResult::Ok { window },
        Err(error) => SpendQueryResult::Err { error: format!("spend window encode failed: {error}") },
    }
}

/// Resolve each admitted study evidence's `detail` to its [`StudyRecord`].
///
/// The fold and the snapshot hold digests, not columns; this is the read-time
/// lookup `report` / [`grade`] / [`measure`] already take as a closure. A
/// missing store, a missing digest, or bytes that are not a canonical study
/// record are all a miss — the honesty boundary, never a zero-cost stand-in.
fn load_study_records(
    artifacts: Option<&mut ArtifactsCapabilityState>,
    snapshot: &Snapshot,
) -> BTreeMap<Digest, StudyRecord> {
    let Some(artifacts) = artifacts else {
        return BTreeMap::new();
    };
    let mut records = BTreeMap::new();
    for bloom in snapshot.blooms.values() {
        for evidence in &bloom.evidence {
            if evidence.kind != EvidenceKind::StudyRecord || records.contains_key(&evidence.detail) {
                continue;
            }
            let GetResult::Ok { bytes, .. } = artifacts.get(lowercase_hex(evidence.detail.as_bytes())) else {
                continue;
            };
            let Ok(study) = from_bytes::<StudyRecord>(&bytes) else {
                continue;
            };
            // The untagged wire can coincidentally decode some other artifact;
            // only a round-trip back to the stored bytes is a study record.
            if to_vec(&study).ok().as_deref() != Some(bytes.as_slice()) {
                continue;
            }
            records.insert(evidence.detail, study);
        }
    }
    records
}

/// Open a second handle on the shared artifacts root. A store that will not
/// open disables study resolution for this process rather than failing
/// boot — the ledger is a grading surface, and a coordinator that cannot
/// read costs must still run blooms.
fn open_artifacts(configured: Option<&str>) -> Option<ArtifactsCapabilityState> {
    let root = resolve_root(configured);
    match ArtifactsCapabilityState::open(&root) {
        Ok(artifacts) => Some(artifacts),
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::control",
                root = %root.display(),
                %error,
                "artifacts store did not open; calibration and spend reads will leave study columns unaccounted",
            );
            None
        }
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

/// The bloom an admit mutates, when it names one.
///
/// Exhaustive over [`Fact`]: a new variant that carries a bloom must wait
/// behind that bloom's in-flight commit (#5168), and a variant that does not
/// must not block one.
fn event_bloom(event: &Event) -> Option<BloomId> {
    match &event.fact {
        Fact::Seal(spec) | Fact::GraphSeal { spec, .. } => Some(spec.id()),
        Fact::Supersede { successor, .. } => Some(successor.id()),
        Fact::Integrate { bloom, .. }
        | Fact::AdmitEvidence { bloom, .. }
        | Fact::Resolve { bloom, .. }
        | Fact::Land { bloom, .. }
        | Fact::AdoptAnswer { bloom, .. }
        | Fact::AttemptCompleted { bloom, .. }
        | Fact::AggregateReviewCompleted { bloom, .. }
        | Fact::AggregateVerifyCompleted { bloom, .. }
        | Fact::LandingRejected { bloom, .. }
        | Fact::GrantAttempts { bloom, .. }
        | Fact::VerifyFailed { bloom, .. }
        | Fact::AggregateReviewExecutorFault { bloom, .. }
        | Fact::FoldConflict { bloom, .. }
        | Fact::OperatorAdjudication { bloom, .. }
        | Fact::OperatorRepair { bloom, .. }
        | Fact::OperatorHold { bloom, .. }
        | Fact::OperatorRelease { bloom, .. }
        | Fact::VerifyHostFault { bloom, .. }
        | Fact::ResumeHostFault { bloom, .. }
        | Fact::SpliceAssembled { bloom, .. }
        | Fact::MemberExecutorFault { bloom, .. }
        | Fact::FoldRefused { bloom, .. }
        | Fact::ContainmentRefused { bloom, .. } => Some(*bloom),
        Fact::ObserveMainline { .. }
        | Fact::ObserveMainlineDiverged { .. }
        | Fact::RequestOrphanClaimRelease { .. }
        | Fact::CompleteOrphanClaimRelease { .. }
        | Fact::SurfaceOverlap { .. } => None,
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

fn membership_mutation(workpiece: &str, bloom: &BloomId) -> MembershipMutation {
    MembershipMutation { workpiece: workpiece.to_owned(), bloom: bloom.0.as_bytes().to_vec() }
}

/// The store-commit axes [`project`] builds: membership releases, claims, and
/// outbox payloads.
type ProjectedAxes = (Vec<MembershipMutation>, Vec<MembershipMutation>, Vec<OutboxPayload>);

/// Project a decided event's effects into the store commit's typed axes: the
/// membership releases and claims the `active_membership` table applies, and the
/// outbox payloads it enqueues. The snapshot-only effects carry no durable store
/// row — they are rebuilt on replay by `reduce` + `apply` from the journaled event.
fn project(decisions: &Decisions) -> Result<ProjectedAxes, WireError> {
    let mut releases = Vec::new();
    let mut claims = Vec::new();
    let mut outbox = Vec::new();
    for effect in &decisions.effects {
        match effect {
            Decision::ClaimMembership { workpiece, bloom } => {
                claims.push(membership_mutation(&workpiece.0, bloom));
            }
            Decision::ReleaseMembership { workpiece, bloom } => {
                releases.push(membership_mutation(&workpiece.0, bloom));
            }
            other => outbox.extend(outbox_payload(other)?),
        }
    }
    Ok((releases, claims, outbox))
}

/// The outbox row one effect enqueues, or `None` for a snapshot-only effect that
/// carries no durable row.
///
/// Split from [`project`] so the membership axes and the outbox axis are read
/// separately: the two membership arms mutate a table, every arm here serializes
/// a payload under a topic, and the classification of which effects are
/// snapshot-only is one list rather than a tail on a longer match.
fn outbox_payload(effect: &Decision) -> Result<Option<OutboxPayload>, WireError> {
    Ok(match effect {
        // The landing-receipt topic carries the receipt *and* the landed
        // bloom's membership: the receipt value names no members, so a
        // payload without them cannot reach the objects it belongs on after
        // a restart drains it (ADR-0149 §The receipt carries its members).
        Decision::EmitReceipt(projected) => Some(OutboxPayload::new(Topic::LandingReceipt, to_vec(projected)?)),
        Decision::RedispatchStage { bloom, question, answer, words } => Some(OutboxPayload::new(
            Topic::Redispatch,
            to_vec(&RedispatchPayload { bloom: bloom.0, question: *question, answer: *answer, words: words.clone() })?,
        )),
        Decision::DispatchAttempt { .. } => dispatch_attempt_outbox(effect)?,
        Decision::DispatchLand { bloom, expected_base, new_head } => {
            let payload = LandPayload { bloom: bloom.0, expected_base: *expected_base, new_head: *new_head };
            Some(OutboxPayload::new(Topic::Land, to_vec(&payload)?))
        }
        Decision::DispatchIntegration { bloom, base, members, adopt_from } => {
            let payload = IntegratePayload {
                bloom: bloom.0,
                base: *base,
                members: members.clone(),
                adopt_from: adopt_from.map(|predecessor| predecessor.0),
            };
            Some(OutboxPayload::new(Topic::Integrate, to_vec(&payload)?))
        }
        Decision::DispatchSplice { .. } => splice_outbox(effect)?,
        Decision::DispatchAggregateReview { bloom, transformation, roll, profile, configs } => {
            let payload = AggregateReviewPayload {
                profile: profile.clone(),
                bloom: bloom.0,
                transformation: transformation.clone(),
                pass: ReviewPass::from_roll(*roll),
                configs: configs.clone(),
            };
            Some(OutboxPayload::new(Topic::AggregateReview, to_vec(&payload)?))
        }
        Decision::DispatchAggregateVerify { bloom, transformation, profile, roll: _ } => {
            let payload = AggregateVerifyPayload {
                profile: profile.clone(),
                bloom: bloom.0,
                transformation: transformation.clone(),
            };
            Some(OutboxPayload::new(Topic::AggregateVerify, to_vec(&payload)?))
        }
        Decision::DispatchOrphanClaimRelease { request, target } => {
            let payload = OrphanClaimReleasePayload { request: *request, target: target.clone() };
            Some(OutboxPayload::new(Topic::OrphanClaimRelease, to_vec(&payload)?))
        }
        Decision::ClaimMembership { .. }
        | Decision::ReleaseMembership { .. }
        | Decision::RecordOrphanClaimRelease { .. }
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
        | Decision::RecordVerifyProof { .. }
        | Decision::RecordVerifyReuse { .. }
        | Decision::RecordLandingRoll { .. }
        | Decision::SetUnresolved { .. }
        | Decision::RecordReviewPark { .. }
        | Decision::RecordWedge { .. }
        | Decision::RevokeResolution { .. }
        | Decision::AdvanceMainline { .. }
        | Decision::RecordObservation { .. }
        | Decision::RecordStageCatalog { .. }
        | Decision::RecordCompositionFinding { .. }
        | Decision::RecordAdjudication { .. }
        | Decision::RecordOperatorRepair { .. }
        | Decision::RecordOperatorHold { .. }
        | Decision::RecordOperatorRelease { .. }
        | Decision::DeferDispatch { .. }
        | Decision::DeferAggregate { .. }
        | Decision::RecordSpendQuiesce { .. }
        | Decision::RecordMemberDependencies { .. }
        | Decision::RecordHostFault { .. }
        | Decision::ClearHostFault { .. }
        | Decision::RecordCandidateVehicle { .. }
        | Decision::RecordMemberMachinery { .. } => None,
    })
}

fn dispatch_attempt_outbox(effect: &Decision) -> Result<Option<OutboxPayload>, WireError> {
    let Decision::DispatchAttempt {
        bloom,
        workpiece,
        stage,
        transformation,
        scope_revision,
        candidate,
        profile,
        configs,
    } = effect
    else {
        return Ok(None);
    };
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
    Ok(Some(OutboxPayload::new(Topic::Dispatch, to_vec(&payload)?)))
}

fn splice_outbox(effect: &Decision) -> Result<Option<OutboxPayload>, WireError> {
    let Decision::DispatchSplice { bloom, workpiece, base, members, adopt_from } = effect else {
        return Ok(None);
    };
    let payload = SplicePayload {
        bloom: bloom.0,
        workpiece: workpiece.clone(),
        base: *base,
        members: members.clone(),
        adopt_from: adopt_from.map(|predecessor| predecessor.0),
    };
    Ok(Some(OutboxPayload::new(Topic::Splice, to_vec(&payload)?)))
}

/// Commit a just-reduced admit, or reply immediately when it needs no new row.
///
/// A duplicate is already applied. A rejected operator repair whose tagged
/// key is already journaled (or in flight) answers the current refusal
/// without occupying the content-derived key a later accepted retry needs
/// (#5107). Every other decision goes through the ordinary claim-or-commit
/// path, with a rejected repair retagged first so the journal row cannot
/// shadow that retry.
fn finish_admit(
    state: &mut ControlCoreState,
    ctx: &mut NativeCtx<'_, Manual>,
    inbound: InboundMail,
    raw: Vec<u8>,
    event: Event,
    decisions: Decisions,
) {
    if matches!(decisions.outcome, Outcome::Duplicate) {
        inbound.reply(&admit_duplicate(&event.idempotency_key.0));
        return;
    }
    match retag_rejected_repair(&state.snapshot, |key| state.inflight_for_key(key), raw, event, &decisions) {
        Ok(Some((raw, event))) => state.gate_or_commit(ctx, inbound, raw, event, decisions),
        Ok(None) => {
            inbound.reply(&admit_ok(&decisions.outcome));
        }
        Err(error) => {
            inbound.reply(&AdmitResult::Err { error: format!("admit encode failed: {error}") });
        }
    }
}

/// Retag a rejected operator repair so the content-derived key stays free.
///
/// `None` means this refusal is already journaled (or a same-tag commit is
/// in flight) and the caller should answer the current outcome without a
/// new row. Any other fact, and any accepted repair, is returned unchanged.
fn retag_rejected_repair(
    snapshot: &Snapshot,
    inflight: impl Fn(&str) -> usize,
    raw: Vec<u8>,
    mut event: Event,
    decisions: &Decisions,
) -> Result<Option<(Vec<u8>, Event)>, WireError> {
    let Outcome::OperatorRepairRejected(error) = &decisions.outcome else {
        return Ok(Some((raw, event)));
    };
    let tagged = rejected_repair_key(&event.idempotency_key, error);
    if snapshot.seen.contains(&tagged) || inflight(&tagged.0) > 0 {
        return Ok(None);
    }
    event.idempotency_key = tagged;
    Ok(Some((to_vec(&event)?, event)))
}

/// Journal key for a refused operator repair: the submitted key plus the
/// refusal class, so a `Held` row cannot occupy the key a later accepted
/// submit of the same content needs, and two different refusal classes of
/// the same repair stay distinct rows.
fn rejected_repair_key(original: &IdempotencyKey, error: &OperatorRepairError) -> IdempotencyKey {
    IdempotencyKey(format!("{}:rejected:{}", original.0, rejected_repair_tag(error)))
}

fn rejected_repair_tag(error: &OperatorRepairError) -> &'static str {
    match error {
        OperatorRepairError::UnknownOrInactiveBloom => "UnknownOrInactiveBloom",
        OperatorRepairError::BlankReason => "BlankReason",
        OperatorRepairError::BlankOperator => "BlankOperator",
        OperatorRepairError::NotAMember(_) => "NotAMember",
        OperatorRepairError::AlreadyResolved(_) => "AlreadyResolved",
        OperatorRepairError::NotWedged(_) => "NotWedged",
        OperatorRepairError::UnapprovedMember(_) => "UnapprovedMember",
        OperatorRepairError::Held => "Held",
    }
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
    aether_bloomery::encode_hex(bytes)
}

/// The source mail that classifies the live head against the snapshot's
/// current mainline — genesis or an encode failure is the boot-bind path
/// (any observed head is a fast-forward).
/// Admit a cadence resume for every member currently held on a host fault.
///
/// Fire-and-forget through the ordinary [`Admit`] door so the fact is
/// journaled and the dispatch is decided by the reducer, not by this tick.
/// The key names the hold's evidence digest: two ticks against the same
/// hold collapse to `Duplicate`, and a later miss (new evidence) is a new
/// key.
fn resume_host_faults(state: &ControlCoreState, ctx: &mut NativeCtx<'_, Manual>) {
    for record in state.snapshot.blooms.values() {
        if record.operator_hold.is_some() {
            continue;
        }
        for (workpiece, hold) in &record.host_faults {
            let event = Event {
                idempotency_key: resume_host_fault_key(&record.spec.id(), workpiece, &hold.evidence),
                fact: Fact::ResumeHostFault { bloom: record.spec.id(), workpiece: workpiece.clone() },
            };
            match to_vec(&event) {
                Ok(bytes) => ctx.actor::<ControlCore>().send_detached(&Admit { event: bytes }),
                Err(error) => tracing::warn!(
                    target: "aether_chassis_bloomery::control",
                    %error,
                    workpiece = %workpiece.0,
                    "host-fault resume did not encode"
                ),
            }
        }
    }
}

/// The admit key for one cadence resume of a host-fault hold.
fn resume_host_fault_key(
    bloom: &BloomId,
    workpiece: &aether_bloomery::WorkpieceId,
    evidence: &Digest,
) -> IdempotencyKey {
    IdempotencyKey(format!(
        "resume-host-fault-{}-{}-{}",
        lowercase_hex(bloom.0.as_bytes()),
        workpiece.0,
        lowercase_hex(evidence.as_bytes()),
    ))
}

fn observe_mail(mainline: &Digest) -> ObserveMainline {
    ObserveMainline { relative_to: to_vec(mainline).unwrap_or_default() }
}

/// The admit key for one observation: the pair of observed head and the
/// mainline it was classified against (#4938). A head already seen against a
/// *different* mainline is a new key, so a regression can recover by
/// re-observing the true head instead of reducing to `Duplicate` forever.
fn observe_mainline_key(head: &Digest, mainline: &Digest) -> IdempotencyKey {
    IdempotencyKey(format!(
        "observe-mainline-{}-at-{}",
        lowercase_hex(head.as_bytes()),
        lowercase_hex(mainline.as_bytes()),
    ))
}

/// Whether this (head, current mainline) pair has already been journaled.
/// The poll skip consults `seen` rather than pointer equality so a
/// `MainlineHeld` observation (recorded head, mainline still behind) is not
/// re-admitted every interval.
fn observation_already_admitted(snapshot: &Snapshot, head: &Digest) -> bool {
    snapshot.seen.contains(&observe_mainline_key(head, &snapshot.mainline))
}

#[cfg(test)]
mod tests {
    use aether_bloomery::testing::digest;
    use aether_bloomery::{
        BloomDraft, BloomId, BloomRecord, CandidateRef, Decisions, Digest, Event, Evidence, EvidenceKind, Fact,
        IdempotencyKey, OperatorRepair, OperatorRepairError, Outcome, QueryResult, Snapshot, StudyCost, StudyRecord,
        WorkpieceId,
    };
    use aether_data::wire::to_vec;

    use super::{
        load_study_records, lowercase_hex, observation_already_admitted, observe_mainline_key, rejected_repair_key,
        release_response, retag_rejected_repair,
    };
    use crate::artifacts::{ArtifactsCapabilityState, PutResult};

    fn snapshot_with_study(subject: Digest, detail: Digest) -> Snapshot {
        let record = BloomRecord {
            evidence: vec![Evidence { subject, kind: EvidenceKind::StudyRecord, detail }],
            ..BloomRecord::empty(BloomDraft::default().seal())
        };
        let mut snapshot = Snapshot::default();
        snapshot.blooms.insert(BloomId(digest(1)), record);
        snapshot
    }

    #[test]
    fn observe_mainline_key_uses_lowercase_hex() {
        assert_eq!(lowercase_hex(&[0, 15, 160, 255]), "000fa0ff");
    }

    // Tripwire: the admit key names both the observed head and the mainline
    // it was classified against (#4938). A head-only key made re-observing
    // the true head after a regression `Duplicate` forever.
    #[test]
    fn observe_mainline_key_pairs_head_with_current_mainline() {
        let head = Digest::from_bytes([0x0a; 32]);
        let mainline = Digest::from_bytes([0x0b; 32]);
        let other = Digest::from_bytes([0x0c; 32]);

        assert_eq!(
            observe_mainline_key(&head, &mainline),
            IdempotencyKey(format!("observe-mainline-{}-at-{}", "0a".repeat(32), "0b".repeat(32))),
        );
        assert_ne!(
            observe_mainline_key(&head, &mainline),
            observe_mainline_key(&head, &other),
            "the same head against a different mainline is a different key",
        );
    }

    // Tripwire: the poll skip keys on the journaled (head, mainline) pair,
    // not on both pointers naming the head (#4938). MainlineHeld records
    // observed=head while mainline stays behind; skipping only when
    // `head == observed && head == mainline` re-admitted that pair every
    // 30s and fired `admit_duplicate` for the life of the bloom.
    #[test]
    fn poll_skips_a_held_head_already_admitted_against_current_mainline() {
        let head = Digest::from_bytes([0x0a; 32]);
        let mainline = Digest::from_bytes([0x0b; 32]);
        let other = Digest::from_bytes([0x0c; 32]);
        let mut snapshot = Snapshot::new(mainline);
        snapshot.observed = head;
        snapshot.seen.insert(observe_mainline_key(&head, &mainline));

        assert!(
            observation_already_admitted(&snapshot, &head),
            "MainlineHeld has already journaled this (head, mainline) pair",
        );

        snapshot.mainline = other;
        assert!(
            !observation_already_admitted(&snapshot, &head),
            "the same head against a different mainline is still a new admit",
        );
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

    // Tripwire (#5107): a refused repair journals under the submitted key plus
    // the refusal class, not under the content key the operator will resubmit.
    // Collapsing every refusal onto one `:rejected` suffix would make a `Held`
    // row look like a prior `NotWedged` of the same content; omitting the
    // suffix is the incident — the refused row occupies the retry.
    #[test]
    fn a_rejected_repair_key_includes_the_refusal_class() {
        let original = IdempotencyKey("aether.bloomery.repair:bloom:digest".into());
        let held = rejected_repair_key(&original, &OperatorRepairError::Held);
        let not_wedged = rejected_repair_key(&original, &OperatorRepairError::NotWedged(WorkpieceId("wp".into())));

        assert_eq!(held.0, "aether.bloomery.repair:bloom:digest:rejected:Held");
        assert_eq!(not_wedged.0, "aether.bloomery.repair:bloom:digest:rejected:NotWedged");
        assert_ne!(held, not_wedged, "two refusal classes of the same repair are two rows");
        assert_ne!(held, original, "the content key itself is not the journal key");
    }

    fn repair_event(key: &str) -> Event {
        Event {
            idempotency_key: IdempotencyKey(key.into()),
            fact: Fact::OperatorRepair {
                bloom: BloomId(digest(1)),
                repair: OperatorRepair {
                    workpiece: WorkpieceId("wp".into()),
                    candidate: CandidateRef { tree: digest(2), checkout: digest(3) },
                    reason: "one-line fix".into(),
                    operator: "eve".into(),
                },
            },
        }
    }

    // The plausible bug: a `Held` refusal is journaled under the content key
    // the operator will resubmit, so the retry after release reduces to
    // Duplicate. Retag must rewrite both the event and the encoded bytes —
    // replay folds `seen` from the bytes, not the store column.
    #[test]
    fn a_rejected_repair_is_retagged_in_the_event_bytes() {
        let event = repair_event("aether.bloomery.repair:same");
        let raw = to_vec(&event).expect("a reducer event encodes");
        let decisions =
            Decisions { outcome: Outcome::OperatorRepairRejected(OperatorRepairError::Held), effects: Vec::new() };

        let (rewritten, tagged) =
            retag_rejected_repair(&Snapshot::default(), |_| 0, raw.clone(), event.clone(), &decisions)
                .expect("retag encodes the tagged event")
                .expect("an unseen refusal is journaled under a new key");

        assert_eq!(tagged.idempotency_key.0, "aether.bloomery.repair:same:rejected:Held");
        assert_ne!(rewritten, raw, "replay reads the event bytes, so they must carry the tagged key");
        assert_eq!(
            retag_rejected_repair(
                &Snapshot::default(),
                |_| 0,
                raw.clone(),
                event.clone(),
                &Decisions {
                    outcome: Outcome::OperatorRepairAccepted {
                        bloom: BloomId(digest(1)),
                        workpiece: WorkpieceId("wp".into()),
                        candidate: digest(2),
                    },
                    effects: Vec::new(),
                }
            )
            .expect("retag of an accepted repair cannot fail")
            .expect("an accepted repair is unchanged")
            .1
            .idempotency_key,
            event.idempotency_key,
            "only a refused repair is retagged",
        );

        let mut seen = Snapshot::default();
        seen.seen.insert(tagged.idempotency_key);
        assert!(
            retag_rejected_repair(&seen, |_| 0, raw.clone(), event.clone(), &decisions)
                .expect("retag of an already-journaled refusal cannot fail")
                .is_none(),
            "a refusal already journaled under its tagged key is not committed again",
        );
        assert!(
            retag_rejected_repair(
                &Snapshot::default(),
                |key| usize::from(key.ends_with(":rejected:Held")),
                raw,
                event,
                &decisions
            )
            .expect("retag of an in-flight tagged commit cannot fail")
            .is_none(),
            "an in-flight tagged commit is treated as already journaled",
        );
    }

    // The plausible bug: the resolver answers a missing digest with a
    // zero-cost record, so an unreadable artifact looks like a free attempt
    // and a calibration read cannot tell "this seat is free" from "we have
    // no measurement".
    #[test]
    fn an_unresolvable_artifact_is_a_miss_not_a_zero_cost_record() {
        let dir = tempfile::tempdir().expect("temp artifacts root");
        let mut artifacts = ArtifactsCapabilityState::open(dir.path()).expect("artifacts store opens");
        let snapshot = snapshot_with_study(digest(2), digest(3));

        let records = load_study_records(Some(&mut artifacts), &snapshot);
        assert!(records.is_empty(), "a missing digest must not become a study record");
        assert_eq!(load_study_records(None, &snapshot).len(), 0, "and neither must a missing store");
    }

    // The plausible bug: the resolver decodes the artifact but keys it by
    // StudyRecord::id (domain-tagged) instead of the content-store hash the
    // evidence detail carries, so report() asks for the store hash and
    // never finds the record that is sitting in the map.
    #[test]
    fn a_stored_study_record_resolves_under_its_content_store_hash() {
        let dir = tempfile::tempdir().expect("temp artifacts root");
        let mut artifacts = ArtifactsCapabilityState::open(dir.path()).expect("artifacts store opens");
        let bloom = BloomId(digest(1));
        let subject = digest(2);
        let study = StudyRecord {
            bloom,
            subject,
            cost: StudyCost { cost_micro_usd: 7_000, duration_millis: 4_500, ..StudyCost::default() },
        };
        let bytes = to_vec(&study).expect("a study record wire-encodes");
        let PutResult::Ok { digest: stored } = artifacts.put(&bytes, &[]) else {
            panic!("the study artifact stores");
        };
        let detail = Digest::of_wire_bytes(&bytes);
        assert_eq!(stored, lowercase_hex(detail.as_bytes()), "the store hash is the untagged sha256");

        let records = load_study_records(Some(&mut artifacts), &snapshot_with_study(subject, detail));
        assert_eq!(records.get(&detail), Some(&study), "the reported columns are the record's");
        assert_eq!(records.len(), 1);
    }
}
