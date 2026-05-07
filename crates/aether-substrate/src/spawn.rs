//! Spawn primitive for instanced actors (ADR-0079, issue 607 Phase 3).
//!
//! Builds on [`crate::actor_registry::ActorRegistry`] (Phase 2) to add
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
//! the mpsc disconnects). Phase 4 wires `on_close` + the monitor
//! primitive + tombstone population.

use std::any::TypeId;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};

use aether_actor::{HandlesKind, Instanced, NamespaceError, validate_namespace_segment};
use aether_data::{Kind, mailbox_id_from_name};

use crate::actor_registry::ActorRegistry;
use crate::capability::{BootError, Envelope};
use crate::lifecycle::FatalAborter;
use crate::mail::{KindId, MailboxId, ReplyTo};
use crate::mailer::Mailer;
use crate::native_actor::{NativeActor, NativeDispatch, NativeInitCtx};
use crate::native_transport::NativeTransport;
use crate::registry::{NameConflict, Registry};

/// How to derive the subname for a [`Spawner::spawn_actor`] call. The
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

/// Failure modes for [`Spawner::spawn_actor`] / [`SpawnBuilder::finish`].
/// Returned in the order the lifecycle checks them: validate → owner
/// check → tombstone check → name uniqueness → init.
#[derive(Debug)]
pub enum SpawnError {
    /// Subname is empty, contains `:`, has control / whitespace
    /// chars, or exceeds the byte cap. See
    /// [`aether_actor::NamespaceError`].
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
/// `Arc<Spawner>` into every [`NativeTransport`] so per-handler
/// `NativeCtx::spawn_child` can reach it without explicit plumbing.
pub struct Spawner {
    registry: Arc<Registry>,
    actor_registry: Arc<ActorRegistry>,
    mailer: Arc<Mailer>,
    frame_bound_set: Arc<RwLock<HashSet<MailboxId>>>,
    aborter: Arc<dyn FatalAborter>,
    /// Monotonic counter for [`Subname::Counter`]. Per-Spawner so each
    /// chassis runs its own sequence; not shared across substrates.
    counter: AtomicU64,
}

impl Spawner {
    pub fn new(
        registry: Arc<Registry>,
        actor_registry: Arc<ActorRegistry>,
        mailer: Arc<Mailer>,
        frame_bound_set: Arc<RwLock<HashSet<MailboxId>>>,
        aborter: Arc<dyn FatalAborter>,
    ) -> Self {
        Self {
            registry,
            actor_registry,
            mailer,
            frame_bound_set,
            aborter,
            counter: AtomicU64::new(0),
        }
    }

    /// Borrow the actor registry. Read-only; spawn is the sole writer
    /// in Phase 3 (Phase 4 adds the close path).
    pub fn actor_registry(&self) -> &Arc<ActorRegistry> {
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

        let transport = Arc::new(NativeTransport::new(
            Arc::clone(&self.mailer),
            id,
            // Phase 3 instanced actors are free-running. Frame-barrier
            // semantics for instanced types arrive when (if) a
            // forcing function emerges; until then, false.
            false,
            Arc::clone(&self.frame_bound_set),
            Arc::clone(&self.aborter),
            // Pass the chassis's `Spawner` through so the spawned
            // actor can in turn `ctx.spawn_child` from its own
            // handlers.
            Some(Arc::clone(&self)),
        ));
        transport.install_inbox(rx);

        let actor = {
            let empty_actors = crate::Actors::new();
            let mut init_ctx =
                NativeInitCtx::new(&transport, &empty_actors, Arc::clone(&self.mailer));
            match A::init(config, &mut init_ctx) {
                Ok(a) => a,
                Err(e) => return Err(SpawnError::InitFailed(e)),
            }
        };

        // 5-7. Register sink + Live entry + pre-load mail. The actor
        // registry's `insert_live` and the mailbox registry's
        // `try_register_sink` each take their own write lock; a
        // collision on either step rolls back. Sequence chosen so the
        // sink is the gating step (its `try_register_sink` is the
        // only op that can fail with a name collision against a peer
        // singleton claim — the actor_registry slot is keyed on
        // MailboxId which already passed the tombstone check).
        let tx_for_sink = Arc::new(tx.clone());
        let weak_for_handler = Arc::downgrade(&tx_for_sink);
        let registered = self.registry.try_register_sink(
            full_name.clone(),
            Arc::new(
                move |kind: KindId,
                      kind_name: &str,
                      origin: Option<&str>,
                      sender: ReplyTo,
                      payload: &[u8],
                      count: u32| {
                    let Some(tx) = weak_for_handler.upgrade() else {
                        tracing::warn!(
                            target: "aether_substrate::spawn",
                            kind = kind_name,
                            "instanced actor sender dropped — mail discarded"
                        );
                        return;
                    };
                    let env = Envelope {
                        kind,
                        kind_name: kind_name.to_owned(),
                        origin: origin.map(str::to_owned),
                        sender,
                        payload: payload.to_vec(),
                        count,
                    };
                    if tx.send(env).is_err() {
                        tracing::warn!(
                            target: "aether_substrate::spawn",
                            kind = kind_name,
                            "instanced actor receiver dropped — mail discarded"
                        );
                    }
                },
            ),
        );
        let _ = sender_for_init; // Phase 3 doesn't stamp pre-load mail with the spawner; envelopes are pre-built by SpawnBuilder.
        match registered {
            Ok(returned_id) => debug_assert_eq!(returned_id, id),
            Err(NameConflict { name }) => return Err(SpawnError::SubnameInUse { full_name: name }),
        }

        let actor_arc: Arc<A> = Arc::new(actor);

        // Insert before pre-loading mail: the actor_registry holding
        // the sender is the canonical record that the slot is live.
        if self
            .actor_registry
            .insert_live(
                id,
                tx.clone(),
                Arc::clone(&actor_arc) as Arc<dyn std::any::Any + Send + Sync>,
                TypeId::of::<A>(),
            )
            .is_err()
        {
            // Hash collision against an existing Live entry on the
            // same id but a slot the mailbox registry didn't reject —
            // possible if a singleton + instanced collide on the same
            // 64-bit id even with distinct names. Treat as
            // SubnameInUse for the caller; the singleton's claim wins
            // (it landed first).
            return Err(SpawnError::SubnameInUse { full_name });
        }

        // Pre-load bootstrap mail. tx is alive (rx is held by the
        // transport; nobody's polling yet), so these sends always
        // succeed.
        for env in after_init_mail {
            // mpsc::Sender::send only fails when the receiver
            // disconnects; rx is alive here. Discard on the
            // theoretical impossibility.
            let _ = tx.send(env);
        }

        // 8. Spawn dispatcher thread, move actor in. Mirrors the
        // existing `boot_native_actor` shape minus the frame-bound
        // pending-counter decrement (instanced actors are
        // free-running per the comment above).
        let actor_for_thread = Arc::clone(&actor_arc);
        let transport_for_thread = Arc::clone(&transport);
        let thread_name = alloc_instanced_thread_name(&full_name);
        // The strong sink Arc was held only to populate the Weak
        // handler reference; drop the local strong here so the
        // handler's upgrade tracks dispatcher liveness through the
        // tx clone the actor_registry holds.
        drop(tx_for_sink);
        let _ = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                while let Some(env) = transport_for_thread.recv_blocking() {
                    let mut native_ctx =
                        crate::native_actor::NativeCtx::new(&transport_for_thread, env.sender);
                    if actor_for_thread
                        .__aether_dispatch_envelope(&mut native_ctx, env.kind, &env.payload)
                        .is_none()
                    {
                        tracing::warn!(
                            target: "aether_substrate::spawn",
                            actor = A::NAMESPACE,
                            kind = env.kind_name.as_str(),
                            "instanced actor dispatch missed: kind not handled or decode failed"
                        );
                    }
                }
                // Dispatcher exit — actor_arc drops here when the loop
                // ends. Phase 4 will wire `on_close` here and flip the
                // registry slot to `Dead` + insert a tombstone.
            })
            .expect("dispatcher thread spawn must succeed");

        Ok(id)
    }
}

fn alloc_instanced_thread_name(full_name: &str) -> String {
    let mut s = String::with_capacity("aether-instanced-".len() + full_name.len());
    s.push_str("aether-instanced-");
    s.push_str(full_name);
    s
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
    /// Sender on each envelope is the spawner's reply target; reply_to
    /// defaults to the spawner's mailbox.
    ///
    /// `A: HandlesKind<K>` ensures only kinds the actor's handler set
    /// covers can be pre-loaded; the strict-receiver miss path stays
    /// off the bootstrap surface.
    pub fn after_init<K>(mut self, mail: K) -> Self
    where
        A: HandlesKind<K>,
        K: Kind,
    {
        let payload = mail.encode_into_bytes();
        let env = Envelope {
            kind: KindId(<K as Kind>::ID.0),
            kind_name: <K as Kind>::NAME.to_owned(),
            origin: None,
            sender: self.sender,
            payload,
            count: 1,
        };
        self.after_init.push(env);
        self
    }

    /// Consume the builder and run the spawn lifecycle. Returns the
    /// new actor's [`MailboxId`] on success, or a typed [`SpawnError`]
    /// describing which lifecycle step failed.
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
