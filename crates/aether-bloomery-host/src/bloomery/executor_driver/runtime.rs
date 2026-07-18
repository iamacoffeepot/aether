//! The runtime for the executor dispatch-driver capability (ADR-0149 migration
//! step 2 — issue #3505).
//!
//! A poll-driven loop that turns the reducer's dispatch decisions into running
//! work and matched results back into admitted facts:
//!
//! 1. **Drain + submit.** Each tick drains the store's `aether.bloomery.dispatch`
//!    outbox topic (its own connection, so the intake registry the pull side
//!    reads/consumes is one store), decodes each
//!    [`DispatchPayload`](aether_bloomery::DispatchPayload), submits it through the
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
//! Config-gated exactly like the mirror driver: unconfigured (empty
//! token/owner/repo) mounts disabled — no shell, no store, no timer — so a
//! zero-secret dev boot neither errors nor spins; the outbox accumulates until a
//! token is supplied.
//!
//! Store ownership: this driver opens its **own** [`SqliteStore`] on the shared
//! `AETHER_STORE_PATH` because the intake helpers ([`dispatch_and_record`] /
//! [`run_intake_cycle`]) drive the registry in-process over a `StoreBackend`, not
//! over mail. It is the sole writer of the `outstanding_orders` table and acks
//! only its own dispatch topic; the `busy_timeout` on every connection serializes
//! the rare concurrent WAL write with the `StoreCapability`'s. Routing the outbox
//! drain/ack through the store's `DrainOutbox`/`AckOutbox` mail (as the mirror
//! driver does) is a follow-up once the registry ops gain a mail surface.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_actor::runtime;
use aether_bloomery::{Admit, BloomId, DispatchPayload, ExecutionStatus, Nonce, WorkHandle, WorkOrder};
use aether_data::wire::from_bytes;
use aether_data::{Kind, MailboxId, mailbox_id_from_path};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::ExecutorDriverCapability;
use crate::bloomery::CONSTRUCT_IMPLEMENT_COMMAND;
use crate::bloomery::ExecutorShell;
use crate::bloomery::intake::{
    Admission, AdmitSink, CycleReport, DispatchRecord, NameEvidenceClaims, dispatch_and_record, run_intake_cycle,
};
use crate::bloomery::mirror::GithubMirrorConfig;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::store::{SqliteStore, StoreBackend};

/// The outbox topic the reducer enqueues a per-member attempt dispatch under —
/// the mirror of the control actor's `DISPATCH_TOPIC` producer constant. This
/// capability is its sole consumer.
pub const DISPATCH_TOPIC: &str = "aether.bloomery.dispatch";

/// The autoloaded control-core component's lineage mailbox — where an admitted
/// attempt result is sent. Resolved from the lineage path (`mailbox_id_from_path`),
/// mirroring the REST API's `control_mailbox()`; the control actor is not a native
/// singleton, so the sibling-cap typed send does not apply.
const CONTROL_CORE_PATH: &str = "aether.component/aether.embedded:aether.bloomery.control";

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

/// Runtime state for [`ExecutorDriverCapability`]. The shell + store are `Some`
/// only when configured; a disabled driver holds neither and spawns no timer.
/// `tracked` accumulates the dispatched handles the pull side inspects each tick.
pub struct ExecutorDriverState {
    executor: Option<ExecutorShell>,
    store: Option<SqliteStore>,
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
}

impl ExecutorDriverState {
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
            claims: NameEvidenceClaims,
            tracked: Vec::new(),
            control_mailbox: mailbox_id_from_path(CONTROL_CORE_PATH),
            mailer,
            self_mailbox,
            _timer: None,
            backoff: None,
            stale_warn_after: stale_warn_after(GithubMirrorConfig::default().stale_warn_after_secs),
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
    let entries = store.drain_outbox(Some(DISPATCH_TOPIC))?;
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
                target: "aether_bloomery_host::executor",
                sequence = entry.sequence,
                "dispatch outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        // The transformation pins the subject as its first input; a well-formed
        // per-member dispatch always carries one. The subject is the digest the
        // returning evidence must bind to (candidate == displayed == subject).
        let Some(subject) = payload.transformation.inputs.first().copied() else {
            tracing::warn!(
                target: "aether_bloomery_host::executor",
                sequence = entry.sequence,
                "dispatch transformation carries no subject input; stopping the ack prefix to re-drain",
            );
            break;
        };
        // Thread the member's advisory work-order description onto the construct
        // lane (#3595). Only the model-driven `construct.implement` lane reads it
        // (Construct / Refine); the mechanical verify/review lanes never name a
        // task, so neither look it up nor warn on its absence. A store read fault
        // propagates via `?` (re-drained next tick); a missing row leaves the
        // description `None` and warns — a legible subject-only run, never a
        // silent blind dispatch.
        let mut transformation = payload.transformation;
        if transformation.command == CONSTRUCT_IMPLEMENT_COMMAND {
            if let Some(description) =
                store.lookup_dispatch_description(payload.bloom.as_bytes(), &payload.workpiece.0)?
            {
                transformation.description = Some(description);
            } else {
                tracing::warn!(
                    target: "aether_bloomery_host::executor",
                    sequence = entry.sequence,
                    workpiece = %payload.workpiece.0,
                    "no work-order description persisted for the dispatched construct member; assembling a subject-only prompt",
                );
            }
        }
        let nonce = Nonce(format!("dispatch-{}", entry.sequence));
        let order = WorkOrder { transformation, nonce: nonce.clone() };
        let record = DispatchRecord {
            nonce,
            bloom: BloomId(payload.bloom),
            workpiece: payload.workpiece,
            scope_revision: subject,
            candidate: subject,
            displayed_digest: subject,
            stage: payload.stage,
        };
        match dispatch_and_record(executor, store, &order, &record) {
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
                    target: "aether_bloomery_host::executor",
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
                    target: "aether_bloomery_host::executor",
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

/// Pull matched attempt results for the tracked handles and return the [`Admit`]s
/// to forward to the control core, pruning the handles whose order the broker
/// consumed (a completed + admitted run). Also runs the staleness sweep
/// (#3635): a handle still tracked past `stale_warn_after` warns once, naming
/// its nonce, age, and last observed status — `stale_warn_after: None` disables
/// it. The factored-out network side, unit-testable like [`drain_and_dispatch`].
fn pull_and_admit(
    store: &mut dyn StoreBackend,
    executor: &ExecutorShell,
    claims: NameEvidenceClaims,
    tracked: &mut Vec<TrackedHandle>,
    stale_warn_after: Option<Duration>,
) -> Vec<Admit> {
    let mut sink = CollectingSink::default();
    let handles: Vec<WorkHandle> = tracked.iter().map(|tracked_handle| tracked_handle.handle.clone()).collect();
    let report = match run_intake_cycle(store, executor, &handles, &claims, &mut sink) {
        Ok(report) => report,
        Err(error) => {
            tracing::warn!(target: "aether_bloomery_host::executor", %error, "intake cycle failed; results re-drive next tick");
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
                target: "aether_bloomery_host::executor",
                nonce = %tracked_handle.handle.nonce.0,
                %error,
                "order lookup failed; keeping the handle tracked to retry",
            );
            true
        }
    });

    for (nonce, age, status) in select_stale_handles(tracked, &report.pending, Instant::now(), stale_warn_after) {
        tracing::warn!(
            target: "aether_bloomery_host::executor",
            nonce = %nonce.0,
            age_secs = age.as_secs(),
            last_status = ?status,
            "dispatched run has not resolved past the staleness threshold",
        );
    }

    sink.0.into_iter().map(|admission| admission.admit).collect()
}

#[runtime]
impl NativeActor for ExecutorDriverCapability {
    type State = ExecutorDriverState;
    type Config = GithubMirrorConfig;

    const NAMESPACE: &'static str = "aether.bloomery.executor";

    fn init(config: GithubMirrorConfig, ctx: &mut NativeInitCtx<'_>) -> Result<ExecutorDriverState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let control_mailbox = mailbox_id_from_path(CONTROL_CORE_PATH);

        // Unconfigured → disabled: no shell, no store, no timer. The dispatch
        // outbox accumulates and drains once a token/owner/repo is supplied.
        let configured = !(config.token.is_empty() || config.owner.is_empty() || config.repo.is_empty());
        if !configured {
            tracing::info!(
                target: "aether_bloomery_host::executor",
                "executor dispatch driver mounted disabled (unconfigured token/owner/repo); dispatch outbox will accumulate",
            );
            return Ok(ExecutorDriverState {
                executor: None,
                store: None,
                claims: NameEvidenceClaims,
                tracked: Vec::new(),
                control_mailbox,
                mailer,
                self_mailbox,
                _timer: None,
                backoff: None,
                stale_warn_after: stale_warn_after(config.stale_warn_after_secs),
            });
        }

        let executor = ExecutorShell::connect(&config).map_err(|e| BootError::Other(Box::new(e)))?;
        let mut store = SqliteStore::open(&config.store_path).map_err(|e| BootError::Other(Box::new(e)))?;
        // Restart recovery (#3641): a driver that cannot read its recovery set
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
            target: "aether_bloomery_host::executor",
            owner = %config.owner,
            repo = %config.repo,
            poll_interval_secs = config.poll_interval_secs,
            retracked = tracked.len(),
            "executor dispatch driver mounted; polling the store for dispatch decisions",
        );
        Ok(ExecutorDriverState {
            executor: Some(executor),
            store: Some(store),
            claims: NameEvidenceClaims,
            tracked,
            control_mailbox,
            mailer,
            self_mailbox,
            _timer: Some(timer),
            backoff: None,
            stale_warn_after: stale_warn_after(config.stale_warn_after_secs),
        })
    }

    /// Fire an immediate boot tick so a dispatch left undrained by a prior crash
    /// submits without waiting a full poll interval. Disabled drivers push nothing.
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
                        && let Err(error) = store.ack_outbox(Some(DISPATCH_TOPIC), sequence)
                    {
                        tracing::warn!(target: "aether_bloomery_host::executor", %error, "dispatch ack failed; entries re-drive");
                    }
                    let now = Instant::now();
                    state.tracked.extend(handles.into_iter().map(|handle| TrackedHandle::new(handle, now)));
                    state.backoff = next_backoff(state.backoff.as_ref(), transient_failure);
                }
                Err(error) => {
                    tracing::warn!(target: "aether_bloomery_host::executor", %error, "dispatch drain failed");
                }
            }
        }

        // Pull matched results and forward each admitted attempt to the control core.
        let stale_warn_after = state.stale_warn_after;
        for admit in pull_and_admit(store, &executor, claims, &mut state.tracked, stale_warn_after) {
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
