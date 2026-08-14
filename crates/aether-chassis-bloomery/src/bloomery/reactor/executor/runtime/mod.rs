//! The runtime for the executor dispatch-reactor capability (ADR-0149 migration
//! step 2 — issue #3505).
//!
//! A poll-driven loop that turns the reducer's dispatch decisions into running
//! work and matched results back into admitted facts:
//!
//! 1. **Drain + submit.** Each tick drains the store's
//!    [`Topic::Dispatch`]
//!    outbox topic (its own connection, so the intake registry the pull side
//!    reads/consumes is one store), decodes each
//!    [`DispatchPayload`], submits it through the
//!    [`ExecutorShell`] with a sequence-derived idempotency nonce, and records the
//!    intake context ([`dispatch_and_record`]). The delivered prefix is acked so a
//!    dispatch is submitted once; a submit failure stops the prefix so the entry
//!    re-drains.
//! 2. **Pull + admit.** The same tick runs one [`run_intake_cycle`] over the
//!    tracked handles: a completed run's evidence is decoded from its artifact name
//!    ([`NameEvidenceClaims`]), the broker binds it to the displayed digest, and
//!    every admitted attempt result is forwarded to the `aether.bloomery.control`
//!    actor as an [`Admit`] — where the reducer advances the member's cursor and
//!    (via the dispatch topic) dispatches its next stage.
//!
//! Config-gated: mounts whenever either backend is usable — fully configured
//! (GitHub + local) mounts a `RoutingExecutor` over both, unconfigured + local
//! enabled mounts the local backend only (Actions lanes fail fast naming the
//! missing knobs), and unconfigured + local disabled mounts disabled — no shell,
//! no store, no timer — so a zero-secret dev boot with no local lane neither
//! errors nor spins.
//!
//! Store ownership: this reactor opens its **own** [`SqliteStore`] on the shared
//! `AETHER_STORE_PATH` because the intake helpers ([`dispatch_and_record`] /
//! [`run_intake_cycle`]) drive the registry in-process over a `StoreBackend`, not
//! over mail. It is the sole writer of the `outstanding_orders` table and acks
//! only its own dispatch topic; the `busy_timeout` on every connection serializes
//! the rare concurrent WAL write with the `StoreCapability`'s. Routing the outbox
//! drain/ack through the store's `DrainOutbox`/`AckOutbox` mail (as the mirror
//! reactor does) is a follow-up once the registry ops gain a mail surface.

use std::fmt::Write as _;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aether_actor::Addressable;
use aether_actor::runtime;
use aether_bloomery::{
    Admit, AggregateReviewPayload, AggregateVerifyPayload, BloomId, ConfigRegistry, ConfigScopes, Digest,
    DispatchPayload, ExecutionStatus, Fact, ModelOverride, Nonce, RedispatchPayload, ReviewPass, SharedCorrespondence,
    StageId, StageVerdict, TimeoutRecord, Topic, VerifyFailureSet, WorkHandle, WorkpieceId,
};
use aether_bloomery_github::{GitObjectId, candidate_ref_name, short_hex};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::ExecutorReactorCapability;
use crate::artifacts::{ArtifactsCapabilityState, PutResult, resolve_root};
use crate::bloomery::CONSTRUCT_IMPLEMENT_COMMAND;
use crate::bloomery::ExecutorShell;
#[cfg(test)]
use crate::bloomery::GithubConnectionConfig;
use crate::bloomery::dispatch_model;
use crate::bloomery::executor::OutstandingDispatch;
use crate::bloomery::intake::{
    Admission, AdmitDecision, AdmitSink, CycleReport, DispatchRecord, NameEvidenceClaims, UploadedEvidence,
    admit_uploaded, dispatch_and_record, dispatch_nonce, run_intake_cycle,
};
use crate::bloomery::outbox::TopicOutbox;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
#[cfg(any(test, feature = "testing"))]
use crate::bloomery::study::{StudyAdmitDecision, UploadedStudyRecord, admit_study};
#[cfg(any(test, feature = "testing"))]
use crate::bloomery::testing::{ScriptedEvidence, ScriptedEvidenceResult, ScriptedUpload};
use crate::bloomery::{CoordinatorConfig, ExecutorReactorSetup};
use crate::control::ControlCore;
use crate::store::{SqliteStore, StoreBackend, StoreConfigError, resolve_config};

mod strand;

use strand::readopt_stranded_dispatches;

/// The self-addressed wake the poll timer fires each interval; its handler drains
/// the dispatch topic and pulls matched results. Zero-field — the timer carries
/// only the schedule.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.executor.dispatch_tick")]
pub struct DispatchTick {}

/// The transient-re-drive backoff base and cap (#3593): a sustained transient
/// outage decays the poll-tick-flat re-drive cadence geometrically from `BASE`
/// (the same order as the poll interval) up to `CAP`, rather than hammering at a
/// flat 5s.
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_CAP: Duration = Duration::from_mins(5);

/// The geometric backoff delay for the `failures`-th consecutive transient
/// failure of the same head outbox entry — `BASE * 2^(failures - 1)`, clamped at
/// `CAP`. Pure so it is unit-testable without a clock; `on_dispatch_tick` is the
/// only caller that feeds it a live `Instant`.
fn backoff_delay(failures: u32) -> Duration {
    BACKOFF_BASE.saturating_mul(1u32.checked_shl(failures.saturating_sub(1)).unwrap_or(u32::MAX)).min(BACKOFF_CAP)
}

/// The transient-re-drive backoff cursor: which outbox `sequence` is failing, how
/// many consecutive times, and the `Instant` before which the next drain is
/// skipped. A permanent-refusal park or a successful drain clears it; a changed
/// head sequence resets `failures` to 1 rather than continuing to grow a stale
/// count.
struct BackoffCursor {
    sequence: u64,
    failures: u32,
    retry_after: Instant,
}

/// Advance the backoff cursor for a drain's outcome: `None` (nothing to drain, a
/// success, or a permanent-refusal park) clears it; a transient failure of the
/// same head `sequence` as `current` grows the consecutive-failure count, while a
/// transient failure of a *different* sequence (the prior head cleared some other
/// way) resets it to 1 rather than continuing to grow a stale count. Factored out
/// of `on_dispatch_tick` so the cursor transition is unit-testable without a mail
/// harness; only `retry_after`'s `Instant::now()` is impure.
fn next_backoff(current: Option<&BackoffCursor>, transient_failure: Option<u64>) -> Option<BackoffCursor> {
    let sequence = transient_failure?;
    let failures = match current {
        Some(cursor) if cursor.sequence == sequence => cursor.failures + 1,
        _ => 1,
    };
    Some(BackoffCursor { sequence, failures, retry_after: Instant::now() + backoff_delay(failures) })
}

/// A tracked dispatch handle plus its staleness bookkeeping (#3635): `first_seen`
/// marks when the handle was added to `tracked`, and `stale_warned` guards the
/// tick's stale-sweep so a still-wedged handle warns once per crossing rather
/// than every poll tick.
struct TrackedHandle {
    handle: WorkHandle,
    first_seen: Instant,
    stale_warned: bool,
    /// Guards the whole handling of an expired order this build cannot
    /// terminate, the way `stale_warned` guards the staleness one. Such an order
    /// stays outstanding, so every later expiry sweep selects it again: without
    /// a latch its report buries the first one under the poll cadence, and its
    /// reclaiming cancel is reissued at that same cadence forever.
    unterminable_reported: bool,
}

impl TrackedHandle {
    fn new(handle: WorkHandle, first_seen: Instant) -> Self {
        Self { handle, first_seen, stale_warned: false, unterminable_reported: false }
    }
}

/// The current wall clock in Unix milliseconds — the reading one tick compares
/// every persisted deadline against and stamps every order it records with
/// (ADR-0177).
///
/// Taken once per tick rather than per order, so the dispatches and expiries of
/// a single tick share one instant and cannot disagree about what "now" was. A
/// clock before the epoch is not a reading any deadline arithmetic can use, so
/// it reads as `0` — which defers every expiry rather than terminating anything
/// on a number that means nothing: `0` is behind every deadline a dispatch under
/// the same clock would have stamped. Deadlines stop enforcing until the host's
/// clock is usable again, and no order is cancelled on a fiction in the meantime.
fn now_unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// The verdict a timed-out order of `stage` admits under (ADR-0177), paired with
/// the typed verifier set the intake's ADR-0178 transport invariant requires for
/// it. `None` when no verdict in the current vocabulary states the fact.
///
/// A member stage and `AggregateVerify` are `VerificationFailed`: the attempt
/// did not produce the passing evidence its gate wanted, which is the same fact
/// a failed run states, so it spends the same sealed attempt or repair budget
/// and reaches the same wedge — no parallel retry authority.
///
/// A member `Verify` names no verifier at all. A timeout cannot know which one
/// would have failed, and it must not guess: the reducer prices a named identity
/// as a defect in the candidate and sends the member to `Refine` to repair it,
/// which is the wrong answer for a lane that was killed on the clock before it
/// judged anything. The empty set is the honest report — the gate rendered no
/// verdict — and the reducer answers it by re-running `Verify` on the member's
/// own Verify budget, wedging there once that budget is spent.
///
/// `AggregateReview` is deliberately `None`. ADR-0177 routes it to ADR-0176's
/// `ExecutorFault` — a review lane that never answered produced no judgement of
/// the fold, which is the same fact as one that reported an environment failure,
/// and must charge the same fold-fault ledger rather than reopening members.
/// That vocabulary is issue #4738's to introduce; synthesising a
/// `VerificationFailed` here instead would charge every member a repair lap for
/// a critic that never ran, so this build defers the aggregate-review timeout
/// rather than recording the wrong thing.
///
/// Exhaustive over [`StageId`] rather than wildcarded. `None` here means the
/// order never terminates, and the stages that reach it split into two very
/// different reasons for that — one deferred vocabulary and a set of stages no
/// executor order carries at all. A wildcard reads a stage that later becomes
/// dispatchable into the second group silently; naming every variant makes it a
/// compile error instead.
fn timeout_verdict(stage: StageId) -> Option<(StageVerdict, VerifyFailureSet)> {
    match stage {
        StageId::Verify | StageId::Construct | StageId::Refine | StageId::Reconcile | StageId::AggregateVerify => {
            Some((StageVerdict::VerificationFailed, VerifyFailureSet::EMPTY))
        }
        // No verdict, for two different reasons. `AggregateReview`'s is deferred,
        // as above. The rest are never dispatched to an executor at all — the
        // pre-line stages, the per-member `Review` the member walk does not enter
        // (`StageCatalog::MEMBER_LINE` ends at `Verify`), and the bloom-level
        // tail the coordinator performs itself — so no order carries one and
        // none can expire.
        StageId::AggregateReview
        | StageId::Sketch
        | StageId::Scope
        | StageId::Approve
        | StageId::Review
        | StageId::Integrate
        | StageId::Land
        | StageId::Study => None,
    }
}

/// Render a digest as the lowercase-hex parent string the artifact store records
/// — the derivation edge from a timeout record to the subject it accounts for.
fn digest_to_parent(digest: &Digest) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest.as_bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Put a timeout record's canonical bytes into the content store, returning the
/// address the synthesised evidence details them by — or `None` when the expiry
/// must leave the order live and retry on the next tick.
///
/// The address is sha256 over the record's canonical wire bytes, which is
/// exactly the key the artifacts store files them under: `put` hashes the same
/// bytes the same way, so a `detail` minted here resolves against the store a
/// wedge later reads. It is computed rather than read back out of the reply so
/// the no-store host below can still name where the bytes belong, and it is
/// deliberately *not* [`TimeoutRecord::id`] — the value vocabulary's typed
/// address hashes a length-prefixed domain tag ahead of the same bytes, so an
/// evidence detail taken from it would point at nothing any store holds. The
/// local lane mints its evidence detail the same way ([`Digest::of_wire_bytes`]
/// over the bytes it wrote).
///
/// Two "no store" shapes, answered differently on purpose. A host with no
/// artifacts store configured never had anywhere to put the bytes and never
/// will, so refusing here would leave the order outstanding forever — which is
/// the bug being fixed. The address is a pure function of the record, so the
/// evidence still names where the artifact belongs; the bytes are simply
/// unretrievable, and the hole is loud in the log (the same trade `record_cost`
/// makes for study rows). A store that is present and *faults* is transient, so
/// that one leaves the order live to retry on the next tick.
fn store_timeout_record(artifacts: Option<&mut ArtifactsCapabilityState>, record: &TimeoutRecord) -> Option<Digest> {
    let Ok(bytes) = to_vec(record) else {
        tracing::error!(
            target: "aether_chassis_bloomery::executor",
            nonce = %record.nonce.0,
            "timeout record did not encode; leaving the order live to retry",
        );
        return None;
    };
    let address = Digest::of_wire_bytes(&bytes);

    let Some(artifacts) = artifacts else {
        tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            nonce = %record.nonce.0,
            "no artifacts store configured; the timeout terminates the order but its record bytes are unretrievable",
        );
        return Some(address);
    };
    match artifacts.put(&bytes, &[digest_to_parent(&record.subject)]) {
        PutResult::Ok { .. } => Some(address),
        PutResult::Err { error } => {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                nonce = %record.nonce.0,
                ?error,
                "timeout record was not stored; leaving the order live to retry",
            );
            None
        }
    }
}

/// Terminate every outstanding order whose persisted deadline has passed
/// (ADR-0177), returning the [`Admit`]s to forward to the control core.
///
/// Runs *after* [`run_intake_cycle`], so an order whose evidence arrived at the
/// boundary has already been admitted and consumed and cannot be selected here.
/// For each order that is still pending past its deadline: cancel the run
/// idempotently, store the deterministic [`TimeoutRecord`] the synthesised
/// evidence details, and put the result through the ordinary intake broker,
/// which consumes the order exactly once.
///
/// Every step before the admission is ordered so a fault leaves the order
/// durable and the whole expiry retryable on the next tick: the cancel is
/// idempotent, the record's address is a pure function of the order, and the
/// broker's consume-once is what makes the retry admit nothing twice. Late
/// worker evidence for a consumed order refuses as an unknown nonce, which is
/// the same answer a replay gets.
///
/// Retryable is not the same as repeated forever. A fault is retried because the
/// next sweep may get a different answer; the three ways an expiry ends without
/// terminating its order — an undecodable row, a deferred stage verdict, an
/// intake refusal — will get the same answer every time, so each is reported and
/// reclaimed exactly once per process (see [`reported_unterminable`]).
fn expire_overdue_orders(
    stores: Stores<'_>,
    executor: &ExecutorShell,
    tracked: &mut Vec<TrackedHandle>,
    now_unix_millis: u64,
) -> Vec<Admit> {
    let Stores { store, mut artifacts } = stores;
    let expired = match store.list_expired_orders(now_unix_millis) {
        Ok(expired) => expired,
        Err(error) => {
            tracing::warn!(target: "aether_chassis_bloomery::executor", %error, "expired-order read failed; deadlines re-check next tick");
            return Vec::new();
        }
    };

    let mut admits = Vec::new();
    for order in expired {
        let deadline = order.deadline_unix_millis;
        let nonce = Nonce(order.nonce.clone());
        // An order this build already cancelled and then could not terminate is
        // still outstanding, so every sweep after it selects the same row again.
        // Idempotence makes a repeat cancel *harmless*, not free: on the Actions
        // lane each one probes both wrappers over the network for a run that
        // will never change again, once per poll interval for the life of the
        // process.
        if reported_unterminable(tracked, &nonce) {
            continue;
        }
        // Cancel first, verdict second, and before the decode. Whether this
        // build can *account* for the expiry is a separate question from whether
        // the run may keep going: an overdue order still has a child burning
        // wall clock and a scratch worktree checked out behind it, and neither a
        // stage whose verdict vocabulary has not landed yet nor a row that no
        // longer decodes is a reason to leave those running until the process
        // exits. The nonce is a plain column, so the cancel needs none of the
        // decoding below.
        if let Err(error) = executor.cancel(&WorkHandle::new(nonce.clone())) {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                nonce = %nonce.0,
                %error,
                "expired order's cancel failed; leaving it live to retry",
            );
            continue;
        }
        let Some(record) = DispatchRecord::from_stored(&order) else {
            latch_unterminable(tracked, &nonce);
            tracing::error!(
                target: "aether_chassis_bloomery::executor",
                nonce = %nonce.0,
                "expired order did not decode into a dispatch record; its run was cancelled, but the order cannot be terminated by this build",
            );
            continue;
        };
        let Some((verdict, failed_verifiers)) = timeout_verdict(record.stage) else {
            warn_deferred_timeout(tracked, &record, deadline);
            continue;
        };

        let timeout = TimeoutRecord {
            bloom: record.bloom,
            // A bloom-level lane carries no member axis, and the empty
            // workpiece the dispatch fills in is that absence, not a member
            // named "".
            workpiece: (!record.workpiece.0.is_empty()).then(|| record.workpiece.clone()),
            stage: record.stage,
            nonce: record.nonce.clone(),
            subject: record.displayed_digest,
            deadline_unix_millis: deadline,
        };
        let Some(detail) = store_timeout_record(artifacts.as_deref_mut(), &timeout) else {
            continue;
        };

        let upload = UploadedEvidence {
            nonce: record.nonce.clone(),
            subject: record.displayed_digest,
            verdict,
            detail,
            candidate: None,
            findings: None,
            failed_verifiers,
            cost: None,
            calls: None,
        };
        match admit_uploaded(store, &upload) {
            Ok(AdmitDecision::Admitted(admission)) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    nonce = %record.nonce.0,
                    workpiece = %record.workpiece.0,
                    stage = ?record.stage,
                    deadline_unix_millis = deadline,
                    "dispatched run outlived its sealed execution limit; cancelled and recorded as a failed attempt",
                );
                admits.push(admission.admit);
                tracked.retain(|tracked_handle| tracked_handle.handle.nonce != record.nonce);
            }
            Ok(AdmitDecision::Refused(refusal)) => {
                // A refusal is a judgement about the order's own stored columns,
                // so the same order refuses the same way on every later sweep —
                // latch it with the other two unterminable shapes rather than
                // re-logging and re-cancelling at the poll cadence.
                latch_unterminable(tracked, &record.nonce);
                tracing::error!(
                    target: "aether_chassis_bloomery::executor",
                    nonce = %record.nonce.0,
                    ?refusal,
                    "the intake refused a timeout for this order; it cannot be terminated by this build",
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    nonce = %record.nonce.0,
                    %error,
                    "timeout admission faulted; leaving the order live to retry",
                );
            }
        }
    }
    admits
}

/// Whether the expiry sweep has already cancelled `nonce` and found it could not
/// terminate the order behind it — a row that no longer decodes, a stage whose
/// timeout verdict is deferred, or an admission the intake refuses. All three
/// leave the order outstanding, so every later sweep re-selects it; the latch is
/// what makes the report and the reclaiming cancel one-shot rather than one per
/// poll tick.
///
/// Kept on the tracked handle rather than in a set of its own because every
/// outstanding order already has one: a live dispatch tracks its handle, and
/// [`seed_tracked`] re-seeds one per outstanding nonce after a restart. A nonce
/// with no handle reads as unlatched, which reports and reclaims rather than
/// silently skipping an order nothing else is watching.
fn reported_unterminable(tracked: &[TrackedHandle], nonce: &Nonce) -> bool {
    tracked.iter().any(|tracked_handle| tracked_handle.handle.nonce == *nonce && tracked_handle.unterminable_reported)
}

/// Latch `nonce` against [`reported_unterminable`]. A no-op for an untracked
/// nonce, which cannot be latched and is reported again next sweep.
fn latch_unterminable(tracked: &mut [TrackedHandle], nonce: &Nonce) {
    if let Some(tracked_handle) = tracked.iter_mut().find(|tracked_handle| tracked_handle.handle.nonce == *nonce) {
        tracked_handle.unterminable_reported = true;
    }
}

/// Report an expired order this build has no verdict vocabulary for, once per
/// tracked handle rather than once per tick.
fn warn_deferred_timeout(tracked: &mut [TrackedHandle], record: &DispatchRecord, deadline_unix_millis: u64) {
    latch_unterminable(tracked, &record.nonce);
    tracing::warn!(
        target: "aether_chassis_bloomery::executor",
        nonce = %record.nonce.0,
        stage = ?record.stage,
        deadline_unix_millis,
        "expired order's stage has no timeout verdict in this build (issue #4738 owns the aggregate-review one; \
         any other stage here is a corrupt order, since nothing else is dispatched); \
         the order stays outstanding rather than being charged the wrong ledger",
    );
}

/// Whether a handle first seen at `first_seen` has aged past `threshold` as of
/// `now`. Pure so it is unit-testable without a clock, mirroring [`backoff_delay`].
fn is_stale(first_seen: Instant, now: Instant, threshold: Duration) -> bool {
    now.duration_since(first_seen) >= threshold
}

/// Project the config's `stale_warn_after_secs` knob into the threshold
/// [`select_stale_handles`] takes — `0` disables the sweep entirely.
fn stale_warn_after(stale_warn_after_secs: u64) -> Option<Duration> {
    (stale_warn_after_secs > 0).then(|| Duration::from_secs(stale_warn_after_secs))
}

/// Select the tracked handles that just crossed the staleness threshold: past
/// `threshold` age, not yet warned, paired with their last observed status from
/// the cycle's `pending` report (defaulting to [`ExecutionStatus::Unknown`] when
/// the cycle didn't report one — e.g. a handle tracked this same tick). Marks
/// each selected handle `stale_warned` so a still-wedged handle warns once per
/// crossing, not every tick. `threshold: None` (the `stale_warn_after_secs == 0`
/// disabled case) selects nothing. Factored out of the tick so the selection is
/// unit-testable with an injected `Instant`, mirroring [`next_backoff`].
fn select_stale_handles(
    tracked: &mut [TrackedHandle],
    pending: &[(Nonce, ExecutionStatus)],
    now: Instant,
    threshold: Option<Duration>,
) -> Vec<(Nonce, Duration, ExecutionStatus)> {
    let Some(threshold) = threshold else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    for tracked_handle in tracked.iter_mut() {
        if tracked_handle.stale_warned || !is_stale(tracked_handle.first_seen, now, threshold) {
            continue;
        }
        let status = pending
            .iter()
            .find(|(nonce, _)| *nonce == tracked_handle.handle.nonce)
            .map_or(ExecutionStatus::Unknown, |(_, status)| status.clone());
        warnings.push((tracked_handle.handle.nonce.clone(), now.duration_since(tracked_handle.first_seen), status));
        tracked_handle.stale_warned = true;
    }
    warnings
}

/// Runtime state for [`ExecutorReactorCapability`]. The shell + store are `Some`
/// only when configured; a disabled reactor holds neither and spawns no timer.
/// `tracked` accumulates the dispatched handles the pull side inspects each tick.
pub struct ExecutorReactorState {
    executor: Option<ExecutorShell>,
    store: Option<SqliteStore>,
    // Where an admitted attempt's study record is put (#4679); `None` when the
    // content store would not open, which disables the study lane without
    // disabling the reactor.
    artifacts: Option<ArtifactsCapabilityState>,
    claims: NameEvidenceClaims,
    tracked: Vec<TrackedHandle>,
    control_mailbox: MailboxId,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    // The poll timer sidecar; `None` when disabled. Held for its `Drop`, which
    // stops + joins the thread on teardown.
    _timer: Option<TimerHandle>,
    // Paces the transient re-drive (#3593); `None` when nothing is backing off.
    backoff: Option<BackoffCursor>,
    // How long a tracked handle may stay unresolved before the tick warns
    // (#3635); `None` when `stale_warn_after_secs == 0` disables the sweep.
    stale_warn_after: Option<Duration>,
    // The correspondence the push side resolves an admitted capture's commit
    // through (ADR-0152); `None` on a disabled reactor.
    correspondence: Option<SharedCorrespondence>,
    // The candidate-ref push seam (ADR-0152); production shells git.
    pusher: Arc<dyn CandidatePush>,
}

impl ExecutorReactorState {
    /// Build state over an explicit shell + store — the seam the runtime tests
    /// drive with a fake-GitHub-backed shell and an in-memory store, bypassing
    /// `init` (which needs config and a real connect). Spawns no timer; a test
    /// drives the loop by feeding a [`DispatchTick`] into the handler directly.
    ///
    /// This constructor exists only for tests, so it is by definition a fixture
    /// boot: its `pusher` resolves through `default_candidate_push`'s refusing
    /// arm rather than hand-picking a default here, so the boot-time selector
    /// stays the one policy site. (That selector is crate-private, so it is
    /// named rather than linked.) Chain [`Self::with_pusher`] to substitute a
    /// recording seam.
    #[must_use]
    pub fn with_parts(
        executor: Option<ExecutorShell>,
        store: Option<SqliteStore>,
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
    ) -> Self {
        Self {
            executor,
            store,
            artifacts: None,
            claims: NameEvidenceClaims,
            tracked: Vec::new(),
            control_mailbox: <ControlCore as Addressable>::resolve(0, ()),
            mailer,
            self_mailbox,
            _timer: None,
            backoff: None,
            stale_warn_after: stale_warn_after(CoordinatorConfig::default().stale_warn_after_secs),
            correspondence: None,
            pusher: default_candidate_push(true),
        }
    }

    /// Substitute the candidate-push seam — for a test harness that wants to
    /// record exactly which (commit, ref) pairs a push issued, rather than the
    /// fixture-default refusal [`Self::with_parts`] otherwise resolves.
    #[must_use]
    pub fn with_pusher(mut self, pusher: Arc<dyn CandidatePush>) -> Self {
        self.pusher = pusher;
        self
    }
}

/// Collect admitted attempt results so the handler can forward each to the control
/// actor after the intake cycle returns (the cycle is `&mut dyn AdmitSink`, and a
/// send needs the ctx the cycle does not hold).
#[derive(Default)]
struct CollectingSink(Vec<Admission>);

impl AdmitSink for CollectingSink {
    fn admit(&mut self, admission: Admission) {
        self.0.push(admission);
    }
}

/// Pin a member's workpiece id onto the shared work-order body.
///
/// Sibling members of one bloom are sealed from one template, so the store
/// holds byte-identical rows. The dispatched `--task` is what the lane reads
/// and what `pool_task` keys the session pool on, so an unpinned body collapses
/// every sibling onto one prompt and one pool key. The body stays the sealed
/// shared order; the header is the first line the lane can trust.
fn pin_workpiece_description(workpiece: &str, body: &str) -> String {
    format!("Workpiece: {workpiece}\n\n{body}")
}

/// Thread the host-resolved axes — the stage's agent profile and the member's
/// advisory work-order description (#3595) — onto a member `transformation`
/// about to dispatch. Only the model-driven `construct.implement` lane reads
/// them (Construct / Refine); the mechanical verify/review lanes never name a
/// task, so neither look one up nor warn on its absence. A store read fault
/// propagates via `?` (re-drained next tick); a missing row leaves the
/// description `None` and warns — a legible subject-only run, never a silent
/// blind dispatch.
///
/// A sealed configuration that cannot be resolved is a
/// [`StoreConfigError`] rather than a fall-through to the calibrated default
/// (ADR-0174): dispatching on the default while the receipt attests the sealed
/// override is exactly the divergence the registry closes, so the caller parks
/// the member instead. A member that sealed *no* override resolves to the
/// default, which is not an error and changes nothing.
///
/// Shared by the first dispatch of a stage and the replay of a parked one
/// (#3664), so a re-dispatched lane resolves its profile and prompt exactly the
/// way the attempt that parked did.
fn overlay_member_advisory(
    store: &mut dyn StoreBackend,
    record: &mut DispatchRecord,
    sequence: u64,
) -> Result<(), StoreConfigError> {
    if record.transformation.command != CONSTRUCT_IMPLEMENT_COMMAND {
        return Ok(());
    }
    let (bloom, workpiece) = (record.bloom.0.as_bytes().to_vec(), record.workpiece.0.clone());

    // The stage's calibrated agent profile (ADR-0149 §The line) as overridden by
    // the member's sealed scope revision, resolved host-side and overlaid the
    // same way the description below is: the reducer names both the stage catalog
    // and the revision by digest and resolves neither, so without this the lane
    // runs under whatever model the runner's ambient default happens to be and
    // the receipt attests a profile that never ran. `stage` is what separates
    // Construct from Refine — both dispatch the same command at different
    // calibrated efforts — and the override is what separates one member from
    // its siblings within a stage.
    let model_override =
        resolve_config::<ModelOverride>(store, ConfigScopes::bloom_wide(&record.configs))?.unwrap_or_default();
    record.transformation.model = Some(dispatch_model(record.stage, &record.profile, &model_override));
    if let Some(description) = store.lookup_dispatch_description(&bloom, &workpiece)? {
        record.transformation.description = Some(pin_workpiece_description(&workpiece, &description));
    } else {
        tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            sequence,
            workpiece = %workpiece,
            "no work-order description persisted for the dispatched construct member; assembling a subject-only prompt",
        );
    }

    // A failing verdict's persisted findings ride the same advisory channel as
    // their own labeled section (#3656, ADR-0153), so a Refine re-entry's prompt
    // names both the original order and what the failing gate flagged. The member
    // row wins; a failing aggregate review persists bloom-scoped findings under
    // the empty workpiece key until the decomposition slices them per member, and
    // every re-opened member reads that bloom row. The assembled prompt is plain
    // markdown concatenation, so the section header composes in-channel.
    let findings = match store.lookup_review_findings(&bloom, &workpiece)? {
        Some(findings) => Some(findings),
        None => store.lookup_review_findings(&bloom, "")?,
    };
    if let Some(findings) = findings {
        let task = record.transformation.description.take().unwrap_or_default();
        record.transformation.description = Some(format!("{task}\n\n## Findings\n\n{findings}"));
    }
    // A fold collision's overlay rides the same channel (ADR-0189): the
    // original description, then the contract, the conflicting paths, and
    // the conflicted candidate. Only Reconcile looks it up — Construct /
    // Refine have no collision to name.
    if record.stage == StageId::Reconcile
        && let Some(overlay) = store.lookup_fold_conflict(&bloom, &workpiece)?
    {
        let task = record.transformation.description.take().unwrap_or_default();
        record.transformation.description = Some(format!("{task}\n\n{overlay}"));
    }
    Ok(())
}

/// Drain the dispatch topic and submit each entry through the executor, recording
/// its intake context. Returns the newly-tracked handles and the highest
/// contiguously-submitted outbox sequence to ack (`None` when nothing submitted).
/// A decode or submit failure stops the ack prefix at the last success, so the
/// failed entry re-drains on the next tick rather than being acked past. The
/// factored-out network side, unit-testable against a `SqliteStore` + a
/// fake-GitHub-backed shell without the mail harness.
fn drain_and_dispatch(
    store: &mut dyn StoreBackend,
    executor: &ExecutorShell,
    now_unix_millis: u64,
) -> rusqlite::Result<(Vec<WorkHandle>, Option<u64>, Option<u64>)> {
    let entries = store.drain_topic(Topic::Dispatch)?;
    let mut handles = Vec::new();
    let mut ack_through = None;
    // The outbox sequence of a transient submit failure that stopped the drain —
    // `on_dispatch_tick` paces the next drain against it (#3593). `None` when the
    // drain reached the end of the batch clean, or stopped on a decode/subject
    // fault or a permanent-refusal park (neither re-drives on a timer).
    let mut transient_failure = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<DispatchPayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                "dispatch outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        // A retired plan's queued work is retired with it. The dispatch outbox is
        // durable and drains on its own timer, so a bloom superseded between seal
        // and drain leaves orders behind — and running one spends a full model
        // dispatch on a plan the operator explicitly replaced, then returns a
        // candidate for a bloom that can no longer admit it (#4640). A
        // supersession releases every one of the predecessor's memberships in the
        // decision set that marks it superseded, so holding none is the reducer's
        // own statement that this bloom is no longer live. Acked rather than
        // stopped: the entry is disposed of, not deferred, so the queue drains
        // instead of accumulating dead work — and this self-heals a queue
        // stranded by any path to the same state, not just supersession.
        if !store.holds_active_membership(payload.bloom.as_bytes())? {
            tracing::info!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                bloom = %short_hex(&payload.bloom),
                workpiece = %payload.workpiece.0,
                "dispatch belongs to a bloom that holds no active membership; retiring it undispatched",
            );
            ack_through = Some(entry.sequence);
            continue;
        }

        // The transformation pins the subject as its first input; a well-formed
        // per-member dispatch always carries one. The record's axes come from the
        // payload's explicit fields (ADR-0152), so the input is only checked for
        // well-formedness here — the executor backends re-derive it themselves.
        if payload.transformation.inputs.is_empty() {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                "dispatch transformation carries no subject input; stopping the ack prefix to re-drain",
            );
            break;
        }
        // The record's axes come from the payload's explicit fields (ADR-0152):
        // the true scope revision always, and the displayed digest — what the
        // returning evidence must bind to — the candidate tree when the member
        // has one, else the scope revision. `subject` (inputs[0]) agrees with
        // the displayed digest by reducer construction.
        let displayed = payload.candidate.unwrap_or(payload.scope_revision);
        let mut record = DispatchRecord {
            nonce: dispatch_nonce(entry.sequence),
            bloom: BloomId(payload.bloom),
            workpiece: payload.workpiece,
            scope_revision: payload.scope_revision,
            profile: payload.profile,
            candidate: displayed,
            displayed_digest: displayed,
            stage: payload.stage,
            transformation: payload.transformation,
            configs: payload.configs,
        };

        if let Err(error) = overlay_member_advisory(store, &mut record, entry.sequence) {
            if let StoreConfigError::Store(error) = error {
                return Err(error);
            }
            // A sealed configuration that will not resolve never resolves on
            // retry, so this parks like a permanent submit refusal: the entry
            // leaves the outbox, the queue behind it unblocks, and the member
            // stalls visibly rather than running under a configuration its
            // receipt does not attest.
            tracing::error!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                workpiece = %record.workpiece.0,
                %error,
                "sealed configuration did not resolve; parking the dispatch rather than running the default",
            );
            ack_through = Some(entry.sequence);
            continue;
        }
        match dispatch_and_record(executor, store, &record, now_unix_millis) {
            Ok(handle) => {
                handles.push(handle);
                ack_through = Some(entry.sequence);
            }
            Err(error) if error.is_permanent() => {
                // A permanent refusal never clears on retry, so parking (acking
                // past it) is what "wedge the member" means here: the entry
                // leaves the outbox, the queue behind it unblocks, and the error
                // log is the visible reason (member context + HTTP fault).
                tracing::error!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    bloom = ?record.bloom.0,
                    workpiece = %record.workpiece.0,
                    stage = ?record.stage,
                    nonce = %record.nonce.0,
                    %error,
                    "dispatch submit refused permanently; parking the entry instead of re-driving",
                );
                ack_through = Some(entry.sequence);
                break;
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    %error,
                    "dispatch submit/record failed; stopping the ack prefix to re-drive",
                );
                transient_failure = Some(entry.sequence);
                break;
            }
        }
    }
    Ok((handles, ack_through, transient_failure))
}

/// Drain the aggregate-review topic and submit each entry through the executor
/// under a bloom-level order record (ADR-0153): the `review.critic` lane run
/// against the integrated head, its task context composed from the whole
/// membership's persisted work orders — the sealed intent the critic judges
/// the integrated diff against. Same ack-prefix / park / backoff semantics as
/// [`drain_and_dispatch`]; the returned handles ride the same intake cycle,
/// and the intake routes the verdict by the record's `AggregateReview` stage.
/// Compose the aggregate-review task prompt (ADR-0153): the whole membership's
/// persisted work orders — the sealed intent the critic judges the integrated
/// diff against — plus the roll's framing. The first roll instructs the
/// attribution convention the findings decomposition parses back (each finding
/// block opens with the owning task id in square brackets); a later roll is
/// the delta-confirm, framed against the frozen findings row instead — judge
/// whether that set was resolved, never a fresh hunt. `None` when there is
/// nothing to compose (no orders, no frozen row) — the subject-only prompt.
fn compose_aggregate_task(
    store: &mut dyn StoreBackend,
    payload: &AggregateReviewPayload,
    sequence: u64,
) -> rusqlite::Result<Option<String>> {
    use core::fmt::Write;

    let orders = store.list_dispatch_descriptions(payload.bloom.as_bytes())?;
    if orders.is_empty() {
        tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            sequence,
            "no work-order descriptions persisted for the reviewed bloom; assembling a subject-only prompt",
        );
    }
    let frozen = if matches!(payload.pass, ReviewPass::DeltaConfirm) {
        let frozen = store.lookup_review_findings(payload.bloom.as_bytes(), "")?;
        if frozen.is_none() {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                sequence,
                pass = ?payload.pass,
                "delta-confirm dispatch found no frozen findings row; framing a full review",
            );
        }
        frozen
    } else {
        None
    };
    if orders.is_empty() && frozen.is_none() {
        return Ok(None);
    }
    let mut task = String::from(if frozen.is_some() {
        "Delta-confirm review: the first pass failed with the frozen findings below, and the implicated \
         members have repaired and re-integrated. Judge only whether the frozen findings are resolved in \
         this integrated tree — new findings do not extend the review. Every member's work order follows \
         for context."
    } else {
        "Review the whole integrated diff against the sealed intent: every member's work order follows."
    });
    for (workpiece, description) in &orders {
        let _ = write!(task, "\n\n## Task — {workpiece}\n\n{description}");
    }
    match &frozen {
        Some(findings) => {
            let _ = write!(task, "\n\n## Frozen findings\n\n{findings}");
        }
        None => {
            if let Some((example, _)) = orders.first() {
                let _ = write!(
                    task,
                    "\n\nAttribute each finding to the task that owns it: open the finding's first line \
                     with the owning task id in square brackets, e.g. `[{example}]`. Leave a finding that \
                     spans tasks untagged."
                );
            }
        }
    }
    Ok(Some(task))
}

fn drain_and_dispatch_aggregate(
    store: &mut dyn StoreBackend,
    executor: &ExecutorShell,
    now_unix_millis: u64,
) -> rusqlite::Result<(Vec<WorkHandle>, Option<u64>, Option<u64>)> {
    let entries = store.drain_topic(Topic::AggregateReview)?;
    let mut handles = Vec::new();
    let mut ack_through = None;
    let mut transient_failure = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<AggregateReviewPayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                "aggregate-review outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        if payload.transformation.inputs.is_empty() {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                "aggregate-review transformation carries no subject input; stopping the ack prefix to re-drain",
            );
            break;
        }
        // A retired plan's queued review is retired with it, on the same reading
        // as the member lane above: a critic run is a full model dispatch, and
        // one judging a superseded bloom's integrated diff is spent on a plan
        // that no longer exists.
        if !store.holds_active_membership(payload.bloom.as_bytes())? {
            tracing::info!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                bloom = %short_hex(&payload.bloom),
                "aggregate review belongs to a bloom that holds no active membership; retiring it undispatched",
            );
            ack_through = Some(entry.sequence);
            continue;
        }

        // A full-pass dispatch opens a fresh review cycle — the first ever, or an
        // owner re-arm after a park (ADR-0153). Clear any stale frozen row so
        // the new cycle's first failure freezes cleanly instead of appending
        // itself under the spent cycle's delta-confirm label.
        if matches!(payload.pass, ReviewPass::Full) {
            store.clear_review_findings(payload.bloom.as_bytes(), "")?;
        }
        let task = compose_aggregate_task(store, &payload, entry.sequence)?;
        let mut transformation = payload.transformation;
        // The aggregate critic is a model lane too, so it takes its calibrated
        // profile on the same overlay channel as the member lane above: the
        // bloom's sealed ModelOverride, resolved host-side so the receipt
        // attests the agent that actually ran (ADR-0174). A sealed address
        // that will not resolve parks rather than falling through to the
        // catalog default — the same divergence the member lane refuses.
        let model_override = match resolve_config::<ModelOverride>(store, ConfigScopes::bloom_wide(&payload.configs)) {
            Ok(override_) => override_.unwrap_or_default(),
            Err(StoreConfigError::Store(error)) => return Err(error),
            Err(error) => {
                tracing::error!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    bloom = %short_hex(&payload.bloom),
                    %error,
                    "sealed configuration did not resolve; parking the aggregate review rather than running the default",
                );
                ack_through = Some(entry.sequence);
                continue;
            }
        };
        transformation.model = Some(dispatch_model(StageId::AggregateReview, &payload.profile, &model_override));
        // The evidence-binding subject is the integrated tree the reducer
        // pinned as inputs[0] — also the displayed digest the returning
        // verdict must bind.
        let displayed = transformation.inputs[0];
        if let Some(task) = task {
            transformation.description = Some(task);
        }
        let record = DispatchRecord {
            nonce: dispatch_nonce(entry.sequence),
            bloom: BloomId(payload.bloom),
            // A bloom-level order has no member axis (ADR-0153): the stage
            // discriminates at intake, and the empty workpiece never routes.
            workpiece: WorkpieceId(String::new()),
            profile: payload.profile,
            scope_revision: displayed,
            candidate: displayed,
            displayed_digest: displayed,
            stage: StageId::AggregateReview,
            transformation,
            // The bloom-wide registry (ADR-0174): the critic has no member
            // axis, so this is the only scope the overlay walks.
            configs: payload.configs,
        };
        match dispatch_and_record(executor, store, &record, now_unix_millis) {
            Ok(handle) => {
                handles.push(handle);
                ack_through = Some(entry.sequence);
            }
            Err(error) if error.is_permanent() => {
                tracing::error!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    bloom = ?record.bloom.0,
                    pass = ?payload.pass,
                    nonce = %record.nonce.0,
                    %error,
                    "aggregate-review submit refused permanently; parking the entry instead of re-driving",
                );
                ack_through = Some(entry.sequence);
                break;
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    %error,
                    "aggregate-review submit/record failed; stopping the ack prefix to re-drive",
                );
                transient_failure = Some(entry.sequence);
                break;
            }
        }
    }
    Ok((handles, ack_through, transient_failure))
}

/// Drain the aggregate-verify topic and submit each entry through the executor
/// under a bloom-level order record — the mechanical `verify.check` fan-out run
/// over the folded head, before the critic sees it.
///
/// Same ack-prefix / park / backoff semantics as [`drain_and_dispatch_aggregate`],
/// and the intake routes the verdict by the record's `AggregateVerify` stage.
/// Shorter than the review's drain because a compiler needs no prompt: no task
/// composition, no pass framing, no findings row to clear or freeze — the lane
/// gets the fold and runs.
fn drain_and_dispatch_aggregate_verify(
    store: &mut dyn StoreBackend,
    executor: &ExecutorShell,
    now_unix_millis: u64,
) -> rusqlite::Result<(Vec<WorkHandle>, Option<u64>, Option<u64>)> {
    let entries = store.drain_topic(Topic::AggregateVerify)?;
    let mut handles = Vec::new();
    let mut ack_through = None;
    let mut transient_failure = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<AggregateVerifyPayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                "aggregate-verify outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        if payload.transformation.inputs.is_empty() {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                "aggregate-verify transformation carries no subject input; stopping the ack prefix to re-drain",
            );
            break;
        }
        // A retired plan's queued verify is retired with it, like the member and
        // review lanes: the fold it would build belongs to a plan that no longer
        // exists.
        if !store.holds_active_membership(payload.bloom.as_bytes())? {
            tracing::info!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                bloom = %short_hex(&payload.bloom),
                "aggregate verify belongs to a bloom that holds no active membership; retiring it undispatched",
            );
            ack_through = Some(entry.sequence);
            continue;
        }

        // The evidence-binding subject is the folded tree the reducer pinned as
        // inputs[0] — also the displayed digest the returning verdict must bind.
        let displayed = payload.transformation.inputs[0];
        let record = DispatchRecord {
            nonce: dispatch_nonce(entry.sequence),
            bloom: BloomId(payload.bloom),
            // A bloom-level order has no member axis: the stage discriminates at
            // intake, and the empty workpiece never routes.
            workpiece: WorkpieceId(String::new()),
            profile: payload.profile,
            scope_revision: displayed,
            candidate: displayed,
            displayed_digest: displayed,
            stage: StageId::AggregateVerify,
            // No model overlay: this is a mechanical lane, so it carries no
            // resolved model for the same reason the member `Verify` does not.
            transformation: payload.transformation,
            configs: ConfigRegistry::default(),
        };
        match dispatch_and_record(executor, store, &record, now_unix_millis) {
            Ok(handle) => {
                handles.push(handle);
                ack_through = Some(entry.sequence);
            }
            Err(error) if error.is_permanent() => {
                tracing::error!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    bloom = ?record.bloom.0,
                    nonce = %record.nonce.0,
                    %error,
                    "aggregate-verify submit refused permanently; parking the entry instead of re-driving",
                );
                ack_through = Some(entry.sequence);
                break;
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    %error,
                    "aggregate-verify submit/record failed; stopping the ack prefix to re-drive",
                );
                transient_failure = Some(entry.sequence);
                break;
            }
        }
    }
    Ok((handles, ack_through, transient_failure))
}

/// The advisory section a re-dispatched lane reads the owner's decision from.
/// Composes in-channel beside `## Findings`, the same plain-markdown way.
const DECISION_SECTION: &str = "## Decision";

/// Assemble the replay a released question's held order dispatches: the held
/// lane under a fresh nonce, its advisory channel re-resolved, and the answer
/// that released the hold appended as its own section.
///
/// `Ok(None)` means the entry names nothing replayable — no held row, or a row
/// whose columns do not decode. Neither clears on retry, so the caller acks past
/// it; a store fault is the `Err` that re-drains.
fn resolve_replay(
    store: &mut dyn StoreBackend,
    payload: &RedispatchPayload,
    sequence: u64,
) -> rusqlite::Result<Option<DispatchRecord>> {
    let Some(held) = store.lookup_parked_question(payload.bloom.as_bytes(), payload.question.as_bytes())? else {
        // Either the park predates this reactor, or the answer named a hold no
        // dispatched attempt raised.
        tracing::error!(
            target: "aether_chassis_bloomery::executor",
            sequence,
            bloom = ?payload.bloom,
            question = ?payload.question,
            "no parked order held under the released question; acking past the redispatch",
        );
        return Ok(None);
    };
    let Some(mut record) = DispatchRecord::from_stored(&held) else {
        tracing::error!(
            target: "aether_chassis_bloomery::executor",
            sequence,
            nonce = %held.nonce,
            "held order did not decode into a dispatch record; acking past the redispatch",
        );
        return Ok(None);
    };

    // A fresh nonce off the outbox sequence, so the replay is its own attempt
    // rather than a re-run of the spent one the park consumed.
    record.nonce = Nonce(format!("redispatch-{sequence}"));
    if let Err(error) = overlay_member_advisory(store, &mut record, sequence) {
        if let StoreConfigError::Store(error) = error {
            return Err(error);
        }
        tracing::error!(
            target: "aether_chassis_bloomery::executor",
            sequence,
            workpiece = %record.workpiece.0,
            %error,
            "sealed configuration did not resolve on replay; acking past the redispatch",
        );
        return Ok(None);
    }

    match str::from_utf8(&payload.words) {
        Ok(answer) if record.transformation.command == CONSTRUCT_IMPLEMENT_COMMAND => {
            let task = record.transformation.description.take().unwrap_or_default();
            record.transformation.description = Some(format!("{task}\n\n{DECISION_SECTION}\n\n{answer}"));
        }
        Ok(_) => tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            sequence,
            command = %record.transformation.command,
            "re-dispatching a lane with no advisory channel; it cannot see the answer and will park again",
        ),
        Err(error) => tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            sequence,
            %error,
            "answer statement is not UTF-8; re-dispatching without the decision overlay",
        ),
    }
    Ok(Some(record))
}

/// Drain the redispatch topic and replay each released question's held attempt
/// (ADR-0151, #3664). An adopted answer releases the hold and decides a
/// re-dispatch; this is the half that performs it — it looks the parked order up
/// under `(bloom, question)`, overlays the answer onto the lane's advisory
/// channel, and submits it under a fresh nonce.
///
/// The overlay is load-bearing, not decoration: a lane replayed without the
/// decision that released it sees exactly the inputs that made it park and parks
/// again on the same question, so the bloom would wedge in a loop rather than
/// silently. A lane with no advisory channel (the mechanical zero-egress
/// `verify.check`) cannot be told anything, and warns rather than replaying a
/// question it has no way to answer.
///
/// Same ack-prefix / park / backoff semantics as [`drain_and_dispatch`]: a
/// decode, lookup, or submit failure stops the ack prefix so the entry re-drains
/// rather than being acked past. The held row is consumed only *after* the
/// replay dispatches, so a transient failure re-drains against a row still there
/// — the ordering the integrate correspondence learned the hard way (#3667).
fn drain_and_redispatch(
    store: &mut dyn StoreBackend,
    executor: &ExecutorShell,
    now_unix_millis: u64,
) -> rusqlite::Result<(Vec<WorkHandle>, Option<u64>, Option<u64>)> {
    let entries = store.drain_topic(Topic::Redispatch)?;
    let mut handles = Vec::new();
    let mut ack_through = None;
    let mut transient_failure = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<RedispatchPayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                "redispatch outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        let Some(record) = resolve_replay(store, &payload, entry.sequence)? else {
            // The entry names nothing replayable and never will, so ack past it
            // rather than wedge the queue behind it.
            ack_through = Some(entry.sequence);
            continue;
        };

        match dispatch_and_record(executor, store, &record, now_unix_millis) {
            Ok(handle) => {
                handles.push(handle);
                ack_through = Some(entry.sequence);
                // The replay is submitted and tracked, so the hold's row has done
                // its job. A delete fault here leaves an orphan row nothing reads
                // (the outbox entry it answered is acked), never a lost redispatch.
                if let Err(error) = store.consume_parked_question(payload.bloom.as_bytes(), payload.question.as_bytes())
                {
                    tracing::warn!(
                        target: "aether_chassis_bloomery::executor",
                        sequence = entry.sequence,
                        %error,
                        "redispatched attempt submitted but its parked row did not clear",
                    );
                }
            }
            Err(error) if error.is_permanent() => {
                tracing::error!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    bloom = ?record.bloom.0,
                    workpiece = %record.workpiece.0,
                    stage = ?record.stage,
                    nonce = %record.nonce.0,
                    %error,
                    "redispatch submit refused permanently; parking the entry instead of re-driving",
                );
                ack_through = Some(entry.sequence);
                break;
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    %error,
                    "redispatch submit/record failed; stopping the ack prefix to re-drive",
                );
                transient_failure = Some(entry.sequence);
                break;
            }
        }
    }
    Ok((handles, ack_through, transient_failure))
}

/// The candidate-ref push seam (ADR-0152): make an admitted capture commit
/// reachable on the hosted repo, so a zero-secret Actions runner can check it
/// out and the API-side integrate can resolve its tree. Production shells the
/// operator's own authenticated clone; tests substitute a recorder.
pub trait CandidatePush: Send + Sync {
    /// Force-push `commit_hex` to `target_ref` on `origin`.
    ///
    /// # Errors
    /// The push shell-out failed; the message is the diagnostic tail.
    fn push(&self, commit_hex: &str, target_ref: &str) -> Result<(), String>;
}

/// The production push: `git push --force origin <sha>:<ref>` from the host
/// process's own clone. The capture commit was created in a scratch worktree of
/// this same repository, so its object is in the shared object store and
/// pushable after the worktree is gone; the host's ambient credentials are the
/// ADR-0150 trust domain (the worker itself never pushes).
///
/// # Warning to anyone extending the in-process fixture tier
///
/// Which pusher a reactor carries is chosen at boot by
/// [`default_candidate_push`] (#4835), and that choice keys on the *backend
/// configuration* rather than on build shape. Two routes therefore differ:
/// [`ExecutorReactorState::with_parts`] — the fixture-shaped constructor these
/// scenarios use — resolves the refusing arm, while a reactor that comes up
/// through `init` without `AETHER_GITHUB_BACKEND=fixture` still carries *this*
/// pusher even under `cargo test` (#4842).
///
/// That distinction is what a scenario on the second route has to respect,
/// because nothing downstream holds it back. [`on_dispatch_tick`] calls
/// [`pull_and_admit`] unconditionally, dozens of times per scenario, so the
/// tick loop is not a barrier. What stops such a scenario today is one link
/// further down: `FakeGithub::dispatch_workflow` records a dispatch and never a
/// run, so `find_run` answers `None`, the intake cycle matches nothing, and the
/// push loop iterates an empty slice.
///
/// That containment is one `seed_run` away from gone. A scenario that seeds a
/// completed run with artifacts — the natural next step when extending this
/// harness toward the real pull path — makes an admitted capture reach here,
/// and the correspondence the fixture already seeds resolves its checkout to a
/// real commit, against whatever `origin` the developer's checkout points at.
/// Before writing such a scenario, put a recording seam in through
/// [`ExecutorReactorState::with_pusher`]; do not rely on the boot-time selector
/// having picked the refusing arm for you.
///
/// [`on_dispatch_tick`]: ExecutorReactorCapability
struct GitCandidatePush;

impl CandidatePush for GitCandidatePush {
    fn push(&self, commit_hex: &str, target_ref: &str) -> Result<(), String> {
        // A source sha that is all-zero — or empty — is not a sha git resolves;
        // both are its ref-delete sentinels (#4841). `git push --force origin
        // 0000…:<ref>` and `git push --force origin :<ref>` each exit 0 and
        // report `- [deleted]`, so this seam's success test (`status.success()`)
        // reads a destroyed candidate ref as a published one. Every *other*
        // unresolvable sha exits 1 with `bad object`, which is why only these
        // two values need naming.
        //
        // `GitObjectId` refuses the null oid at construction, so a correspondence
        // record cannot deliver one here. This guard covers the gap that leaves:
        // the trait takes `&str`, so nothing in the type system stops a future
        // caller that formats a sha some other way.
        if commit_hex.bytes().all(|byte| byte == b'0') {
            return Err(format!(
                "refusing to push `{commit_hex}` to {target_ref}: git reads an all-zero or empty source sha as a ref deletion, not a commit",
            ));
        }

        let refspec = format!("{commit_hex}:{target_ref}");
        let output = Command::new("git")
            .args(["push", "--force", "origin"])
            .arg(&refspec)
            .output()
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }
}

/// The fixture-boot push: declines every push rather than no-opping. A no-op
/// hides a harness that reached the push path by accident behind a quiet
/// success; a refusal is legible in a log — the message names exactly what it
/// declined — so a fixture boot that wanders into this seam fails loudly
/// instead of pretending to have pushed.
struct RefusingCandidatePush;

impl CandidatePush for RefusingCandidatePush {
    fn push(&self, commit_hex: &str, target_ref: &str) -> Result<(), String> {
        Err(format!("refusing to push fixture commit {commit_hex} to {target_ref}: no real push in a fixture boot"))
    }
}

/// Select the [`CandidatePush`] seam for boot: a boot that must not touch a real
/// `origin` refuses every push, and any other boot shells `git push`.
///
/// `refuse` is the caller's answer to "could this process's `origin` be a live
/// repository?", and the boot site answers it from **build shape first**
/// (`cfg!(any(test, feature = "testing"))`) and configuration second
/// (`uses_fixture`). Configuration alone was not enough: `cargo test` forks a
/// `testing`-featured binary that names no backend, so it resolved to the real
/// pusher with its cwd inside the real checkout (#4842).
///
/// Crate-only on purpose. `CandidatePush` has to stay public — it types the
/// `pub pusher` field — but the selector handing out a live `GitCandidatePush`
/// does not, and leaving it public put one within reach of every out-of-crate
/// integration-test binary, which is exactly the reach this seam exists to deny.
///
/// Declared `pub` because every module between here and `bloomery` is private,
/// so this is already unreachable from outside; the `pub(crate)` that actually
/// restricts it sits on the re-export in `bloomery`, the one public module in
/// the chain. Writing `pub(crate)` at each hop instead would be the redundancy
/// `clippy::redundant_pub_crate` flags, and it would state the restriction in
/// four places while only one of them enforces it.
#[must_use]
pub fn default_candidate_push(refuse: bool) -> Arc<dyn CandidatePush> {
    if refuse {
        Arc::new(RefusingCandidatePush)
    } else {
        Arc::new(GitCandidatePush)
    }
}

/// Push each admitted passing capture to its bloom candidate ref (ADR-0152).
/// Best-effort with loud warns: a failed push leaves the candidate local-only —
/// a downstream Actions checkout of it will fail visibly and retry through the
/// stage machinery, never silently run the wrong tree. A failing completion's
/// capture is not pushed (the reducer discards it).
fn push_admitted_candidates(
    admissions: &[Admission],
    correspondence: Option<&SharedCorrespondence>,
    pusher: &dyn CandidatePush,
) {
    for admission in admissions {
        let Fact::AttemptCompleted { bloom, workpiece, passed: true, candidate: Some(candidate), .. } =
            &admission.event.fact
        else {
            continue;
        };
        let Some(correspondence) = correspondence else {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                workpiece = %workpiece.0,
                "admitted capture has no correspondence store to resolve its commit; candidate stays local-only",
            );
            continue;
        };
        let commit = match correspondence.resolve_backend_object(&candidate.checkout) {
            Ok(Some(commit)) => match GitObjectId::try_from(commit) {
                Ok(commit) => commit,
                Err(error) => {
                    tracing::warn!(target: "aether_chassis_bloomery::executor", workpiece = %workpiece.0, %error, "capture correspondence is not a valid git object id");
                    continue;
                }
            },
            Ok(None) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    workpiece = %workpiece.0,
                    "admitted capture's checkout digest has no recorded git object; candidate stays local-only",
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(target: "aether_chassis_bloomery::executor", workpiece = %workpiece.0, %error, "capture correspondence read failed");
                continue;
            }
        };
        let target_ref = candidate_ref_name(bloom, &workpiece.0);
        match pusher.push(&commit.to_hex(), &target_ref) {
            Ok(()) => tracing::info!(
                target: "aether_chassis_bloomery::executor",
                workpiece = %workpiece.0,
                target_ref = %target_ref,
                "candidate capture pushed",
            ),
            Err(error) => tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                workpiece = %workpiece.0,
                target_ref = %target_ref,
                %error,
                "candidate push failed; candidate stays local-only",
            ),
        }
    }
}

/// Whether the reactor has no backend to mount for this pair of configs —
/// GitHub unconfigured *and* the local lane disabled (#4626). Unconfigured alone
/// is not enough: the local backend needs no credential, so it still dispatches
/// every lane routed to it.
///
/// A `#[cfg(test)]` **mirror** of the expression `actor_setups` mounts by, so
/// the mount decision is assertable without building a `NativeInitCtx`. Being a
/// copy rather than the production predicate is its standing weakness: it can
/// drift, and it had. `actor_setups` already reads a selected fixture (#4732) as
/// a configured backend — the in-memory double answers every dispatch and
/// artifact call even though it names no token, owner, or repo — while this copy
/// still consulted the missing connection knobs alone. The two therefore
/// disagreed on exactly the configuration the in-process scenarios boot (#4711):
/// a fixture with the local lane off.
///
/// Only the mirror was wrong. No binary mounts on this expression, so nothing
/// was ever silently disabled in a shipping coordinator; what the repair fixes
/// is a test that would have vouched for the wrong answer, and
/// `a_selected_fixture_mounts_even_with_the_local_lane_off` pins the copy back
/// against `actor_setups`.
#[cfg(test)]
fn is_disabled_mount(connection: &GithubConnectionConfig, coordinator: &CoordinatorConfig) -> bool {
    !connection.uses_fixture() && !connection.missing_connection_knobs().is_empty() && !coordinator.local_lane_enabled
}

/// The restart recovery set (issue #3641): one [`WorkHandle`] per nonce still
/// outstanding in the store, so a dispatched-but-unresolved order (its outbox
/// entry acked/delivered, but not yet admitted when the process stopped) is
/// tracked again after a restart instead of being permanently stranded. Only
/// the nonce is read — the admit path already re-reads the full
/// [`crate::store::OutstandingOrder`] via `lookup_order` at match time, so
/// decoding the other columns here would be dead work. Factored out of `init`
/// so a test can exercise it without constructing a `NativeInitCtx`.
fn seed_tracked(store: &mut dyn StoreBackend) -> rusqlite::Result<Vec<WorkHandle>> {
    Ok(store.list_outstanding_nonces()?.into_iter().map(|nonce| WorkHandle::new(Nonce(nonce))).collect())
}

/// The same outstanding orders as [`seed_tracked`], carrying the transformation
/// each one dispatched — what the executor port needs to reconcile its own
/// in-flight state at boot (issue #4847).
///
/// Re-tracking a handle is only half of restart recovery: the port that handle
/// resolves against has an empty routing map and an empty local run registry, so
/// the re-tracked order resolves to the wrong arm and finds no run there. The
/// transformation is the missing input — the local arm derives a re-adopted run's
/// evidence-binding subject and lane gates from it.
///
/// A separate pass over the same table rather than one that produces both,
/// deliberately: an order whose persisted `transformation` will not decode must
/// still be **tracked**, or a single unreadable blob would strand an order the way
/// #3641 stranded all of them. So a shortfall here drops that order out of the
/// reconciliation with a warn and leaves the recovery set whole.
fn seed_dispatches(store: &mut dyn StoreBackend) -> rusqlite::Result<Vec<OutstandingDispatch>> {
    let mut dispatches = Vec::new();
    for nonce in store.list_outstanding_nonces()? {
        let Some(transformation) = store.lookup_order(&nonce)?.and_then(|order| from_bytes(&order.transformation).ok())
        else {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                %nonce,
                "outstanding order carries no decodable transformation; it is still tracked but excluded from lane reconciliation",
            );
            continue;
        };
        dispatches.push(OutstandingDispatch { nonce: Nonce(nonce), transformation });
    }
    Ok(dispatches)
}

/// This reactor's own handle on the artifacts content store, where an admitted
/// attempt's study record is put (#4679).
///
/// Its own, opened at the configured root — the same shape as the `SqliteStore`
/// above, which this reactor also opens rather than sharing. Both are durable
/// stores addressed by path, and the artifacts store is content-addressed, so a
/// second writer stores identical bytes under an identical digest; there is no
/// state to reconcile between handles.
///
/// A store that will not open yields `None` and disables the study lane for this
/// process, logged once at boot. Deliberately not a `BootError`: the ledger is a
/// grading surface, and a coordinator that cannot record costs must still run
/// blooms.
fn open_artifacts(configured: Option<&str>) -> Option<ArtifactsCapabilityState> {
    let root = resolve_root(configured);
    match ArtifactsCapabilityState::open(&root) {
        Ok(artifacts) => Some(artifacts),
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                root = %root.display(),
                %error,
                "artifacts store did not open; attempt costs will not be recorded this session",
            );
            None
        }
    }
}

/// The two durable stores one pull cycle writes through: the journal registry
/// that holds the outstanding-order table, and the content store the study lane
/// puts an attempt's cost record into. Bundled because they are one concept —
/// where this cycle's writes land — and travel together to every caller.
struct Stores<'a> {
    store: &'a mut dyn StoreBackend,
    artifacts: Option<&'a mut ArtifactsCapabilityState>,
}

/// The clock readings one tick works from, taken once so its dispatches,
/// expiries, and staleness warns all agree about when the tick was.
struct TickClock {
    /// Unix milliseconds — what a recorded order's deadline is computed from and
    /// what every persisted deadline is tested against (ADR-0177).
    now_unix_millis: u64,
    /// How long a tracked handle may stay unresolved before the advisory warn
    /// (#3635); `None` when the sweep is disabled. Advisory only: it warns, and
    /// the deadline beside it is what terminates.
    stale_warn_after: Option<Duration>,
}

/// Pull matched attempt results for the tracked handles and return the [`Admit`]s
/// to forward to the control core, pruning the handles whose order the broker
/// consumed (a completed + admitted run).
///
/// Three passes, in an order ADR-0177 fixes. Completion first, so evidence that
/// arrived at the deadline boundary is admitted normally rather than losing to a
/// clock that has just crossed. Then the deadline sweep, which terminates every
/// order still pending past its persisted deadline — and which runs only when
/// the completion pass actually completed, because a faulted cycle has not
/// looked at every handle and its "still pending" is unearned. Then the advisory
/// staleness sweep (#3635), which is left exactly as it was: a handle past
/// `stale_warn_after` warns once, naming its nonce, age, and last observed
/// status — it reports, and the deadline is what acts.
///
/// The factored-out network side, unit-testable like [`drain_and_dispatch`].
fn pull_and_admit(
    stores: Stores<'_>,
    executor: &ExecutorShell,
    claims: NameEvidenceClaims,
    tracked: &mut Vec<TrackedHandle>,
    clock: &TickClock,
    correspondence: Option<&SharedCorrespondence>,
    pusher: &dyn CandidatePush,
) -> Vec<Admit> {
    let Stores { store, mut artifacts } = stores;
    let mut sink = CollectingSink::default();
    let handles: Vec<WorkHandle> = tracked.iter().map(|tracked_handle| tracked_handle.handle.clone()).collect();
    let cycle = run_intake_cycle(store, executor, &handles, &claims, artifacts.as_deref_mut(), &mut sink);
    let completion_was_observed = cycle.is_ok();
    let report = cycle.unwrap_or_else(|error| {
        tracing::warn!(target: "aether_chassis_bloomery::executor", %error, "intake cycle failed; results re-drive next tick");
        CycleReport::default()
    });

    // Drop the handles whose order was consumed on admit — a still-outstanding
    // order (order lookup Some) stays tracked to poll again; a store fault leaves
    // it tracked to retry rather than silently dropping it. Prune before the
    // staleness sweep below, so a handle that just resolved and was consumed this
    // same cycle never spuriously selects as stale (#3635 review finding): its
    // nonce carries no `pending` entry once consumed, so a pre-prune sweep would
    // read that absence as "no status observed" rather than "just resolved".
    tracked.retain(|tracked_handle| match store.lookup_order(&tracked_handle.handle.nonce.0) {
        Ok(order) => order.is_some(),
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                nonce = %tracked_handle.handle.nonce.0,
                %error,
                "order lookup failed; keeping the handle tracked to retry",
            );
            true
        }
    });

    // Only now that completion has been observed and consumed: terminate what is
    // still pending past its sealed deadline.
    //
    // And only if it *was* observed. `run_intake_cycle` abandons its loop on the
    // first handle whose inspect or evidence stream faults, so a failed cycle
    // leaves the handles behind it uninspected — "still pending" then means
    // "never asked", not "not finished". Sweeping on that reading cancels a lane
    // that completed well inside its budget and admits a synthesised failure over
    // real passing evidence, spending a retry to hide a transport blip. A
    // deferred sweep costs one poll interval and the transport re-drives; a wrong
    // sweep destroys the attempt.
    let timed_out = if completion_was_observed {
        expire_overdue_orders(Stores { store, artifacts }, executor, tracked, clock.now_unix_millis)
    } else {
        Vec::new()
    };

    for (nonce, age, status) in select_stale_handles(tracked, &report.pending, Instant::now(), clock.stale_warn_after) {
        tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            nonce = %nonce.0,
            age_secs = age.as_secs(),
            last_status = ?status,
            "dispatched run has not resolved past the staleness threshold",
        );
    }

    // Make each admitted passing capture reachable on the hosted repo before the
    // fact reaches the reducer — the very next tick can dispatch the follow-on
    // stage to a zero-secret Actions runner that must fetch it (ADR-0152).
    push_admitted_candidates(&sink.0, correspondence, pusher);

    sink.0.into_iter().map(|admission| admission.admit).chain(timed_out).collect()
}

/// Admit one scripted lane verdict against this reactor's own stores, returning
/// the scenario's reply and the [`Admit`] to forward (#4711).
///
/// The factored-out intake side of [`on_scripted_evidence`], mirroring
/// [`pull_and_admit`]: everything that touches the two durable stores lives
/// here, and the ctx send stays in the handler.
///
/// The store-side steps are the production ones. The study lane writes through
/// *this reactor's* boot-opened artifacts handle — so a reactor that opened the
/// wrong root files costs where nothing reads them, and the scenario that reads
/// them back fails (#4705) — and the verdict itself goes through
/// [`admit_uploaded`], which refuses a nonce naming no live order and a subject
/// the order did not display. A scenario therefore cannot script an attempt the
/// coordinator never ordered. The one deliberate departure inside them is the
/// study lane's fault policy, which fails the call instead of swallowing —
/// reasoned at the call site.
///
/// # What is substituted besides the verdict
///
/// [`pull_and_admit`] ends by calling [`push_admitted_candidates`], which
/// resolves an admitted capture's `checkout` through the correspondence store
/// and publishes that commit at [`candidate_ref_name`]. This function does
/// neither. A scenario's reactor comes up through
/// [`ExecutorReactorState::with_parts`], whose pusher refuses every push
/// (#4835), so routing the fold through the real publish would buy a refusal
/// rather than a push — and the arm that would *not* refuse shells a real
/// `git push --force origin`, which a scenario must never run. The fixture
/// harness plants the candidate ref itself, through the same
/// `candidate_ref_name` helper.
///
/// So the ADR-0152 candidate push is substituted as surely as the verdict is,
/// and that is a coverage hole rather than a detail: a wrong ref name, a dropped
/// push, or a mis-resolved correspondence is invisible to every scenario built
/// on this seam, because the fold reads a ref the harness wrote. Proving that
/// step needs the lane-boundary tier, which runs a real pusher.
///
/// [`on_scripted_evidence`]: ExecutorReactorCapability
#[cfg(any(test, feature = "testing"))]
fn admit_scripted(state: &mut ExecutorReactorState, encoded: &[u8]) -> (ScriptedEvidenceResult, Option<Admit>) {
    let failed = |error: String| (ScriptedEvidenceResult::Err { error }, None);
    let Some(store) = state.store.as_mut() else {
        return failed("the executor reactor mounted disabled".to_owned());
    };
    let upload = match from_bytes::<ScriptedUpload>(encoded) {
        Ok(upload) => upload.into_upload(),
        Err(error) => return failed(format!("scripted upload did not decode: {error}")),
    };

    // Study before the verdict, for the reason the intake cycle documents:
    // `admit_study` matches the order without consuming it while the admit below
    // consumes, so the other order records nothing.
    //
    // The *fault* policy deliberately diverges from that cycle's, which swallows
    // a refusal or a store fault (warn, admit the verdict anyway) because the
    // study lane grades attempts and must never gate them: on a live
    // coordinator, trading a missing ledger row for a stalled bloom is never
    // worth it. Here there is no bloom to stall and nobody reading warns. A
    // swallowed study fault would instead surface several steps later as a
    // missing index row — the same symptom as the wrong-artifacts-root defect
    // (#4705) this tier exists to catch, with the real cause only in a log the
    // test harness discards. So a scripted study that does not record stops the
    // call and names which of the three it was.
    match (upload.cost, state.artifacts.as_mut()) {
        (Some(cost), Some(artifacts)) => {
            let record = UploadedStudyRecord {
                nonce: upload.nonce.clone(),
                subject: upload.subject,
                cost,
                calls: upload.calls.clone(),
            };
            match admit_study(store, artifacts, &record) {
                Ok(StudyAdmitDecision::Admitted(_)) => {}
                Ok(StudyAdmitDecision::Refused(refusal)) => {
                    return failed(format!("scripted study record refused: {refusal:?}"));
                }
                Err(error) => return failed(format!("scripted study record failed: {error}")),
            }
        }
        // The third way a scripted study fails to record: `open_artifacts`
        // warned and handed back no handle, so a scripted cost has nowhere to be
        // filed. Passing over that in silence produces exactly the delayed,
        // mislabelled symptom the policy above rejects — the scenario still
        // fails, but several steps later at a missing index row.
        (Some(_), None) => {
            return failed("scripted study record dropped: the reactor holds no artifacts handle".to_owned());
        }
        (None, _) => {}
    }

    match admit_uploaded(store, &upload) {
        Ok(AdmitDecision::Admitted(admission)) => (
            ScriptedEvidenceResult::Admitted { idempotency_key: admission.event.idempotency_key.0.clone() },
            Some(admission.admit.clone()),
        ),
        Ok(AdmitDecision::Refused(refusal)) => {
            (ScriptedEvidenceResult::Refused { refusal: format!("{refusal:?}") }, None)
        }
        Err(error) => failed(error.to_string()),
    }
}

#[runtime]
impl NativeActor for ExecutorReactorCapability {
    type State = ExecutorReactorState;
    type Config = ();
    type Params = ExecutorReactorSetup;

    const NAMESPACE: &'static str = "aether.bloomery.executor";

    fn init(
        (): (),
        config: ExecutorReactorSetup,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<ExecutorReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let control_mailbox = <ControlCore as Addressable>::resolve(0, ());

        // Mount whenever either backend is usable: fully configured (GitHub +
        // local) → RoutingExecutor, unconfigured + local enabled → local-only
        // (local lanes still dispatch; Actions lanes fail fast with the missing-
        // knob reason), neither usable → disabled with no shell/store/timer.
        let Some(executor) = config.executor else {
            // A `warn`, not an `info` (#4625): declining to mount is the one
            // condition that makes every later seal look healthy and never
            // dispatch, so it must not sit below the boot chatter. Naming the
            // empty knobs turns diagnosis from a code-read into a read of this
            // line — `token` in particular resolves from the unprefixed
            // `GITHUB_TOKEN`, so `AETHER_GITHUB_TOKEN` is the obvious guess and
            // silently does nothing.
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                missing = %config.disabled_missing.join(", "),
                "executor dispatch reactor mounted disabled (unconfigured); dispatch outbox will accumulate and no sealed bloom will dispatch",
            );
            return Ok(ExecutorReactorState {
                executor: None,
                store: None,
                claims: NameEvidenceClaims,
                tracked: Vec::new(),
                control_mailbox,
                mailer,
                self_mailbox,
                artifacts: None,
                _timer: None,
                backoff: None,
                stale_warn_after: stale_warn_after(config.stale_warn_after_secs),
                correspondence: None,
                pusher: config.pusher,
            });
        };

        let mut store = SqliteStore::open(&config.store_path).map_err(|e| BootError::Other(Box::new(e)))?;
        // Restart recovery (#3641): a reactor that cannot read its recovery set
        // must not silently start with an empty one — that is the bug this
        // seed exists to fix, so a read error fails boot rather than mounting
        // with a stranded `tracked` vec.
        let seeded_at = Instant::now();
        let tracked: Vec<TrackedHandle> = seed_tracked(&mut store)
            .map_err(|e| BootError::Other(Box::new(e)))?
            .into_iter()
            .map(|handle| TrackedHandle::new(handle, seeded_at))
            .collect();
        // The other half of restart recovery (issue #4847): hand the port the same
        // outstanding orders so it re-adopts the local runs a previous process
        // dispatched and reclaims the scratch checkouts of orders that are no longer
        // outstanding. A read fault fails boot for the same reason `seed_tracked`'s
        // does — mounting with a silently empty recovery set is the bug, not the fix.
        let reconciled = executor.reconcile(&seed_dispatches(&mut store).map_err(|e| BootError::Other(Box::new(e)))?);
        // The third leg (issue #4956): both legs above are scoped to the orders
        // the store still holds, so a dispatch whose order was spent without its
        // fact reaching the journal leaves them nothing to find and the member
        // permanently parked. Re-queue those for the ordinary drain. A read
        // fault fails boot for the same reason the two above do.
        let restranded = readopt_stranded_dispatches(&mut store).map_err(|e| BootError::Other(Box::new(e)))?;
        let interval = Duration::from_secs(config.poll_interval_secs.max(1));
        let timer = spawn_timer(
            Arc::clone(&mailer),
            self_mailbox,
            DispatchTick::ID,
            DispatchTick::default().encode_into_bytes(),
            "aether-bloomery-executor-dispatch",
            interval,
        );
        tracing::info!(
            target: "aether_chassis_bloomery::executor",
            repository = ?config.repository,
            poll_interval_secs = config.poll_interval_secs,
            retracked = tracked.len(),
            readopted = reconciled.readopted.len(),
            reclaimed = reconciled.reclaimed,
            requeued = restranded.len(),
            "executor dispatch reactor mounted; polling the store for dispatch decisions",
        );
        // The push side resolves an admitted capture's commit through its own
        // correspondence handle on the shared store (ADR-0152). Fixture (#4732)
        // uses the in-memory FakeGithub correspondence so the candidate push
        // resolves without a SQLite store.
        let correspondence = config.correspondence;
        Ok(ExecutorReactorState {
            executor: Some(executor),
            store: Some(store),
            artifacts: open_artifacts(config.artifacts_root.as_deref()),
            claims: NameEvidenceClaims,
            tracked,
            control_mailbox,
            mailer,
            self_mailbox,
            _timer: Some(timer),
            backoff: None,
            stale_warn_after: stale_warn_after(config.stale_warn_after_secs),
            correspondence,
            pusher: config.pusher,
        })
    }

    /// Fire an immediate boot tick so a dispatch left undrained by a prior crash
    /// submits without waiting a full poll interval. Disabled reactors push nothing.
    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        if state.executor.is_some() {
            state.mailer.push(Mail::new(
                state.self_mailbox,
                DispatchTick::ID,
                DispatchTick::default().encode_into_bytes(),
                1,
            ));
        }
    }

    /// Poll wake: drain + submit the dispatch topic, then pull + admit matched
    /// results. The GitHub calls run inline on the dispatcher (the poll cadence
    /// spaces them); a detached-worker split is a follow-up if latency demands it.
    #[handler::single]
    fn on_dispatch_tick(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: DispatchTick) {
        let Some(executor) = state.executor.clone() else {
            return;
        };
        let claims = state.claims;
        let control_mailbox = state.control_mailbox;
        let Some(store) = state.store.as_mut() else {
            return;
        };

        // One clock reading for the whole tick: every order this tick records
        // takes its deadline from it, and every persisted deadline is tested
        // against it, so a dispatch and an expiry in the same tick cannot
        // disagree about when the tick was.
        let clock = TickClock { now_unix_millis: now_unix_millis(), stale_warn_after: state.stale_warn_after };

        // Skip the drain while inside a transient-failure backoff window (#3593) —
        // paces the re-drive instead of hammering GitHub at the flat poll cadence.
        let skip_drain = state.backoff.as_ref().is_some_and(|cursor| cursor.retry_after > Instant::now());
        if !skip_drain {
            // Drain + submit the newly-decided dispatches, acking the submitted prefix.
            match drain_and_dispatch(store, &executor, clock.now_unix_millis) {
                Ok((handles, ack_through, transient_failure)) => {
                    if let Some(sequence) = ack_through
                        && let Err(error) = store.ack_topic(Topic::Dispatch, sequence)
                    {
                        tracing::warn!(target: "aether_chassis_bloomery::executor", %error, "dispatch ack failed; entries re-drive");
                    }
                    let now = Instant::now();
                    state.tracked.extend(handles.into_iter().map(|handle| TrackedHandle::new(handle, now)));
                    state.backoff = next_backoff(state.backoff.as_ref(), transient_failure);
                }
                Err(error) => {
                    tracing::warn!(target: "aether_chassis_bloomery::executor", %error, "dispatch drain failed");
                }
            }
            // Drain + submit the whole-bloom aggregate reviews (ADR-0153) the
            // same way — its handles ride the same intake cycle, and a
            // transient failure joins the shared backoff window.
            match drain_and_dispatch_aggregate(store, &executor, clock.now_unix_millis) {
                Ok((handles, ack_through, transient_failure)) => {
                    if let Some(sequence) = ack_through
                        && let Err(error) = store.ack_topic(Topic::AggregateReview, sequence)
                    {
                        tracing::warn!(target: "aether_chassis_bloomery::executor", %error, "aggregate-review ack failed; entries re-drive");
                    }
                    let now = Instant::now();
                    state.tracked.extend(handles.into_iter().map(|handle| TrackedHandle::new(handle, now)));
                    if transient_failure.is_some() {
                        state.backoff = next_backoff(state.backoff.as_ref(), transient_failure);
                    }
                }
                Err(error) => {
                    tracing::warn!(target: "aether_chassis_bloomery::executor", %error, "aggregate-review drain failed");
                }
            }
            // Drain + submit the whole-bloom aggregate verifies the same way —
            // the mechanical gate the fold passes before its critic dispatches.
            match drain_and_dispatch_aggregate_verify(store, &executor, clock.now_unix_millis) {
                Ok((handles, ack_through, transient_failure)) => {
                    if let Some(sequence) = ack_through
                        && let Err(error) = store.ack_topic(Topic::AggregateVerify, sequence)
                    {
                        tracing::warn!(target: "aether_chassis_bloomery::executor", %error, "aggregate-verify ack failed; entries re-drive");
                    }
                    let now = Instant::now();
                    state.tracked.extend(handles.into_iter().map(|handle| TrackedHandle::new(handle, now)));
                    if transient_failure.is_some() {
                        state.backoff = next_backoff(state.backoff.as_ref(), transient_failure);
                    }
                }
                Err(error) => {
                    tracing::warn!(target: "aether_chassis_bloomery::executor", %error, "aggregate-verify drain failed");
                }
            }
            // Replay the attempts whose parked questions were answered (#3664),
            // on the same shared handle tracking and backoff window.
            match drain_and_redispatch(store, &executor, clock.now_unix_millis) {
                Ok((handles, ack_through, transient_failure)) => {
                    if let Some(sequence) = ack_through
                        && let Err(error) = store.ack_topic(Topic::Redispatch, sequence)
                    {
                        tracing::warn!(target: "aether_chassis_bloomery::executor", %error, "redispatch ack failed; entries re-drive");
                    }
                    let now = Instant::now();
                    state.tracked.extend(handles.into_iter().map(|handle| TrackedHandle::new(handle, now)));
                    if transient_failure.is_some() {
                        state.backoff = next_backoff(state.backoff.as_ref(), transient_failure);
                    }
                }
                Err(error) => {
                    tracing::warn!(target: "aether_chassis_bloomery::executor", %error, "redispatch drain failed");
                }
            }
        }

        // Pull matched results and forward each admitted attempt to the control core.
        let correspondence = state.correspondence.clone();
        let pusher = Arc::clone(&state.pusher);
        for admit in pull_and_admit(
            Stores { store, artifacts: state.artifacts.as_mut() },
            &executor,
            claims,
            &mut state.tracked,
            &clock,
            correspondence.as_ref(),
            pusher.as_ref(),
        ) {
            // Fire-and-forget: the control actor's on_admit is reliable local mail,
            // and the reducer's idempotency key dedups a resend, so the settlement
            // handle is not needed here.
            let _ = ctx.send_envelope_detached(control_mailbox, Admit::ID, &admit.encode_into_bytes());
        }
    }

    /// Admit one scripted lane verdict for an order this reactor really
    /// dispatched (#4711) — the in-process fixture tier's stand-in for the model
    /// a lane would have run.
    ///
    /// The store-side steps are the production ones: the study lane writes
    /// through *this reactor's* boot-opened artifacts handle (so a reactor that
    /// opened the wrong root fails the scenario rather than filing costs where
    /// nothing reads them, #4705), and the verdict itself goes through
    /// [`admit_uploaded`] — which refuses a nonce naming no live order and a
    /// subject the order did not display. A scenario therefore cannot script an
    /// attempt the coordinator never ordered.
    ///
    /// The verdict is not the only substitution, and the other one is easy to
    /// forget: [`admit_scripted`] omits [`push_admitted_candidates`], the tail
    /// [`pull_and_admit`] runs, so the ADR-0152 candidate push does not happen
    /// on this path and the fixture plants the candidate ref itself. See
    /// [`admit_scripted`] for what that leaves uncovered.
    ///
    /// The `Admit` rides **tracked** rather than detached, unlike the pull
    /// side's: a scenario's call must settle only once the control core has
    /// reduced and committed the fact, which is what lets a scenario step the
    /// bloom instead of sleeping on it.
    #[cfg(any(test, feature = "testing"))]
    #[handler::single]
    fn on_scripted_evidence(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: ScriptedEvidence,
    ) -> ScriptedEvidenceResult {
        // Destructured rather than borrowed through: `#[handler::single]` hands
        // the mail over by value, so taking the payload out of it consumes the
        // envelope instead of leaving it to be dropped unread.
        let ScriptedEvidence { upload } = mail;
        let control_mailbox = state.control_mailbox;
        let (result, admit) = admit_scripted(state, &upload);

        if let Some(admit) = admit {
            let _ = ctx.send_envelope_tracked(control_mailbox, Admit::ID, &admit.encode_into_bytes());
        }
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
