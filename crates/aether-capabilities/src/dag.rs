//! `aether.dag` cap (ADR-0047 §4, iamacoffeepot/aether#976). Owns the
//! substrate-side [`Executor`](aether_substrate::dag::executor::Executor)
//! and routes the three request kinds (`aether.dag.{submit,cancel,
//! status}`) plus the executor's two internal wake kinds — `Settled`
//! (the per-`Call` settlement notice) and `DagReapTick` (the reaping
//! timer) — into it.
//!
//! Same construction shape as `HandleCapability` and
//! `RpcServerCapability`: a singleton
//! [`NativeActor`](aether_substrate::NativeActor) registered at
//! chassis boot under the `aether.dag` namespace (ADR-0078
//! chassis-internal actor). `init` caches the `Arc<Mailer>` + own
//! mailbox id off the init ctx and builds the executor against them,
//! then spawns a detached timer thread that fires `DagReapTick` every
//! ~30s so the reaping sweep runs on this actor's own dispatcher
//! thread (the executor's `DagState` map is lock-free actor state).
//!
//! Source / `Call` replies arrive as arbitrary kinds correlated by the
//! dispatch's correlation id; the `#[fallback]` forwards them to the
//! executor's
//! [`Executor::on_reply`](aether_substrate::dag::executor::Executor::on_reply).
//! The hub chassis doesn't register
//! this cap (ADR-0035 / ADR-0047 §8) — a `aether.dag.submit` to a hub
//! substrate warn-drops as an unknown mailbox, the right shape for
//! "DAG submission is not supported here".

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::{Cancel, DagReapTick, DagTransformDone, Status, Submit, trace::Settled};

#[aether_actor::bridge(singleton)]
mod native {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::{Cancel, DagReapTick, DagTransformDone, Settled, Status, Submit};
    use aether_actor::{MailCtx, actor};
    use aether_data::{Kind, KindId, MailId, MailboxId};
    use aether_kinds::{StatusResult, SubmitResult};
    use aether_substrate::Mail;
    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::dag::executor::{Executor, ExecutorConfig, SubmitOutcome};
    use aether_substrate::mail::mailer::Mailer;

    /// Reaping cadence (ADR-0047 §7) — the timer thread fires a
    /// `DagReapTick` at this interval so the executor sweeps terminal
    /// DAGs + times out never-settling `Call`s.
    const REAP_INTERVAL: Duration = Duration::from_secs(30);

    /// `aether.dag` cap. Owns the per-substrate [`Executor`].
    pub struct DagCapability {
        executor: Executor,
        /// Cached so `unwire` can stop the reaping timer thread.
        reap_shutdown: Arc<AtomicBool>,
    }

    #[actor]
    impl NativeActor for DagCapability {
        type Config = ();
        /// ADR-0047 §4 + ADR-0078: chassis-internal actor under the
        /// `aether.<name>` namespace.
        const NAMESPACE: &'static str = "aether.dag";

        /// Build the executor against the substrate's shared `Mailer`
        /// (which surfaces the routing registry, capability registry,
        /// handle store, and settlement registry the executor needs)
        /// and spawn the reaping timer.
        fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let mailer: Arc<Mailer> = ctx.mailer();
            let self_id: MailboxId = ctx.self_id();
            let executor = Executor::new(Arc::clone(&mailer), self_id, ExecutorConfig::from_env());

            // Detached timer thread: fire a `DagReapTick` wake mail at
            // the cap every `REAP_INTERVAL`. The mail wakes the
            // single-threaded dispatcher, which runs the sweep on the
            // executor's own thread.
            let reap_shutdown = Arc::new(AtomicBool::new(false));
            let reap_shutdown_for_thread = Arc::clone(&reap_shutdown);
            let reap_kind = KindId(<DagReapTick as Kind>::ID.0);
            let timer_mailer = Arc::clone(&mailer);
            let spawned = thread::Builder::new()
                .name("aether-dag-reaper".to_owned())
                .spawn(move || {
                    while !reap_shutdown_for_thread.load(Ordering::Acquire) {
                        thread::sleep(REAP_INTERVAL);
                        if reap_shutdown_for_thread.load(Ordering::Acquire) {
                            break;
                        }
                        timer_mailer.push(Mail::new(self_id, reap_kind, Vec::new(), 1));
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(
                    target: "aether_substrate::dag",
                    error = %e,
                    "failed to spawn DAG reaping timer thread; reaping disabled",
                );
            }

            Ok(Self {
                executor,
                reap_shutdown,
            })
        }

        fn unwire(&mut self, _ctx: &mut NativeCtx<'_>) {
            self.reap_shutdown.store(true, Ordering::Release);
            // Join the transform compute-pool workers (ADR-0048 §3).
            self.executor.shutdown();
        }

        /// Submit a computation DAG for validation + execution. Validation
        /// runs synchronously; the reply carries the structured
        /// [`DagError`](aether_kinds::DagError) on failure or the minted
        /// `DagId` + per-node output handles on success (ADR-0047 §1).
        ///
        /// # Agent
        /// Reply: `SubmitResult`.
        #[handler]
        fn on_submit(&mut self, ctx: &mut NativeCtx<'_>, mail: Submit) {
            let reply = match self.executor.submit(ctx, mail.descriptor) {
                SubmitOutcome::Ok {
                    dag_id,
                    output_handles,
                } => SubmitResult::Ok {
                    dag_id,
                    output_handles,
                },
                SubmitOutcome::Err { error } => SubmitResult::Err { error },
            };
            ctx.reply(&reply);
        }

        /// Cancel an in-flight DAG by its `DagId` (ADR-0047 §5).
        ///
        /// # Agent
        /// Reply: `CancelResult`.
        #[handler]
        fn on_cancel(&mut self, ctx: &mut NativeCtx<'_>, mail: Cancel) {
            let reply = self.executor.cancel(mail.dag_id);
            ctx.reply(&reply);
        }

        /// Query a DAG's execution status (ADR-0047 §1/§6). An unknown
        /// `DagId` (never submitted, or already reaped) reports as
        /// `Failed { error: "unknown dag <id>" }` — the existing error
        /// shape, no new wire variant.
        ///
        /// # Agent
        /// Reply: `StatusResult`.
        #[handler]
        fn on_status(&mut self, ctx: &mut NativeCtx<'_>, mail: Status) {
            let reply = self
                .executor
                .status(mail.dag_id)
                .unwrap_or_else(|| StatusResult::Failed {
                    node_id: aether_kinds::NodeId(0),
                    error: format!("unknown dag {}", mail.dag_id),
                });
            ctx.reply(&reply);
        }

        /// Settlement notice for a `Call` dispatch (ADR-0047 §4 step 4).
        /// Closes the call's bundle. Fires from the chassis settlement
        /// registry, not from external mail.
        ///
        /// # Agent
        /// Internal — not part of the cap's external surface.
        #[handler]
        fn on_settled(&mut self, ctx: &mut NativeCtx<'_>, mail: Settled) {
            self.executor.on_settled(ctx, mail.root);
        }

        /// Reaping timer wake (ADR-0047 §7). Sweeps terminal DAGs past
        /// retention + times out never-settling `Call`s. Fires from the
        /// cap's own timer thread, not from external mail.
        ///
        /// # Agent
        /// Internal — not part of the cap's external surface.
        #[handler]
        fn on_reap_tick(&mut self, _ctx: &mut NativeCtx<'_>, _mail: DagReapTick) {
            let _ = self.executor.reap();
        }

        /// Off-thread native-transform completion wake (ADR-0048 §3).
        /// The compute pool fires this after a transform `fn` returns (or
        /// panics); forward it to the executor, which pulls the stashed
        /// outcome and resolves / fails the node on this actor's own
        /// thread. Fires from the pool, not from external mail.
        ///
        /// # Agent
        /// Internal — not part of the cap's external surface.
        #[handler]
        fn on_transform_done(&mut self, ctx: &mut NativeCtx<'_>, mail: DagTransformDone) {
            self.executor.on_transform_complete(ctx, mail.job_id);
        }

        /// Catch-all reply interception. A source / `Call` reply lands
        /// here as an arbitrary kind correlated by the dispatch's
        /// correlation id; forward it to the executor. A reply with no
        /// matching pending dispatch (a late reply for a cancelled /
        /// completed DAG) is a silent no-op.
        ///
        /// # Agent
        /// Not user-callable — the executor's reply-correlation path.
        #[fallback]
        fn on_reply(&mut self, ctx: &mut NativeCtx<'_>, env: &Envelope) {
            let correlation = env.sender.correlation_id;
            let matched =
                self.executor
                    .on_reply(ctx, correlation, KindId(env.kind.0), env.payload.bytes());
            if !matched {
                tracing::debug!(
                    target: "aether_substrate::dag",
                    kind = %env.kind_name,
                    correlation,
                    "dag reply with no matching pending dispatch; dropping",
                );
            }
            let _ = MailId::NONE;
        }
    }
}

/// Address-resolution helper: the cap's mailbox id, derived from its
/// `NAMESPACE` via the standard name-hash. Convenience for chassis code
/// addressing the cap without a runtime lookup.
#[must_use]
pub fn dag_mailbox_id() -> aether_data::MailboxId {
    use aether_actor::Actor;
    aether_data::mailbox_id_from_name(<DagCapability as Actor>::NAMESPACE)
}

// Test-support kinds + actors for the DAG-executor scenario tests
// (iamacoffeepot/aether#976). Defined at module root (not nested in
// `mod tests`) so the `Kind` derive's inventory submission stays
// addressable from a path the linker keeps — and so the derive
// registers them in `aether_kinds::descriptors::all()` for the test
// substrate's registry walk. Whole module is `#[cfg(test)]`.
#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests;
