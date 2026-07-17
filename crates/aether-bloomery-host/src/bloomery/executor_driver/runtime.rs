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
use std::time::Duration;

use aether_actor::runtime;
use aether_bloomery::{Admit, BloomId, DispatchPayload, Nonce, WorkHandle, WorkOrder};
use aether_data::wire::from_bytes;
use aether_data::{Kind, MailboxId, mailbox_id_from_path};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::ExecutorDriverCapability;
use crate::bloomery::ExecutorShell;
use crate::bloomery::intake::{
    Admission, AdmitSink, DispatchRecord, NameEvidenceClaims, dispatch_and_record, run_intake_cycle,
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

/// Runtime state for [`ExecutorDriverCapability`]. The shell + store are `Some`
/// only when configured; a disabled driver holds neither and spawns no timer.
/// `tracked` accumulates the dispatched handles the pull side inspects each tick.
pub struct ExecutorDriverState {
    executor: Option<ExecutorShell>,
    store: Option<SqliteStore>,
    claims: NameEvidenceClaims,
    tracked: Vec<WorkHandle>,
    control_mailbox: MailboxId,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    // The poll timer sidecar; `None` when disabled. Held for its `Drop`, which
    // stops + joins the thread on teardown.
    _timer: Option<TimerHandle>,
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
) -> rusqlite::Result<(Vec<WorkHandle>, Option<u64>)> {
    let entries = store.drain_outbox(Some(DISPATCH_TOPIC))?;
    let mut handles = Vec::new();
    let mut ack_through = None;
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
        let nonce = Nonce(format!("dispatch-{}", entry.sequence));
        let order = WorkOrder { transformation: payload.transformation, nonce: nonce.clone() };
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
            Err(error) => {
                tracing::warn!(
                    target: "aether_bloomery_host::executor",
                    sequence = entry.sequence,
                    %error,
                    "dispatch submit/record failed; stopping the ack prefix to re-drive",
                );
                break;
            }
        }
    }
    Ok((handles, ack_through))
}

/// Pull matched attempt results for the tracked handles and return the [`Admit`]s
/// to forward to the control core, pruning the handles whose order the broker
/// consumed (a completed + admitted run). The factored-out network side, unit-
/// testable like [`drain_and_dispatch`].
fn pull_and_admit(
    store: &mut dyn StoreBackend,
    executor: &ExecutorShell,
    claims: NameEvidenceClaims,
    tracked: &mut Vec<WorkHandle>,
) -> Vec<Admit> {
    let mut sink = CollectingSink::default();
    if let Err(error) = run_intake_cycle(store, executor, tracked, &claims, &mut sink) {
        tracing::warn!(target: "aether_bloomery_host::executor", %error, "intake cycle failed; results re-drive next tick");
    }
    // Drop the handles whose order was consumed on admit — a still-outstanding
    // order (order lookup Some) stays tracked to poll again; a store fault leaves
    // it tracked to retry rather than silently dropping it.
    tracked.retain(|handle| match store.lookup_order(&handle.nonce.0) {
        Ok(order) => order.is_some(),
        Err(error) => {
            tracing::warn!(
                target: "aether_bloomery_host::executor",
                nonce = %handle.nonce.0,
                %error,
                "order lookup failed; keeping the handle tracked to retry",
            );
            true
        }
    });
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
            });
        }

        let executor = ExecutorShell::connect(&config).map_err(|e| BootError::Other(Box::new(e)))?;
        let store = SqliteStore::open(&config.store_path).map_err(|e| BootError::Other(Box::new(e)))?;
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
            "executor dispatch driver mounted; polling the store for dispatch decisions",
        );
        Ok(ExecutorDriverState {
            executor: Some(executor),
            store: Some(store),
            claims: NameEvidenceClaims,
            tracked: Vec::new(),
            control_mailbox,
            mailer,
            self_mailbox,
            _timer: Some(timer),
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

        // Drain + submit the newly-decided dispatches, acking the submitted prefix.
        match drain_and_dispatch(store, &executor) {
            Ok((handles, ack_through)) => {
                if let Some(sequence) = ack_through
                    && let Err(error) = store.ack_outbox(Some(DISPATCH_TOPIC), sequence)
                {
                    tracing::warn!(target: "aether_bloomery_host::executor", %error, "dispatch ack failed; entries re-drive");
                }
                state.tracked.extend(handles);
            }
            Err(error) => {
                tracing::warn!(target: "aether_bloomery_host::executor", %error, "dispatch drain failed");
            }
        }

        // Pull matched results and forward each admitted attempt to the control core.
        for admit in pull_and_admit(store, &executor, claims, &mut state.tracked) {
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
