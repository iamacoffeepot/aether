//! `aether.control` cap (issue 603). The wasm-component supervisor:
//! the single owner of every wasm component's lifetime, table, and
//! dispatcher thread. Pre-extraction the supervisor was split between
//! `aether-substrate::control::ControlPlane` (decided load / drop /
//! replace) and `aether-substrate::scheduler::Scheduler` (owned the
//! dispatcher threads + table); the two sides shared `ComponentTable`
//! by `Arc<RwLock<...>>`. Phase 1 of issue 603 collapses both into one
//! cap that owns the table outright and installs itself as the
//! `Mailer`'s [`ComponentRouter`] for component-bound routing.
//!
//! The cap is a `#[bridge] mod native { ... }` per the ADR-0076 /
//! issue 565 pattern. `Actor::NAMESPACE = "aether.control"`. Per-kind
//! `#[handler]` methods replace the legacy `dispatch` match; the
//! macro-emitted `ConfigureLogDrain` handler covers the per-actor
//! `LogDrainSlot` for free, and the cap reads `current_drain()` in
//! `handle_load` to push the same drain into freshly-loaded
//! components.
//!
//! Chassis-peripheral kinds (`capture_frame`, `set_window_mode`,
//! `set_window_title`, `platform_info`) flow through the
//! [`ChassisControlHandler`] closure on `ControlPlaneConfig` for the
//! Phase 1 migration window. Phases 2–4 of issue 603 peel them off
//! into their own caps and the closure retires.
//!
//! Dispatcher infrastructure (`PendingGate`, `ComponentEntry`,
//! `dispatcher_loop`, `kill_actor`, `splice_inbox`,
//! `spawn_dispatcher_on`, `close_and_join`) lives at the bottom of
//! the `mod native` body. Resolved Decision §1: hand-rolled here
//! until the shape converges with `make_native_actor_boot`'s thread
//! spawn.

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::{
    DropComponent, LoadComponent, ReplaceComponent, SubscribeInput, UnsubscribeInput,
};

// `ControlPlaneConfig` and `ChassisControlHandler` are exported from
// inside `mod native` for native callers (the cap struct itself is
// auto-emitted at file root by `#[bridge]`).
#[cfg(not(target_arch = "wasm32"))]
pub use native::{ChassisControlHandler, ControlPlaneConfig};

#[aether_actor::bridge]
mod native {
    use std::collections::HashMap;
    use std::panic::AssertUnwindSafe;
    use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Condvar, Mutex, RwLock};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use aether_actor::actor;
    use aether_data::{Kind, KindDescriptor};
    use aether_kinds::{
        ComponentCapabilities, ComponentDied, DropResult, LoadResult, ReplaceResult,
        SubscribeInputResult,
    };
    use wasmtime::{Engine, Linker, Module};

    use super::{DropComponent, LoadComponent, ReplaceComponent, SubscribeInput, UnsubscribeInput};

    use aether_substrate::capability::{BootError, Envelope};
    use aether_substrate::component::{Component, DISPATCH_UNKNOWN_KIND};
    use aether_substrate::control_helpers::{decode_payload, register_or_match_all};
    use aether_substrate::ctx::SubstrateCtx;
    use aether_substrate::input::{self, InputSubscribers};
    use aether_substrate::kind_manifest;
    use aether_substrate::mail::{Mail, MailboxId, ReplyTo};
    use aether_substrate::mailer::Mailer;
    use aether_substrate::native_actor::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::outbound::HubOutbound;
    use aether_substrate::registry::{MailboxEntry, Registry};
    use aether_substrate::supervisor::{
        ComponentRouter, ComponentSendOutcome, DrainDeath, DrainOutcome, DrainSummary,
    };

    /// ADR-0038 retains `ReplaceComponent::drain_timeout_ms` for wire
    /// compatibility but the field is no longer load-bearing: replace
    /// is a channel splice, so the "drain" phase is implicit in joining
    /// the old dispatcher.
    const DEFAULT_DRAIN_TIMEOUT_MS: u32 = 5_000;

    /// Closure contract for a chassis-registered control-plane fallback.
    /// Called for every mail arriving at `aether.control` whose kind
    /// isn't one the cap handles natively. The chassis is responsible
    /// for decoding, replying (via the outbound it constructed with),
    /// and any mail orchestration. Phase 1 migration shape — Phases
    /// 2–4 of issue 603 retire it as the chassis-peripheral kinds
    /// (`capture_frame`, `set_window_mode`, `set_window_title`,
    /// `platform_info`) move to their own caps.
    pub type ChassisControlHandler =
        Arc<dyn Fn(aether_data::KindId, &str, ReplyTo, &[u8]) + Send + Sync>;

    /// Configuration for [`ControlPlaneCapability`]. `engine` and
    /// `linker` are the wasmtime instances every load / replace
    /// instantiates against; `hub_outbound` is the egress handle the
    /// cap uses for `*Result` replies and `aether.kinds.changed`
    /// announcements; `input_subscribers` is the ADR-0021 fan-out
    /// table (shared with the platform thread).
    ///
    /// `chassis_handler` is the Phase 1 migration shape for chassis-
    /// peripheral kinds (§9 / Phases 2–4 retire it).
    pub struct ControlPlaneConfig {
        pub engine: Arc<Engine>,
        pub linker: Arc<Linker<SubstrateCtx>>,
        pub hub_outbound: Arc<HubOutbound>,
        pub input_subscribers: InputSubscribers,
        pub chassis_handler: Option<ChassisControlHandler>,
    }

    /// `aether.control` cap. Outer struct is a thin
    /// [`Arc<ControlPlaneInner>`] wrapper so [`Self::init`] can install
    /// the inner as the `Mailer`'s [`ComponentRouter`] before the
    /// chassis builder constructs the cap's own `Arc<Self>` (the
    /// `make_native_actor_boot` path Arcs the actor *after* `init`
    /// returns, so the cap can't install itself).
    pub struct ControlPlaneCapability {
        inner: Arc<ControlPlaneInner>,
    }

    /// State the cap captures and the [`ComponentRouter`] impl reads.
    /// Owned exclusively by the cap; the `Mailer`'s router slot holds
    /// an `Arc<Self>` so route lookups don't go through the cap struct
    /// (which is Arc-shared with the chassis Actors map and the
    /// dispatcher thread).
    struct ControlPlaneInner {
        engine: Arc<Engine>,
        linker: Arc<Linker<SubstrateCtx>>,
        registry: Arc<Registry>,
        queue: Arc<Mailer>,
        outbound: Arc<HubOutbound>,
        components: RwLock<HashMap<MailboxId, Arc<ComponentEntry>>>,
        input_subscribers: InputSubscribers,
        default_name_counter: AtomicU64,
        chassis_handler: Option<ChassisControlHandler>,
    }

    impl ControlPlaneCapability {
        /// Test-support constructor. Builds the cap with the supplied
        /// `registry` / `queue` and installs `ControlPlaneInner` as
        /// `mailer`'s `ComponentRouter`. Per Resolved Decision §3,
        /// narrow pure-handler unit tests use this; lifecycle tests
        /// (load → drop → replace, shutdown ordering) boot a real
        /// chassis via `Builder::with_actor::<ControlPlaneCapability>(...)`.
        ///
        /// `#[doc(hidden)] pub` so cross-crate test helpers (the
        /// `aether-substrate-bundle` integration tests) can reach for it
        /// without a feature flag — production code goes through
        /// `Builder::with_actor` and never hits this method.
        #[doc(hidden)]
        pub fn for_test(
            config: ControlPlaneConfig,
            registry: Arc<Registry>,
            queue: Arc<Mailer>,
        ) -> Self {
            let inner = Arc::new(ControlPlaneInner {
                engine: config.engine,
                linker: config.linker,
                registry,
                queue: Arc::clone(&queue),
                outbound: config.hub_outbound,
                components: RwLock::new(HashMap::new()),
                input_subscribers: config.input_subscribers,
                default_name_counter: AtomicU64::new(0),
                chassis_handler: config.chassis_handler,
            });
            queue.install_component_router(Arc::clone(&inner) as Arc<dyn ComponentRouter>);
            Self { inner }
        }

        /// Test-support: spawn a dispatcher thread for `component`
        /// against the pre-registered mailbox `id`. Mirrors the legacy
        /// `Scheduler::add_component` shape so cross-crate integration
        /// tests can attach a hand-built `Component` without going
        /// through `handle_load`'s wasm-bytes path.
        ///
        /// `#[doc(hidden)] pub` per the same rationale as
        /// [`Self::for_test`].
        #[doc(hidden)]
        pub fn attach_component_for_test(&self, id: MailboxId, component: Component) {
            self.inner.insert_component(id, component);
        }

        /// Test-support: report whether the supervisor table has an
        /// entry for `id`. Mirrors the legacy
        /// `plane.components.read().contains_key(&id)` peek that the
        /// pre-extraction `aether-substrate::control::tests` did.
        #[doc(hidden)]
        pub fn contains_component_for_test(&self, id: MailboxId) -> bool {
            self.inner.components.read().unwrap().contains_key(&id)
        }

        /// Test-support: dispatch a typed `LoadComponent` payload
        /// through the cap's load handler synchronously. Returns the
        /// `LoadResult` the cap would reply with; tests assert on it
        /// directly rather than threading a reply outbound. Mirrors
        /// `aether.control.load_component` mail end-to-end minus the
        /// dispatcher thread hop.
        #[doc(hidden)]
        pub fn load_for_test(&self, payload: LoadComponent) -> LoadResult {
            let bytes = postcard::to_allocvec(&payload).expect("encode LoadComponent");
            self.inner.handle_load_bytes(&bytes)
        }

        /// Test-support counterpart of [`Self::load_for_test`].
        #[doc(hidden)]
        pub fn drop_for_test(&self, payload: DropComponent) -> DropResult {
            let bytes = postcard::to_allocvec(&payload).expect("encode DropComponent");
            self.inner.handle_drop_bytes(&bytes)
        }

        /// Test-support counterpart of [`Self::load_for_test`].
        #[doc(hidden)]
        pub fn replace_for_test(&self, payload: ReplaceComponent) -> ReplaceResult {
            let bytes = postcard::to_allocvec(&payload).expect("encode ReplaceComponent");
            self.inner.handle_replace_bytes(&bytes)
        }

        /// Test-support counterpart of [`Self::load_for_test`].
        #[doc(hidden)]
        pub fn subscribe_for_test(&self, payload: SubscribeInput) -> SubscribeInputResult {
            let bytes = postcard::to_allocvec(&payload).expect("encode SubscribeInput");
            self.inner.handle_subscribe_bytes(&bytes)
        }

        /// Test-support counterpart of [`Self::load_for_test`].
        #[doc(hidden)]
        pub fn unsubscribe_for_test(&self, payload: UnsubscribeInput) -> SubscribeInputResult {
            let bytes = postcard::to_allocvec(&payload).expect("encode UnsubscribeInput");
            self.inner.handle_unsubscribe_bytes(&bytes)
        }
    }

    #[actor]
    impl NativeActor for ControlPlaneCapability {
        type Config = ControlPlaneConfig;
        const NAMESPACE: &'static str = "aether.control";

        fn init(
            config: ControlPlaneConfig,
            ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            let mailer = ctx.mailer();
            let registry = mailer.registry().cloned().ok_or_else(|| {
                BootError::Other(
                    std::io::Error::other(
                        "registry must be wired on Mailer before ControlPlaneCapability::init",
                    )
                    .into(),
                )
            })?;
            let inner = Arc::new(ControlPlaneInner {
                engine: config.engine,
                linker: config.linker,
                registry,
                queue: Arc::clone(&mailer),
                outbound: config.hub_outbound,
                components: RwLock::new(HashMap::new()),
                input_subscribers: config.input_subscribers,
                default_name_counter: AtomicU64::new(0),
                chassis_handler: config.chassis_handler,
            });
            // Install `inner` as the Mailer's component router. Mail
            // landing on `MailboxEntry::Component` from any thread now
            // goes through `inner` directly, without dipping into the
            // chassis Actors map.
            mailer.install_component_router(Arc::clone(&inner) as Arc<dyn ComponentRouter>);
            Ok(Self { inner })
        }

        /// Load a fresh wasm component into the substrate.
        ///
        /// # Agent
        /// Pass the wasm bytes plus an optional `name`. On Ok the cap
        /// registers the kinds the wasm declared in its `aether.kinds`
        /// section, mints a name-derived `MailboxId`, instantiates the
        /// `Component`, spawns its dispatcher thread, and replies with
        /// `LoadResult::Ok { mailbox_id, name, capabilities }`. Errors
        /// (bad postcard, kind conflict, name conflict, invalid wasm,
        /// instantiation trap) come back as `LoadResult::Err`.
        #[handler]
        fn on_load_component(&self, ctx: &mut NativeCtx<'_>, payload: LoadComponent) {
            let result = self.inner.handle_load(payload);
            ctx.transport()
                .send_reply_for_handler(ctx.reply_target(), &result);
        }

        /// Drop a component by its mailbox id.
        ///
        /// # Agent
        /// `DropComponent { mailbox_id }`. The cap removes the entry
        /// from its table, joins the dispatcher thread (after closing
        /// the inbox), runs the component's `on_drop` hook, and
        /// replies `DropResult::Ok` (or `Err` for unknown / sink /
        /// already-dropped ids).
        #[handler]
        fn on_drop_component(&self, ctx: &mut NativeCtx<'_>, payload: DropComponent) {
            let result = self.inner.handle_drop(payload);
            ctx.transport()
                .send_reply_for_handler(ctx.reply_target(), &result);
        }

        /// Replace the component at `mailbox_id` with a fresh wasm
        /// binary, preserving the mailbox id and any input
        /// subscriptions (ADR-0022 + ADR-0038 splice).
        ///
        /// # Agent
        /// `ReplaceComponent { mailbox_id, wasm, drain_timeout_ms }`.
        /// `drain_timeout_ms` is accepted for wire compatibility but
        /// ignored under ADR-0038's structural splice.
        #[handler]
        fn on_replace_component(&self, ctx: &mut NativeCtx<'_>, payload: ReplaceComponent) {
            let result = self.inner.handle_replace(payload);
            ctx.transport()
                .send_reply_for_handler(ctx.reply_target(), &result);
        }

        /// Subscribe a mailbox to an input stream (ADR-0021).
        ///
        /// # Agent
        /// `SubscribeInput { kind, mailbox }`. Component mailboxes only —
        /// sinks and dropped mailboxes are rejected.
        #[handler]
        fn on_subscribe_input(&self, ctx: &mut NativeCtx<'_>, payload: SubscribeInput) {
            let result = self.inner.handle_subscribe(payload);
            ctx.transport()
                .send_reply_for_handler(ctx.reply_target(), &result);
        }

        /// Unsubscribe a mailbox from an input stream (ADR-0021).
        ///
        /// # Agent
        /// `UnsubscribeInput { kind, mailbox }`. Idempotent on
        /// "not currently subscribed"; rejects unknown / sink mailboxes.
        #[handler]
        fn on_unsubscribe_input(&self, ctx: &mut NativeCtx<'_>, payload: UnsubscribeInput) {
            let result = self.inner.handle_unsubscribe(payload);
            ctx.transport()
                .send_reply_for_handler(ctx.reply_target(), &result);
        }

        /// Phase 1 migration safety net for chassis-peripheral kinds
        /// that haven't been peeled into their own caps yet
        /// (`capture_frame`, `set_window_mode`, `set_window_title`,
        /// `platform_info`). The chassis main supplies a closure on
        /// `ControlPlaneConfig.chassis_handler` that decodes + replies
        /// for these. Phases 2–4 of issue 603 retire the closure
        /// entirely as each kind moves to its own cap.
        #[fallback]
        fn on_chassis_kind(&self, _ctx: &mut NativeCtx<'_>, env: &Envelope) {
            if let Some(handler) = &self.inner.chassis_handler {
                handler(env.kind, env.kind_name.as_str(), env.sender, &env.payload);
            } else {
                tracing::warn!(
                    target: "aether_capabilities::control",
                    kind = %env.kind_name,
                    "aether.control received unrecognised kind (no chassis handler registered) — dropping",
                );
            }
        }
    }

    impl ComponentRouter for ControlPlaneInner {
        fn route(&self, recipient: MailboxId, mail: Mail) -> ComponentSendOutcome {
            let entry = self
                .components
                .read()
                .unwrap()
                .get(&recipient)
                .map(Arc::clone);
            let Some(entry) = entry else {
                return ComponentSendOutcome::Unknown;
            };
            // Issue 321 Phase 2: the dead-state check happens before
            // send so Mailer's mail-dropped warn can distinguish "actor
            // panicked / trapped" from "shutdown closed".
            if entry.is_dead() {
                return ComponentSendOutcome::Dead;
            }
            if entry.send(mail) {
                ComponentSendOutcome::Sent
            } else {
                ComponentSendOutcome::Closed
            }
        }

        fn drain_all_with_budget(&self, budget: Duration) -> DrainSummary {
            let mut summary = DrainSummary::default();
            loop {
                let entries: Vec<Arc<ComponentEntry>> =
                    self.components.read().unwrap().values().cloned().collect();
                for entry in &entries {
                    match entry.drain_with_budget(budget) {
                        DrainOutcome::Quiesced => {}
                        DrainOutcome::Died(d) => summary.deaths.push(d),
                        DrainOutcome::Wedged { waited } => {
                            summary.wedged = Some((entry.mailbox, waited));
                            return summary;
                        }
                    }
                }
                let still_busy = entries
                    .iter()
                    .any(|e| e.gate.pending.load(Ordering::Acquire) > 0);
                if !still_busy {
                    return summary;
                }
            }
        }
    }

    impl Drop for ControlPlaneInner {
        fn drop(&mut self) {
            // Chassis shutdown reverse-order: by the time the last Arc
            // to `ControlPlaneInner` drops, the cap's own dispatcher
            // (and every other cap) has already exited, so no fresh
            // mail is landing. Walk the table once, close each inbox,
            // and join the per-component dispatcher threads — the
            // structural counterpart to `handle_drop` running for every
            // entry simultaneously.
            //
            // Best-effort: an entry whose `Sender` was already taken
            // (concurrent `handle_drop` in flight at chassis exit) is
            // skipped silently. An entry whose dispatcher panicked
            // already updated `state` to `STATE_DEAD` and exited; the
            // join completes immediately.
            let entries: Vec<Arc<ComponentEntry>> = {
                let mut table = self.components.write().unwrap();
                table.drain().map(|(_, v)| v).collect()
            };
            for entry in entries {
                if let Some(mut component) = try_close_and_join(entry) {
                    component.on_drop();
                }
            }
        }
    }

    impl ControlPlaneInner {
        fn handle_load_bytes(&self, bytes: &[u8]) -> LoadResult {
            match decode_payload(bytes) {
                Ok(p) => self.handle_load(p),
                Err(error) => LoadResult::Err { error },
            }
        }

        fn handle_load(&self, payload: LoadComponent) -> LoadResult {
            let descriptors: Vec<KindDescriptor> =
                match kind_manifest::read_from_bytes(&payload.wasm) {
                    Ok(d) => d,
                    Err(error) => return LoadResult::Err { error },
                };
            if let Err(error) = register_or_match_all(&self.registry, &descriptors) {
                return LoadResult::Err { error };
            }

            let capabilities = match kind_manifest::read_inputs_from_bytes(&payload.wasm) {
                Ok(c) => c,
                Err(error) => return LoadResult::Err { error },
            };

            let module = match Module::new(&self.engine, &payload.wasm) {
                Ok(m) => m,
                Err(e) => {
                    return LoadResult::Err {
                        error: format!("invalid wasm module: {e}"),
                    };
                }
            };

            // Issue 525 Phase 1B: when the load payload omits an
            // explicit name, prefer the component's `aether.namespace`
            // section over a generic `component_N` slot.
            let name = match payload.name {
                Some(n) => n,
                None => match kind_manifest::read_namespace_from_bytes(&payload.wasm) {
                    Ok(Some(declared)) => declared,
                    Ok(None) => {
                        let n = self.default_name_counter.fetch_add(1, Ordering::Relaxed);
                        format!("component_{n}")
                    }
                    Err(error) => return LoadResult::Err { error },
                },
            };

            // ADR-0029: mailbox ids are name-derived, so we precompute
            // and pass to `init` *before* publishing — on instantiate
            // failure the registry is untouched and the name remains
            // available for a retry (issue 358 + #403).
            let mailbox = MailboxId::from_name(&name);

            let ctx = SubstrateCtx::new(
                mailbox,
                Arc::clone(&self.registry),
                Arc::clone(&self.queue),
                Arc::clone(&self.outbound),
                Arc::clone(&self.input_subscribers),
            );
            let mut component =
                match Component::instantiate(&self.engine, &self.linker, &module, ctx) {
                    Ok(c) => c,
                    Err(e) => {
                        return LoadResult::Err {
                            error: format!("wasm instantiation failed: {e}"),
                        };
                    }
                };

            let registered = match self.registry.try_register_component(&name) {
                Ok(id) => id,
                Err(e) => {
                    component.on_drop();
                    return LoadResult::Err {
                        error: e.to_string(),
                    };
                }
            };
            debug_assert_eq!(
                registered, mailbox,
                "registered mailbox id must match precomputed id from name hash",
            );

            // Issue #403: derive auto-subscriptions from the handler
            // manifest now that the mailbox is registered.
            auto_subscribe_inputs(
                &self.input_subscribers,
                &self.registry,
                mailbox,
                &capabilities,
            );

            self.insert_component(mailbox, component);
            self.announce_kinds();

            // Issue #601: dispatch the chassis-declared log drain to
            // the freshly-registered component. The cap reads its own
            // per-actor `LogDrainSlot` (set by the `#[actor]` macro's
            // auto-emitted `ConfigureLogDrain` handler when the chassis
            // Builder dispatched on boot) — no globals, no `OnceLock`
            // field on the cap.
            if let Some(drain) = aether_actor::log::current_drain() {
                let cfg = aether_kinds::ConfigureLogDrain { mailbox: drain };
                let payload = bytemuck::bytes_of(&cfg).to_vec();
                let kind = <aether_kinds::ConfigureLogDrain as aether_data::Kind>::ID;
                self.queue.push(Mail::new(mailbox, kind, payload, 1));
            }

            LoadResult::Ok {
                mailbox_id: mailbox,
                name,
                capabilities,
            }
        }

        fn handle_drop_bytes(&self, bytes: &[u8]) -> DropResult {
            match decode_payload(bytes) {
                Ok(p) => self.handle_drop(p),
                Err(error) => DropResult::Err { error },
            }
        }

        fn handle_drop(&self, payload: DropComponent) -> DropResult {
            let id = payload.mailbox_id;
            if let Err(e) = self.registry.drop_mailbox(id) {
                return DropResult::Err {
                    error: e.to_string(),
                };
            }
            input::remove_from_all(&self.input_subscribers, id);
            let Some(entry) = self.components.write().unwrap().remove(&id) else {
                return DropResult::Ok;
            };
            let mut component = close_and_join(entry);
            component.on_drop();
            DropResult::Ok
        }

        fn handle_subscribe_bytes(&self, bytes: &[u8]) -> SubscribeInputResult {
            match decode_payload(bytes) {
                Ok(p) => self.handle_subscribe(p),
                Err(error) => SubscribeInputResult::Err { error },
            }
        }

        fn handle_subscribe(&self, payload: SubscribeInput) -> SubscribeInputResult {
            let id = payload.mailbox;
            if let Err(e) = validate_subscriber_mailbox(&self.registry, id) {
                return SubscribeInputResult::Err { error: e };
            }
            self.input_subscribers
                .write()
                .unwrap()
                .entry(payload.kind)
                .or_default()
                .insert(id);
            SubscribeInputResult::Ok
        }

        fn handle_unsubscribe_bytes(&self, bytes: &[u8]) -> SubscribeInputResult {
            match decode_payload(bytes) {
                Ok(p) => self.handle_unsubscribe(p),
                Err(error) => SubscribeInputResult::Err { error },
            }
        }

        fn handle_unsubscribe(&self, payload: UnsubscribeInput) -> SubscribeInputResult {
            let id = payload.mailbox;
            if let Err(e) = validate_subscriber_mailbox(&self.registry, id) {
                return SubscribeInputResult::Err { error: e };
            }
            if let Some(set) = self
                .input_subscribers
                .write()
                .unwrap()
                .get_mut(&payload.kind)
            {
                set.remove(&id);
            }
            SubscribeInputResult::Ok
        }

        fn handle_replace_bytes(&self, bytes: &[u8]) -> ReplaceResult {
            match decode_payload(bytes) {
                Ok(p) => self.handle_replace(p),
                Err(error) => ReplaceResult::Err { error },
            }
        }

        fn handle_replace(&self, payload: ReplaceComponent) -> ReplaceResult {
            let id = payload.mailbox_id;
            let _drain_timeout_ms = payload.drain_timeout_ms.unwrap_or(DEFAULT_DRAIN_TIMEOUT_MS);

            match self.registry.entry(id) {
                Some(MailboxEntry::Component) => {}
                Some(MailboxEntry::Sink(_)) => {
                    return ReplaceResult::Err {
                        error: format!("mailbox {} is a sink, not a component", id.0),
                    };
                }
                Some(MailboxEntry::Dropped) => {
                    return ReplaceResult::Err {
                        error: format!("mailbox {} already dropped", id.0),
                    };
                }
                None => {
                    return ReplaceResult::Err {
                        error: format!("unknown mailbox id {}", id.0),
                    };
                }
            }

            let descriptors: Vec<KindDescriptor> =
                match kind_manifest::read_from_bytes(&payload.wasm) {
                    Ok(d) => d,
                    Err(error) => return ReplaceResult::Err { error },
                };
            if let Err(error) = register_or_match_all(&self.registry, &descriptors) {
                return ReplaceResult::Err { error };
            }

            let capabilities = match kind_manifest::read_inputs_from_bytes(&payload.wasm) {
                Ok(c) => c,
                Err(error) => return ReplaceResult::Err { error },
            };

            let module = match Module::new(&self.engine, &payload.wasm) {
                Ok(m) => m,
                Err(e) => {
                    return ReplaceResult::Err {
                        error: format!("invalid wasm module: {e}"),
                    };
                }
            };

            let entry = match self.components.read().unwrap().get(&id).map(Arc::clone) {
                Some(e) => e,
                None => {
                    return ReplaceResult::Err {
                        error: format!("mailbox {} has no bound component", id.0),
                    };
                }
            };

            // ADR-0022 drain-on-swap invariant, preserved under
            // ADR-0038 by the channel splice.
            let (mut old_component, new_rx) = splice_inbox(&entry);
            old_component.on_replace();
            if let Some(err) = old_component.take_save_error() {
                spawn_dispatcher_on(
                    &entry,
                    old_component,
                    new_rx,
                    Arc::clone(&self.registry),
                    Arc::clone(&self.queue),
                );
                return ReplaceResult::Err { error: err };
            }
            let saved = old_component.take_saved_state();
            old_component.on_drop();

            let ctx = SubstrateCtx::new(
                id,
                Arc::clone(&self.registry),
                Arc::clone(&self.queue),
                Arc::clone(&self.outbound),
                Arc::clone(&self.input_subscribers),
            );
            let mut new_component =
                match Component::instantiate(&self.engine, &self.linker, &module, ctx) {
                    Ok(c) => c,
                    Err(e) => {
                        spawn_dispatcher_on(
                            &entry,
                            old_component,
                            new_rx,
                            Arc::clone(&self.registry),
                            Arc::clone(&self.queue),
                        );
                        return ReplaceResult::Err {
                            error: format!("wasm instantiation failed: {e}"),
                        };
                    }
                };

            // ADR-0016 §4: rehydrate the new instance if the old one
            // produced a bundle.
            if let Some(bundle) = saved
                && let Err(e) = new_component.call_on_rehydrate(&bundle)
            {
                spawn_dispatcher_on(
                    &entry,
                    old_component,
                    new_rx,
                    Arc::clone(&self.registry),
                    Arc::clone(&self.queue),
                );
                return ReplaceResult::Err {
                    error: format!("on_rehydrate failed: {e}"),
                };
            }

            drop(old_component);
            spawn_dispatcher_on(
                &entry,
                new_component,
                new_rx,
                Arc::clone(&self.registry),
                Arc::clone(&self.queue),
            );

            // Issue #403: re-derive auto-subscriptions from the new
            // manifest. Replace is additive (ADR-0021 §4).
            auto_subscribe_inputs(&self.input_subscribers, &self.registry, id, &capabilities);

            self.announce_kinds();
            ReplaceResult::Ok { capabilities }
        }

        fn insert_component(&self, id: MailboxId, component: Component) {
            let entry = ComponentEntry::spawn(
                component,
                Arc::clone(&self.registry),
                Arc::clone(&self.queue),
                id,
            );
            self.components.write().unwrap().insert(id, Arc::new(entry));
        }

        fn announce_kinds(&self) {
            let kinds = self.registry.list_kind_descriptors();
            self.outbound.egress_kinds_changed(kinds);
        }
    }

    /// Wire the freshly-registered mailbox into the subscriber set for
    /// every stream kind the component declares a `#[handler]` for
    /// (ADR-0068, issue #403).
    fn auto_subscribe_inputs(
        input_subscribers: &InputSubscribers,
        registry: &Registry,
        mailbox: MailboxId,
        capabilities: &ComponentCapabilities,
    ) {
        let mut subs = input_subscribers.write().unwrap();
        for handler in &capabilities.handlers {
            if registry
                .kind_descriptor(handler.id)
                .is_some_and(|d| d.is_stream)
            {
                subs.entry(handler.id).or_default().insert(mailbox);
            }
        }
    }

    /// Shared validation for `subscribe_input` / `unsubscribe_input`:
    /// the mailbox id must name a live component.
    fn validate_subscriber_mailbox(registry: &Registry, id: MailboxId) -> Result<(), String> {
        match registry.entry(id) {
            Some(MailboxEntry::Component) => Ok(()),
            Some(MailboxEntry::Sink(_)) => {
                Err(format!("mailbox {:?} is a sink, not a component", id))
            }
            Some(MailboxEntry::Dropped) => Err(format!("mailbox {:?} already dropped", id)),
            None => Err(format!("unknown mailbox id {:?}", id)),
        }
    }

    const STATE_LIVE: u8 = 0;
    const STATE_DEAD: u8 = 1;

    /// Per-entry quiescence counter + condvar, shared with the
    /// dispatcher thread.
    #[derive(Default)]
    struct PendingGate {
        pending: AtomicU32,
        lock: Mutex<()>,
        cv: Condvar,
        death: Mutex<Option<DrainDeath>>,
    }

    /// Per-mailbox scheduler state. The `Component` lives on the
    /// dispatcher thread's stack; this side only sees the `Sender` (for
    /// forwarding mail) and the `JoinHandle` (for recovering the
    /// `Component` on teardown).
    pub(super) struct ComponentEntry {
        sender: Mutex<Option<Sender<Mail>>>,
        handle: Mutex<Option<JoinHandle<Component>>>,
        gate: Arc<PendingGate>,
        mailbox: MailboxId,
        state: Arc<AtomicU8>,
    }

    impl ComponentEntry {
        pub(super) fn spawn(
            mut component: Component,
            registry: Arc<Registry>,
            mailer: Arc<Mailer>,
            mailbox: MailboxId,
        ) -> Self {
            let (tx, rx) = mpsc::channel();
            component.install_inbox_rx(rx);
            let gate: Arc<PendingGate> = Arc::new(PendingGate::default());
            let state: Arc<AtomicU8> = Arc::new(AtomicU8::new(STATE_LIVE));
            let gate_for_thread = Arc::clone(&gate);
            let state_for_thread = Arc::clone(&state);
            let thread_name = dispatcher_thread_name(&registry, mailbox);
            let handle = thread::Builder::new()
                .name(thread_name)
                .spawn(move || {
                    dispatcher_loop(
                        component,
                        registry,
                        mailer,
                        gate_for_thread,
                        state_for_thread,
                        mailbox,
                    )
                })
                .expect("spawn component dispatcher");
            Self {
                sender: Mutex::new(Some(tx)),
                handle: Mutex::new(Some(handle)),
                gate,
                mailbox,
                state,
            }
        }

        pub(super) fn is_dead(&self) -> bool {
            self.state.load(Ordering::Acquire) == STATE_DEAD
        }

        pub(super) fn send(&self, mail: Mail) -> bool {
            if self.is_dead() {
                return false;
            }
            let guard = self.sender.lock().unwrap();
            let Some(tx) = guard.as_ref() else {
                return false;
            };
            self.gate.pending.fetch_add(1, Ordering::AcqRel);
            if tx.send(mail).is_ok() {
                true
            } else {
                decrement_and_notify(&self.gate);
                false
            }
        }

        pub(super) fn drain_with_budget(&self, budget: Duration) -> DrainOutcome {
            let deadline = Instant::now() + budget;
            let mut guard = self.gate.lock.lock().unwrap();
            while self.gate.pending.load(Ordering::Acquire) > 0 {
                let now = Instant::now();
                if now >= deadline {
                    return DrainOutcome::Wedged { waited: budget };
                }
                let remaining = deadline - now;
                let (next, timeout) = self.gate.cv.wait_timeout(guard, remaining).unwrap();
                guard = next;
                if timeout.timed_out() && self.gate.pending.load(Ordering::Acquire) > 0 {
                    return DrainOutcome::Wedged { waited: budget };
                }
            }
            if let Some(d) = self.gate.death.lock().unwrap().clone() {
                DrainOutcome::Died(d)
            } else {
                DrainOutcome::Quiesced
            }
        }
    }

    fn decrement_and_notify(gate: &PendingGate) {
        if gate.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _g = gate.lock.lock().unwrap();
            gate.cv.notify_all();
        }
    }

    /// Close the inbox on `entry` and block until the dispatcher
    /// thread returns the `Component`. Caller must hold the last
    /// external strong reference.
    pub(super) fn close_and_join(entry: Arc<ComponentEntry>) -> Component {
        let _ = entry
            .sender
            .lock()
            .unwrap()
            .take()
            .expect("component sender already taken");
        let handle = entry
            .handle
            .lock()
            .unwrap()
            .take()
            .expect("component dispatcher already joined");
        drop(entry);
        handle.join().expect("component dispatcher panicked")
    }

    /// Best-effort variant of `close_and_join` used by `Drop`. Returns
    /// `None` if the entry was already in flight (sender / handle slot
    /// taken by a concurrent `handle_drop` or `splice_inbox`); the
    /// caller skips the `on_drop` hook in that case because the
    /// in-flight handler will run it.
    fn try_close_and_join(entry: Arc<ComponentEntry>) -> Option<Component> {
        let sender = entry.sender.lock().unwrap().take();
        let handle = entry.handle.lock().unwrap().take();
        drop(entry);
        let _ = sender;
        let handle = handle?;
        handle.join().ok()
    }

    /// Splice a new inbox onto `entry` (ADR-0022 + ADR-0038 replace
    /// invariant).
    pub(super) fn splice_inbox(entry: &Arc<ComponentEntry>) -> (Component, Receiver<Mail>) {
        let (new_tx, new_rx) = mpsc::channel();
        let old_tx = entry
            .sender
            .lock()
            .unwrap()
            .replace(new_tx)
            .expect("component sender already taken");
        drop(old_tx);
        let old_handle = entry
            .handle
            .lock()
            .unwrap()
            .take()
            .expect("component dispatcher already joined");
        let old_component = old_handle.join().expect("component dispatcher panicked");
        (old_component, new_rx)
    }

    /// Spawn a fresh dispatcher onto `entry`'s current inbox.
    pub(super) fn spawn_dispatcher_on(
        entry: &Arc<ComponentEntry>,
        mut component: Component,
        rx: Receiver<Mail>,
        registry: Arc<Registry>,
        mailer: Arc<Mailer>,
    ) {
        component.install_inbox_rx(rx);
        let gate = Arc::clone(&entry.gate);
        let state = Arc::clone(&entry.state);
        state.store(STATE_LIVE, Ordering::Release);
        *gate.death.lock().unwrap() = None;
        let mailbox = entry.mailbox;
        let thread_name = dispatcher_thread_name(&registry, mailbox);
        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || dispatcher_loop(component, registry, mailer, gate, state, mailbox))
            .expect("spawn component dispatcher");
        let prev = entry.handle.lock().unwrap().replace(handle);
        debug_assert!(prev.is_none(), "entry handle slot must be empty");
    }

    /// Format: `aether-component-{name}-{mailbox_short}` where
    /// `mailbox_short` is the low 16 hex digits of the 64-bit id (issue
    /// #321 — panic-hook events name the failing mailbox via thread
    /// name).
    fn dispatcher_thread_name(registry: &Registry, mailbox: MailboxId) -> String {
        let name = registry
            .mailbox_name(mailbox)
            .unwrap_or_else(|| "?".to_string());
        format!("aether-component-{}-{:016x}", name, mailbox.0)
    }

    fn dispatcher_loop(
        mut component: Component,
        registry: Arc<Registry>,
        mailer: Arc<Mailer>,
        gate: Arc<PendingGate>,
        state: Arc<AtomicU8>,
        mailbox: MailboxId,
    ) -> Component {
        while let Some(mail) = component.next_mail() {
            let kind_name = registry
                .kind_name(mail.kind)
                .unwrap_or_else(|| format!("kind#{:#x}", mail.kind.0));

            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let span = tracing::info_span!(
                    "dispatch",
                    mailbox = %mailbox,
                    kind = %kind_name,
                );
                let _enter = span.enter();
                component.deliver(&mail)
            }));

            match outcome {
                Ok(Ok(rc)) => {
                    if rc == DISPATCH_UNKNOWN_KIND {
                        tracing::warn!(
                            target: "aether_capabilities::control",
                            mailbox = %mail.recipient,
                            kind = %kind_name,
                            "component has no handler for mail kind (ADR-0033 strict receiver); dropped",
                        );
                    }
                    decrement_and_notify(&gate);
                }
                Ok(Err(trap)) => {
                    tracing::error!(
                        target: "aether_capabilities::control",
                        mailbox = %mail.recipient,
                        kind = %kind_name,
                        error = %trap,
                        "component deliver returned Err (wasmtime trap); marking mailbox dead",
                    );
                    kill_actor(
                        &state,
                        &gate,
                        &mailer,
                        &registry,
                        mailbox,
                        &kind_name,
                        format!("wasmtime trap: {trap}"),
                    );
                    decrement_and_notify(&gate);
                    return component;
                }
                Err(payload) => {
                    let payload_msg = panic_payload_string(&payload);
                    tracing::error!(
                        target: "aether_capabilities::control",
                        mailbox = %mail.recipient,
                        kind = %kind_name,
                        payload = %payload_msg,
                        "host-side panic during deliver; marking mailbox dead",
                    );
                    kill_actor(
                        &state,
                        &gate,
                        &mailer,
                        &registry,
                        mailbox,
                        &kind_name,
                        format!("host panic: {payload_msg}"),
                    );
                    decrement_and_notify(&gate);
                    return component;
                }
            }
        }
        component
    }

    fn kill_actor(
        state: &AtomicU8,
        gate: &PendingGate,
        mailer: &Mailer,
        registry: &Registry,
        mailbox: MailboxId,
        last_kind: &str,
        reason: String,
    ) {
        let mailbox_name = registry
            .mailbox_name(mailbox)
            .unwrap_or_else(|| "?".to_string());

        {
            let mut slot = gate.death.lock().unwrap();
            *slot = Some(DrainDeath {
                mailbox,
                mailbox_name: mailbox_name.clone(),
                last_kind: last_kind.to_string(),
                reason: reason.clone(),
            });
        }

        state.store(STATE_DEAD, Ordering::Release);

        let died = ComponentDied {
            mailbox_id: mailbox,
            mailbox_name,
            last_kind: last_kind.to_string(),
            reason,
        };
        let payload = match postcard::to_allocvec(&died) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    target: "aether_capabilities::control",
                    error = %e,
                    "failed to encode component_died broadcast; death visible only in logs",
                );
                return;
            }
        };

        let mail = Mail::new(
            aether_kinds::mailboxes::HUB_BROADCAST,
            ComponentDied::ID,
            payload,
            1,
        );
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| mailer.push(mail)));
        if result.is_err() {
            tracing::error!(
                target: "aether_capabilities::control",
                "panic while emitting component_died broadcast; death visible only in logs",
            );
        }
    }

    fn panic_payload_string(payload: &Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = payload.downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            format!("<non-string panic payload type_id={:?}>", payload.type_id())
        }
    }
}
