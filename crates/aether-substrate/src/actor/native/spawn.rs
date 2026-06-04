//! Spawn primitive for instanced actors (ADR-0079, issue 607 Phase 3).
//!
//! Builds on [`ActorRegistry`] (Phase 2) to add
//! the atomic register-and-spawn dance: validate subname → check
//! tombstones + name-owner uniqueness → call `A::init` on the caller's
//! thread → register the mailbox sink + insert `Live` entry under one
//! lock → pre-load `after_init` mail → spawn the dispatcher thread.
//!
//! Init failure drops partial state and returns `Err(InitFailed)`
//! before any thread spawns. ADR-0079 §Init lifecycle.
//!
//! Termination not implemented yet — instanced actors live for the
//! chassis's lifetime; their dispatcher thread exits when the
//! `Registry` drops (the sink handler's `Weak<Sender>` upgrade fails,
//! the mpsc disconnects). Phase 4 wires `unwire` + the monitor
//! primitive + tombstone population.

use std::any::TypeId;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use aether_actor::{HandlesKind, Instanced, NamespaceError, validate_namespace_segment};
use aether_data::{Kind, mailbox_id_from_name};
use aether_kinds::trace::Nanos;

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::dispatcher_slot::DispatcherSlot;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::{NativeActor, NativeDispatch, NativeInitCtx};
use crate::actor::registry::ActorRegistry;
use crate::chassis::ctx::MailboxWakeSlot;
use crate::chassis::error::BootError;
use crate::chassis::settlement::{TerminalDisposition, WaitOutcome, await_internal_signal};
use crate::mail::mailer::Mailer;
use crate::mail::registry::OwnedDispatch;
use crate::mail::registry::{NameConflict, Registry};
use crate::mail::{KindId, MailId, MailRef, MailboxId, ReplyTo};
use crate::runtime::lifecycle::FatalAborter;
use crate::scheduler::Drainable;
use crate::scheduler::SeizeHandle;
use crate::scheduler::WakeHandle;
use crate::scheduler::WakeSink;
use aether_actor::local;
use aether_actor::local::ActorSlots;
use std::sync::Weak;
use std::time::Duration;

/// How to derive the subname for a [`SpawnBuilder::finish`] call. The
/// full mailbox name is `"{A::NAMESPACE}:{subname}"`; the substrate
/// hashes that string deterministically (ADR-0029) to produce the
/// returned [`MailboxId`].
#[derive(Debug, Clone, Copy)]
pub enum Subname<'a> {
    /// Listener-allocated monotonic counter. Caller doesn't care which
    /// id the instance gets — useful for "spawn me one of these per
    /// connection" patterns where the listener tracks the resulting
    /// `MailboxId` directly.
    Counter,
    /// Caller-supplied subname. Must validate per
    /// [`validate_namespace_segment`] and must be unique within the
    /// owning prefix (no `:` separator, no control chars / whitespace,
    /// ≤ [`aether_actor::NAMESPACE_SEGMENT_MAX_LEN`] bytes).
    Named(&'a str),
}

/// Failure modes for the [`SpawnBuilder::finish`] spawn pipeline.
/// Returned in the order the lifecycle checks them: validate → owner
/// check → tombstone check → name uniqueness → init.
#[derive(Debug)]
pub enum SpawnError {
    /// Subname is empty, contains `:`, has control / whitespace
    /// chars, or exceeds the byte cap. See
    /// [`NamespaceError`].
    SubnameInvalid(NamespaceError),
    /// `A::NAMESPACE` is already owned by a different `TypeId`. Trips
    /// when an `Instanced` type tries to spawn under a namespace a
    /// `Singleton` already owns (or vice versa). ADR-0079 unique-owner
    /// invariant.
    NamespaceOwnedByOtherType {
        namespace: &'static str,
        owning_type: TypeId,
    },
    /// The full name was previously live and has been retired. Names
    /// don't recycle within a substrate's lifetime (ADR-0079 §Drop /
    /// lifecycle); pick a different subname.
    SubnameRetired { full_name: String },
    /// The full name is currently bound to a live mailbox.
    SubnameInUse { full_name: String },
    /// `A::init` returned an error. The actor's partial state dropped
    /// before this returns; no dispatcher thread was spawned.
    InitFailed(BootError),
}

/// Chassis-level spawn machinery (Phase 3). One per chassis; cloned as
/// `Arc<Spawner>` into every [`NativeBinding`] so per-handler
/// `NativeCtx::spawn_child` can reach it without explicit plumbing.
pub struct Spawner {
    registry: Arc<Registry>,
    actor_registry: Arc<ActorRegistry>,
    mailer: Arc<Mailer>,
    aborter: Arc<dyn FatalAborter>,
    /// Monotonic counter for [`Subname::Counter`]. Per-Spawner so each
    /// chassis runs its own sequence; not shared across substrates.
    counter: AtomicU64,
    /// Issue 635 PR C: chassis worker pool's wake sink — the ready-queue
    /// sender bundled with the spin/park coordinator (iamacoffeepot/aether#1064).
    /// Cloned into [`WakeHandle`]s when the Pooled spawn branch lands a
    /// slot.
    wake_sink: WakeSink,
    /// Issue 635 Phase 3: strong-Arc store for instanced
    /// [`Drainable`] slots spawned via the Pooled
    /// branch. Without this the slot dropped at end of `spawn_actor`
    /// and the [`WakeHandle`]'s `Weak` failed to
    /// upgrade — every wake after spawn would silently no-op.
    /// Slots live until the Spawner itself drops (chassis teardown);
    /// self-closing actors leave their slot Arc here as a small
    /// metadata leak (~80 B) that's reclaimed at teardown.
    ///
    /// Issue 685: each entry now also carries a [`WakeHandle`] clone
    /// so [`Self::shutdown_instanced`] can fire one wake per slot at
    /// chassis teardown — without it, a freshly-`signal_shutdown`-ed
    /// slot whose inbox is empty would never enter `run_cycle` to
    /// observe the flag.
    instanced_slots: Mutex<HashMap<MailboxId, InstancedSlotEntry>>,
}

/// One entry in [`Spawner::instanced_slots`]. Holds both the strong
/// `Arc<dyn Drainable>` (so the wake handle's `Weak` upgrades) and a
/// [`WakeHandle`] clone (so the chassis-teardown
/// path can wake the slot after signaling shutdown). Issue 685.
struct InstancedSlotEntry {
    slot: Arc<dyn Drainable>,
    wake: WakeHandle,
}

impl Spawner {
    pub fn new(
        registry: Arc<Registry>,
        actor_registry: Arc<ActorRegistry>,
        mailer: Arc<Mailer>,
        aborter: Arc<dyn FatalAborter>,
        wake_sink: WakeSink,
    ) -> Self {
        Self {
            registry,
            actor_registry,
            mailer,
            aborter,
            counter: AtomicU64::new(0),
            wake_sink,
            instanced_slots: Mutex::new(HashMap::new()),
        }
    }

    /// Borrow the chassis worker pool's wake sink (ready-queue sender +
    /// spin/park coordinator). The Pooled instanced spawn branch clones
    /// it into each slot's [`WakeHandle`].
    pub(crate) fn wake_sink(&self) -> &WakeSink {
        &self.wake_sink
    }

    /// Issue 685: walk every spawned instanced slot, signal shutdown
    /// on its binding, fire one wake so a pool worker picks it up and
    /// runs the close path (drain residual → `unwire` → registry
    /// close + monitor fan-out), then wait per-slot on a one-shot
    /// completion channel until every slot has finished or `timeout`
    /// elapses.
    ///
    /// Called from [`crate::chassis::builder::BootedPassives::shutdown_in_place`]
    /// before the singleton shutdowns walk. The ordering matters:
    /// spawned actors close *first* so their `MonitorNotice` mail
    /// reaches singleton watchers while they're still alive. The
    /// pool stays alive through this method (it drops via the
    /// `_pool: PoolHandle` field on `BootedPassives` which has a later
    /// drop order than the explicit `shutdown_in_place` call), so
    /// workers can drain the close cycles we just queued.
    ///
    /// Issue 714: the original implementation polled
    /// [`Drainable::is_closed`] every 2 ms with a
    /// `timeout`-bounded loop. Under nextest contention the worker that
    /// observed the wake could be scheduled out long enough that the
    /// 2 s deadline elapsed before the close cycle ran, surfacing as
    /// the `chassis_teardown_runs_unwire` flake. The waker now installs a
    /// one-shot `crossbeam_channel::bounded(1)` per entry; the slot's
    /// close cycle fires it after `unwire` + registry close land, so
    /// teardown wakes the instant the cycle settles instead of polling.
    ///
    /// Issue #1305: each close-done receiver is waited on via
    /// [`await_internal_signal`] with escalating patience rather than a
    /// bare wall-clock `recv_timeout`. A genuinely wedged close cycle is
    /// unrecoverable — `unwire` never ran, so teardown invariants are
    /// already corrupt — so the disposition is `Abort` in release
    /// (route the wedge through the Spawner's
    /// [`FatalAborter`]) and `Panic` in test/debug (so #1295's
    /// assertion fails attributably at the gate site instead of as a
    /// downstream `0 != 1`). The old silent `warn!`-and-return-anyway
    /// path that left an un-closed actor is gone.
    ///
    /// `round_budget` is the per-round patience interval (the log
    /// cadence); `cumulative_cap` is the total patience per slot before
    /// declaring a wedge.
    pub(crate) fn shutdown_instanced(&self, round_budget: Duration, cumulative_cap: Duration) {
        let entries: Vec<InstancedSlotEntry> = {
            let mut guard = self
                .instanced_slots
                .lock()
                .expect("instanced_slots mutex poisoned; fail-fast per ADR-0063");
            guard.drain().map(|(_id, entry)| entry).collect()
        };
        if entries.is_empty() {
            return;
        }
        // Wire one (tx, rx) per entry up-front. Installing the tx on
        // the slot before signalling shutdown ensures the close cycle
        // sees the sender to fire — even if the worker enters the
        // close path before `signal_shutdown` returns control. The
        // slot's `set_close_done_tx` fast-paths an already-closed slot
        // by firing immediately, so there's no race window where the
        // close cycle ran without seeing the tx.
        let mut waiters: Vec<crossbeam_channel::Receiver<()>> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let (tx, rx) = crossbeam_channel::bounded::<()>(1);
            entry.slot.set_close_done_tx(tx);
            waiters.push(rx);
            entry.slot.signal_shutdown();
            // Shutdown wake: schedule the slot so the worker observes
            // the shutdown signal. The CAS-win bool is meaningful only
            // for callers wiring up first-time scheduling races; here
            // we just need *some* worker to pick the slot up.
            let _ = entry.wake.wake();
        }
        // `Panic` in test/debug (attributable failure at the gate),
        // `Abort` in release (the wedge is unrecoverable — route it
        // through the Spawner's aborter). The helper diverges itself on
        // `Panic`; on `Abort` it hands back the wedge for us to abort.
        let disposition = if cfg!(debug_assertions) {
            TerminalDisposition::Panic
        } else {
            TerminalDisposition::Abort
        };
        for rx in &waiters {
            match await_internal_signal(
                rx,
                "shutdown_instanced.close_done",
                round_budget,
                cumulative_cap,
                disposition,
            ) {
                WaitOutcome::Settled => {}
                WaitOutcome::Wedged(wedge) => {
                    // `Abort` disposition (release): the close cycle
                    // never ran `unwire`; teardown invariants are
                    // corrupt and unrecoverable. Route through the
                    // Spawner's aborter — diverges.
                    self.aborter.abort(wedge.reason());
                }
            }
        }
    }

    /// Borrow the actor registry. Crate-private — substrate-internal
    /// dispatcher trampolines (instanced spawn close path, singleton
    /// boot path) use this to call `close_actor` / `mark_dead` /
    /// `try_claim_namespace` etc. Cap handlers reaching for the
    /// registry through `transport.spawner().actor_registry()` is
    /// the wrong shape — caps that supervise a fleet hold their own
    /// child map; caps that just send mail use the typed `ctx.actor`
    /// / `ctx.resolve_actor` shortcuts. ADR-0079 supervisor-as-cap
    /// pattern.
    pub(crate) fn actor_registry(&self) -> &Arc<ActorRegistry> {
        &self.actor_registry
    }

    /// Spawn an instanced actor. Caller threads the bootstrap mail
    /// envelopes through `after_init_mail` (in delivery order); pass
    /// an empty Vec for plain spawn. The `sender_for_after_init`
    /// stamps the originator on each envelope so the spawned actor's
    /// `ctx.reply_target()` resolves to the spawner.
    ///
    /// Per the issue 607 Phase 3 lifecycle:
    /// 1. Resolve / validate subname.
    /// 2. Claim or verify name-owner ownership of `A::NAMESPACE`.
    /// 3. Tombstone check.
    /// 4. Construct + init the actor on the caller's thread.
    /// 5. Register the mailbox sink (atomic with steps 6-7).
    /// 6. Insert `Live` entry into the actor registry.
    /// 7. Pre-load `after_init_mail` into the inbox.
    /// 8. Spawn dispatcher thread, move actor in.
    // Spawn pipeline runs as one linear function so the eight-step
    // sequence above maps 1:1 to the code. Splitting steps into
    // helpers would scatter the rollback bookkeeping each step relies
    // on across multiple sites.
    #[allow(clippy::too_many_lines)]
    fn spawn_actor<A>(
        self: Arc<Self>,
        subname: Subname<'_>,
        config: A::Config,
        after_init_mail: Vec<Envelope>,
        sender_for_init: ReplyTo,
    ) -> Result<MailboxId, SpawnError>
    where
        A: Instanced + NativeActor + NativeDispatch,
    {
        // 1. Resolve subname → string.
        let subname_str = match subname {
            Subname::Counter => self.counter.fetch_add(1, Ordering::Relaxed).to_string(),
            Subname::Named(s) => s.to_owned(),
        };
        validate_namespace_segment(&subname_str).map_err(SpawnError::SubnameInvalid)?;

        // 2. Claim namespace ownership (or verify).
        if let Err(owning) = self
            .actor_registry
            .try_claim_namespace(A::NAMESPACE, TypeId::of::<A>())
        {
            return Err(SpawnError::NamespaceOwnedByOtherType {
                namespace: A::NAMESPACE,
                owning_type: owning,
            });
        }

        // 3. Compute full name + id; tombstone check.
        let full_name = format!("{}:{}", A::NAMESPACE, subname_str);
        let id = MailboxId(mailbox_id_from_name(&full_name).0);
        if self.actor_registry.is_tombstoned(id) {
            return Err(SpawnError::SubnameRetired { full_name });
        }

        // 4. Construct + init on caller's thread. Build the inbox pair
        // up-front so init may publish its self-id by hashing
        // `full_name` (the spawn-side derivation matches
        // `NativeInitCtx::self_id`); spawn-thread doesn't exist yet.
        let (tx, rx) = mpsc::channel::<Envelope>();

        let transport = Arc::new(NativeBinding::new(
            Arc::clone(&self.mailer),
            id,
            Arc::clone(&self.aborter),
            // Pass the chassis's `Spawner` through so the spawned
            // actor can in turn `ctx.spawn_child` from its own
            // handlers.
            Some(Arc::clone(&self)),
        ));
        transport.install_inbox(rx);

        // Per-actor scratch storage (issue 582 / ADR-0074). Stamped
        // into TLS via `local::with_stamped` for the duration of
        // `init` and each handler dispatch so library code inside
        // the actor (e.g., the issue-581 log buffer, `Local<T>`
        // slots) can reach `Local::with_mut` without threading a
        // ctx through. Mirrors the singleton path in
        // `chassis::builder::make_native_actor_boot` (issue 672).
        let slots = Box::new(ActorSlots::new());

        let actor = {
            // Instanced actors don't publish driver-facing sub-handles
            // today — Phase 4+ may revisit. Pass a throwaway
            // ExportedHandles to keep the init-ctx shape uniform with
            // the singleton path.
            let mut throwaway_handles = crate::ExportedHandles::new();
            let mut init_ctx =
                NativeInitCtx::new(&transport, &mut throwaway_handles, Arc::clone(&self.mailer));
            // ADR-0081: wrap `init` in `with_stamped` so any
            // `tracing::*` event the actor fires lands in its
            // per-actor `ActorLogRing`. The pre-ADR
            // `with_actor_dispatch` + `drain_buffer` flush hop
            // retired alongside `LogBatch`.
            let init_result = local::with_stamped(&slots, || A::init(config, &mut init_ctx));
            match init_result {
                Ok(a) => a,
                Err(e) => return Err(SpawnError::InitFailed(e)),
            }
        };

        // 5-7. Register sink + Live entry + pre-load mail. The actor
        // registry's `insert_live` and the mailbox registry's
        // `try_register_inbox` each take their own write lock; a
        // collision on either step rolls back. Sequence chosen so the
        // sink is the gating step (its `try_register_inbox` is the
        // only op that can fail with a name collision against a peer
        // singleton claim — the actor_registry slot is keyed on
        // MailboxId which already passed the tombstone check).
        //
        // The strong `Arc<Sender>` lives in the actor_registry's
        // Live entry. The sink handler's `Weak<Sender>` upgrades only
        // while the Arc is alive — i.e. while the actor's slot is
        // Live. On `mark_dead` the Arc drops, the weak upgrade fails,
        // and external mail addressed to the dead mailbox warn-drops.
        let strong_sender: Arc<mpsc::Sender<Envelope>> = Arc::new(tx.clone());
        let weak_for_handler = Arc::downgrade(&strong_sender);
        // Per-actor inbox-pending counter. Sink handler ++ on push;
        // dispatcher slot -- after handling. Pre-PR-4 the test bench's
        // `wait_instanced_quiesce` queried this through
        // `Spawner::instanced_pending`; ADR-0080 settlement gating
        // retires that polling path. The counter survives because the
        // `DispatcherSlot` still threads it as the optional `pending`
        // arg, but nothing queries the value today — a future cleanup
        // PR can drop it entirely.
        let pending = Arc::new(AtomicU64::new(0));
        let pending_for_handler = Arc::clone(&pending);
        // Issue 635 PR C: pool wake hook. Populated post-init below
        // (every actor is pool-dispatched since issue 1187); empty until
        // then so the closure's `get()` is a single relaxed atomic load.
        let wake_slot: Arc<MailboxWakeSlot> = Arc::new(MailboxWakeSlot::default());
        let wake_for_handler = Arc::clone(&wake_slot);
        // iamacoffeepot/aether#848 PR 3: closure takes `OwnedDispatch`
        // directly. Payload + kind_name + origin all move into the
        // forwarded `Envelope` via `From`; no clones on the
        // instanced-actor hot path. The pre-send increment + on-fail
        // decrement bracket the `tx.send` exactly as before; the
        // failure branch reads `env.kind_name` out of `SendError`
        // since `dispatch` was already moved into `env`.
        let registered = self.registry.try_register_inbox(
            full_name.clone(),
            Arc::new(move |dispatch: OwnedDispatch| {
                let Some(tx) = weak_for_handler.upgrade() else {
                    // ADR-0094: the actor's sender is gone — the mail is
                    // discarded at this relay seam, not held here, so
                    // mark the obligation transferred (the dropped-mail
                    // accounting is a separate concern, not this guard's).
                    dispatch.mark_transferred();
                    tracing::warn!(
                        target: "aether_substrate::spawn",
                        kind = %dispatch.kind_name,
                        "instanced actor sender dropped — mail discarded"
                    );
                    return;
                };
                // ADR-0094: the success path moves `env` onto the channel
                // (a transfer — the obligation rides the value); the
                // actor's dispatcher discharges it on drain. No annotation
                // here because the value is not dropped at this seam.
                let env: Envelope = dispatch;
                pending_for_handler.fetch_add(1, Ordering::AcqRel);
                if let Err(mpsc::SendError(env)) = tx.send(env) {
                    // Receiver disconnected before we could deliver;
                    // un-account for the increment so the counter
                    // stays accurate (a future post-PR-4 cleanup may
                    // drop the counter entirely now that
                    // `wait_instanced_quiesce` retired).
                    pending_for_handler.fetch_sub(1, Ordering::AcqRel);
                    // ADR-0094: discarded at the relay seam — transfer.
                    env.mark_transferred();
                    tracing::warn!(
                        target: "aether_substrate::spawn",
                        kind = %env.kind_name,
                        "instanced actor receiver dropped — mail discarded"
                    );
                    return;
                }
                if let Some(wake) = wake_for_handler.get() {
                    wake();
                }
            }),
        );
        let _ = sender_for_init; // Phase 3 doesn't stamp pre-load mail with the spawner; envelopes are pre-built by SpawnBuilder.
        match registered {
            Ok(returned_id) => debug_assert_eq!(returned_id, id),
            Err(NameConflict { name }) => return Err(SpawnError::SubnameInUse { full_name: name }),
        }

        // Issue 629 / Phase A: dispatcher takes Box<A> ownership.
        // The chassis-side actor_registry no longer holds a clone of
        // the actor — only the sender + type_id + subname for routing
        // and resolve_actor.
        let mut actor: Box<A> = Box::new(actor);

        // Insert before pre-loading mail: the actor_registry holding
        // the sender is the canonical record that the slot is live.
        // The Arc<Sender> here is the same one the sink handler's
        // Weak references — when `mark_dead` drops this entry, the
        // weak upgrade fails for any further external mail.
        if self
            .actor_registry
            .insert_live(
                id,
                Arc::clone(&strong_sender),
                TypeId::of::<A>(),
                subname_str,
            )
            .is_err()
        {
            // Hash collision against an existing Live entry on the
            // same id but a slot the mailbox registry didn't reject —
            // possible if a singleton + instanced collide on the same
            // 64-bit id even with distinct names. Treat as
            // SubnameInUse for the caller; the singleton's claim wins
            // (it landed first).
            //
            // Issue 607 Phase 7: the sink WAS registered above; remove
            // it before returning so the failed spawn doesn't leave
            // a dangling sink that warn-drops mail. The actor itself
            // (init succeeded) drops naturally as `actor` falls out
            // of scope.
            self.registry.remove_closure(id);
            return Err(SpawnError::SubnameInUse { full_name });
        }

        // Issue 584 Phase 2a (ADR-0079 amended): post-init mail-allowed
        // hook. Sink + actor_registry insert_live above means the
        // mailbox is fully published; peers are addressable and any
        // wire-time self-mail lands in this binding's inbox before the
        // dispatcher pulls. Runtime-spawn doesn't need the chassis-boot
        // multi-pass barrier (issue 697) because the substrate is
        // already steady-state when `Spawner::spawn_actor` runs — the
        // child wire→dispatcher transition is sequential within this
        // ctx, peers are running, all mailboxes claimed.
        local::with_stamped(&slots, || {
            let mut wire_ctx = crate::actor::native::NativeCtx::new(
                &transport,
                ReplyTo::NONE,
                MailId::NONE,
                MailId::NONE,
            );
            actor.wire(&mut wire_ctx);
        });

        // Pre-load bootstrap mail. tx is alive (rx is held by the
        // transport; nobody's polling yet), so these sends always
        // succeed. The per-actor `pending` counter increments here
        // for symmetry with the sink-handler path; the value is no
        // longer queried (PR 4 retired `wait_instanced_quiesce`).
        for env in after_init_mail {
            pending.fetch_add(1, Ordering::AcqRel);
            // mpsc::Sender::send only fails when the receiver
            // disconnects; rx is alive here. Discard on the
            // theoretical impossibility.
            let _ = tx.send(env);
        }

        // 8. Pool-register the dispatcher (every actor is pool-dispatched
        // since issue 1187 removed the per-thread `Dedicated` opt-out).
        // The local strong Arc was the populator for the Weak handler
        // ref; the actor_registry now holds an `Arc::clone` of the
        // same Arc, so dropping the local doesn't break the weak.
        drop(strong_sender);
        // Issue 635 PR C + Phase 3: register a `DispatcherSlot` with the
        // chassis worker pool. No per-actor thread. The wake hook on the
        // closure pushes the slot to the ready queue when an envelope
        // lands.
        let slot = DispatcherSlot::<A>::new(
            actor,
            Arc::clone(&transport),
            slots,
            Some(Arc::clone(&pending)),
            Arc::clone(&self.actor_registry),
            Arc::clone(&self.mailer),
            id,
        );
        let slot_dyn: Arc<dyn Drainable> = slot.clone();
        let weak: Weak<dyn Drainable> = Arc::downgrade(&slot_dyn);
        // iamacoffeepot/aether#1135: surface the seize handle on this
        // instanced actor's `Inbox` entry so the blob demuxer dispatches
        // its fan-out in place (ADR-0087 §4). The registry holds the
        // strong slot ref via `instanced_slots` below; the demuxer's
        // `Weak` upgrade fails cleanly once the actor is torn down.
        self.registry.install_seize_handle(
            id,
            SeizeHandle::new(Arc::clone(slot.state()), Arc::downgrade(&slot_dyn)),
        );
        let wake = WakeHandle::new(Arc::clone(slot.state()), weak, self.wake_sink.clone());
        // Stash the slot's strong Arc so wakes can upgrade their `Weak`.
        // PR C dropped it here, which broke every wake after spawn (the
        // registry only holds the inbox sender, not the slot — the
        // comment claiming otherwise was wrong). Slots live until the
        // Spawner itself drops at chassis teardown. Issue 685 also
        // stashes a wake clone so chassis teardown can fire one wake per
        // slot after signaling shutdown.
        drop(slot);
        let teardown_wake = wake.clone();
        self.instanced_slots
            .lock()
            .expect("instanced_slots mutex poisoned; fail-fast per ADR-0063")
            .insert(
                id,
                InstancedSlotEntry {
                    slot: slot_dyn,
                    wake: teardown_wake,
                },
            );
        // Pre-loaded `after_init` mail (lines above) was sent straight to
        // the inbox via `tx.send`, which bypasses the closure's wake
        // hook. Fire one wake now so the slot enters the ready queue and
        // the worker drains those envelopes; subsequent peer sends route
        // through the closure and wake on their own.
        let manual_wake = wake.clone();
        wake_slot.set(Arc::new(move || {
            // Inbox-sender hook: the CAS-win bool would tell us whether
            // *this* sender owns the schedule push, but the scheduler
            // self-deduplicates so either outcome is fine.
            let _ = wake.wake();
        }));
        // Manual catch-up wake for inbox mail that landed before the
        // closure was installed (see comment above).
        let _ = manual_wake.wake();

        Ok(id)
    }
}

/// Builder returned from `NativeCtx::spawn_child` /
/// `BuiltChassis::spawn_actor` / `PassiveChassis::spawn_actor`. Lets
/// the caller chain `after_init` to pre-load bootstrap mail before
/// committing with `finish`.
///
/// Holds the spawner reference borrowed from the calling ctx's
/// transport, the resolved subname, the consumed config, and the
/// running list of after-init envelopes. `finish` consumes the
/// builder and runs the spawn lifecycle.
pub struct SpawnBuilder<'ctx, A: Instanced + NativeActor + NativeDispatch> {
    spawner: Arc<Spawner>,
    subname: Subname<'ctx>,
    config: Option<A::Config>,
    sender: ReplyTo,
    after_init: Vec<Envelope>,
    _marker: PhantomData<fn() -> A>,
    /// Carries the `'ctx` lifetime even though `spawner` is `Arc`
    /// (no longer borrowed). The lifetime ties `Subname::Named(&str)`
    /// to whatever borrow it was constructed from at the call site,
    /// so a stack-local subname doesn't dangle past `finish()`.
    _ctx: PhantomData<&'ctx ()>,
}

impl<'ctx, A: Instanced + NativeActor + NativeDispatch> SpawnBuilder<'ctx, A> {
    /// Internal constructor. Public only because chassis-level
    /// `spawn_actor` entry points (on `BuiltChassis` / `PassiveChassis`)
    /// build these too.
    pub(crate) fn new(
        spawner: Arc<Spawner>,
        subname: Subname<'ctx>,
        config: A::Config,
        sender: ReplyTo,
    ) -> Self {
        Self {
            spawner,
            subname,
            config: Some(config),
            sender,
            after_init: Vec::new(),
            _marker: PhantomData,
            _ctx: PhantomData,
        }
    }

    /// Append `mail` to the bootstrap sequence. Order-preserving —
    /// the spawned actor sees envelopes in the order they were added.
    /// Sender on each envelope is the spawner's reply target; `reply_to`
    /// defaults to the spawner's mailbox.
    ///
    /// `A: HandlesKind<K>` ensures only kinds the actor's handler set
    /// covers can be pre-loaded; the strict-receiver miss path stays
    /// off the bootstrap surface.
    // `mail` is taken by value so the builder API mirrors the rest of
    // the spawn surface (`config: A::Config` is also by value); the
    // value flows straight into `encode_into_bytes` whose owned form
    // matches `Kind`'s wire-encoding convention.
    #[allow(clippy::needless_pass_by_value)]
    #[must_use]
    pub fn after_init<K>(mut self, mail: K) -> Self
    where
        A: HandlesKind<K>,
        K: Kind,
    {
        let payload = mail.encode_into_bytes();
        // ADR-0094: the bootstrap seed carries no settlement lineage
        // (`MailId::NONE`), so it is built *disarmed* — there is no
        // obligation to discharge (and `dispatch_one` no-ops its
        // `record_finished` on `NONE` anyway).
        let env = Envelope::disarmed(
            KindId(<K as Kind>::ID.0),
            <K as Kind>::NAME.to_owned(),
            None,
            self.sender,
            MailRef::from(payload),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            // Bootstrap seed carries no lineage (`MailId::NONE`), so it
            // never folds into a traced tree node — no deposit instant to
            // record (iamacoffeepot/aether#1134).
            Nanos(0),
            0,
            MailboxId(0),
        );
        self.after_init.push(env);
        self
    }

    /// Consume the builder and run the spawn lifecycle. Returns the
    /// new actor's [`MailboxId`] on success, or a typed [`SpawnError`]
    /// describing which lifecycle step failed.
    ///
    /// # Panics
    /// Panics if `finish` is called more than once on the same builder
    /// (the `Config` slot is taken on first call) — fail-fast per
    /// ADR-0063: `SpawnBuilder` is a single-use type, the typestate is
    /// enforced by the move into `finish`, and a double-finish would
    /// require an unsafe API misuse.
    pub fn finish(self) -> Result<MailboxId, SpawnError> {
        let SpawnBuilder {
            spawner,
            subname,
            config,
            sender,
            after_init,
            ..
        } = self;
        let config = config.expect("SpawnBuilder::finish consumed exactly once");
        Spawner::spawn_actor::<A>(spawner, subname, config, after_init, sender)
    }
}
