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

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_actor::runtime;
use aether_bloomery::{
    Admit, AggregateReviewPayload, AggregateVerifyPayload, BloomId, ConfigRegistry, ConfigScopes, DispatchPayload,
    ExecutionStatus, Fact, ModelOverride, Nonce, RedispatchPayload, ReviewPass, StageId, Topic, WorkHandle,
    WorkpieceId,
};
use aether_bloomery_github::{SharedCorrespondence, candidate_ref_name, short_hex};
use aether_data::wire::from_bytes;
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::ExecutorReactorCapability;
use crate::artifacts::{ArtifactsCapabilityState, resolve_root};
use crate::bloomery::CONSTRUCT_IMPLEMENT_COMMAND;
use crate::bloomery::ExecutorShell;
use crate::bloomery::dispatch_model;
use crate::bloomery::intake::{
    Admission, AdmitSink, CycleReport, DispatchRecord, NameEvidenceClaims, dispatch_and_record, run_intake_cycle,
};
use crate::bloomery::mirror::GithubMirrorConfig;
use crate::bloomery::outbox::TopicOutbox;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::control::ControlCore;
use crate::store::{SqliteStore, StoreBackend, StoreConfigError, resolve_config};

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
}

impl TrackedHandle {
    fn new(handle: WorkHandle, first_seen: Instant) -> Self {
        Self { handle, first_seen, stale_warned: false }
    }
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
            stale_warn_after: stale_warn_after(GithubMirrorConfig::default().stale_warn_after_secs),
            correspondence: None,
            pusher: Arc::new(GitCandidatePush),
        }
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
        record.transformation.description = Some(description);
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
            nonce: Nonce(format!("dispatch-{}", entry.sequence)),
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
        match dispatch_and_record(executor, store, &record) {
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
        // profile on the same overlay channel as the member lane above.
        transformation.model =
            Some(dispatch_model(StageId::AggregateReview, &payload.profile, &ModelOverride::default()));
        // The evidence-binding subject is the integrated tree the reducer
        // pinned as inputs[0] — also the displayed digest the returning
        // verdict must bind.
        let displayed = transformation.inputs[0];
        if let Some(task) = task {
            transformation.description = Some(task);
        }
        let record = DispatchRecord {
            nonce: Nonce(format!("dispatch-{}", entry.sequence)),
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
            // A bloom-level lane resolves no member configuration: the overlay
            // reads a registry only for `construct.implement`, and this is the
            // review critic's command.
            configs: ConfigRegistry::default(),
        };
        match dispatch_and_record(executor, store, &record) {
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
            nonce: Nonce(format!("dispatch-{}", entry.sequence)),
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
        match dispatch_and_record(executor, store, &record) {
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

        match dispatch_and_record(executor, store, &record) {
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
struct GitCandidatePush;

impl CandidatePush for GitCandidatePush {
    fn push(&self, commit_hex: &str, target_ref: &str) -> Result<(), String> {
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
        let commit = match correspondence.resolve_git(&candidate.checkout) {
            Ok(Some(commit)) => commit,
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

/// Pull matched attempt results for the tracked handles and return the [`Admit`]s
/// to forward to the control core, pruning the handles whose order the broker
/// consumed (a completed + admitted run). Also runs the staleness sweep
/// (#3635): a handle still tracked past `stale_warn_after` warns once, naming
/// its nonce, age, and last observed status — `stale_warn_after: None` disables
/// it. The factored-out network side, unit-testable like [`drain_and_dispatch`].
fn pull_and_admit(
    stores: Stores<'_>,
    executor: &ExecutorShell,
    claims: NameEvidenceClaims,
    tracked: &mut Vec<TrackedHandle>,
    stale_warn_after: Option<Duration>,
    correspondence: Option<&SharedCorrespondence>,
    pusher: &dyn CandidatePush,
) -> Vec<Admit> {
    let Stores { store, artifacts } = stores;
    let mut sink = CollectingSink::default();
    let handles: Vec<WorkHandle> = tracked.iter().map(|tracked_handle| tracked_handle.handle.clone()).collect();
    let report = match run_intake_cycle(store, executor, &handles, &claims, artifacts, &mut sink) {
        Ok(report) => report,
        Err(error) => {
            tracing::warn!(target: "aether_chassis_bloomery::executor", %error, "intake cycle failed; results re-drive next tick");
            CycleReport::default()
        }
    };

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

    for (nonce, age, status) in select_stale_handles(tracked, &report.pending, Instant::now(), stale_warn_after) {
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

    sink.0.into_iter().map(|admission| admission.admit).collect()
}

/// Whether the reactor has no backend to mount for `config` — GitHub
/// unconfigured *and* the local lane disabled (#4626). Unconfigured alone is not
/// enough: the local backend needs no credential, so it still dispatches every
/// lane routed to it. Pure so the mount decision is testable without a
/// `NativeInitCtx`.
fn is_disabled_mount(config: &GithubMirrorConfig) -> bool {
    !config.missing_connection_knobs().is_empty() && !config.local_lane_enabled
}

fn connect_executor_correspondence(config: &GithubMirrorConfig) -> Result<SharedCorrespondence, BootError> {
    #[cfg(any(test, feature = "testing"))]
    if config.uses_fixture() {
        let fake = config.shared_fixture();
        return Ok(Arc::new(fake) as SharedCorrespondence);
    }
    config.connect_correspondence().map_err(|e| BootError::Other(Box::new(e)))
}

#[runtime]
impl NativeActor for ExecutorReactorCapability {
    type State = ExecutorReactorState;
    type Config = GithubMirrorConfig;

    const NAMESPACE: &'static str = "aether.bloomery.executor";

    fn init(config: GithubMirrorConfig, ctx: &mut NativeInitCtx<'_>) -> Result<ExecutorReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let control_mailbox = <ControlCore as Addressable>::resolve(0, ());

        // Mount whenever either backend is usable: fully configured (GitHub +
        // local) → RoutingExecutor, unconfigured + local enabled → local-only
        // (local lanes still dispatch; Actions lanes fail fast with the missing-
        // knob reason), neither usable → disabled with no shell/store/timer.
        if is_disabled_mount(&config) {
            // A `warn`, not an `info` (#4625): declining to mount is the one
            // condition that makes every later seal look healthy and never
            // dispatch, so it must not sit below the boot chatter. Naming the
            // empty knobs turns diagnosis from a code-read into a read of this
            // line — `token` in particular resolves from the unprefixed
            // `GITHUB_TOKEN`, so `AETHER_GITHUB_TOKEN` is the obvious guess and
            // silently does nothing.
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                missing = %config.missing_connection_knobs().join(", "),
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
                pusher: Arc::new(GitCandidatePush),
            });
        }

        let executor = ExecutorShell::connect(&config).map_err(|e| BootError::Other(Box::new(e)))?;
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
            owner = %config.owner,
            repo = %config.repo,
            poll_interval_secs = config.poll_interval_secs,
            retracked = tracked.len(),
            "executor dispatch reactor mounted; polling the store for dispatch decisions",
        );
        // The push side resolves an admitted capture's commit through its own
        // correspondence handle on the shared store (ADR-0152). Fixture (#4732)
        // uses the in-memory FakeGithub correspondence so the candidate push
        // resolves without a SQLite store.
        let correspondence = connect_executor_correspondence(&config)?;
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
            correspondence: Some(correspondence),
            pusher: Arc::new(GitCandidatePush),
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

        // Skip the drain while inside a transient-failure backoff window (#3593) —
        // paces the re-drive instead of hammering GitHub at the flat poll cadence.
        let skip_drain = state.backoff.as_ref().is_some_and(|cursor| cursor.retry_after > Instant::now());
        if !skip_drain {
            // Drain + submit the newly-decided dispatches, acking the submitted prefix.
            match drain_and_dispatch(store, &executor) {
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
            match drain_and_dispatch_aggregate(store, &executor) {
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
            match drain_and_dispatch_aggregate_verify(store, &executor) {
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
            match drain_and_redispatch(store, &executor) {
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
        let stale_warn_after = state.stale_warn_after;
        let correspondence = state.correspondence.clone();
        let pusher = Arc::clone(&state.pusher);
        for admit in pull_and_admit(
            Stores { store, artifacts: state.artifacts.as_mut() },
            &executor,
            claims,
            &mut state.tracked,
            stale_warn_after,
            correspondence.as_ref(),
            pusher.as_ref(),
        ) {
            // Fire-and-forget: the control actor's on_admit is reliable local mail,
            // and the reducer's idempotency key dedups a resend, so the settlement
            // handle is not needed here.
            let _ = ctx.send_envelope_detached(control_mailbox, Admit::ID, &admit.encode_into_bytes());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
