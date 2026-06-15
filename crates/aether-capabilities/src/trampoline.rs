//! `WasmTrampoline` — a `NativeActor` that delegates to a wasm
//! `Component`. Each loaded wasm component is one trampoline instance
//! addressed at `aether.embedded:NAME` (issue 634 Phase 4
//! PR 1).
//!
//! ## Where this lives (issue 654)
//!
//! Pre-this-PR the trampoline actor lived at
//! `aether_substrate::actor::wasm::trampoline`, leaving the namespace
//! constant split across two crates (substrate for the actor; cap-side
//! mirror for wasm-component senders). The trampoline now sits next to
//! [`crate::component::ComponentHostCapability`] — its only consumer —
//! and the namespace is whatever `WasmTrampoline::NAMESPACE` says it
//! is. Single declaration, cap-owned, reachable on every target via
//! the `Actor` trait const.
//!
//! The native impl still uses substrate internals (`Engine`, `Linker`,
//! `Registry`, `Mailer`, `HubOutbound`) — that's exactly what
//! `aether-capabilities`'s `native` feature pulls in, same as the rest
//! of `ComponentHostCapability`'s implementation. The wasm side gets a
//! ZST stub from the bridge macro so wasm-component senders that
//! address loaded peers via `ctx.actor::<ComponentHostCapability>().loaded(...)`
//! can resolve through `WasmTrampoline::NAMESPACE` without depending
//! on `aether-substrate`.
//!
//! ## Shape
//!
//! Plain instanced `NativeActor`. Anything it doesn't handle natively
//! (today: `DropComponent`, `ReplaceComponent`) falls through
//! `#[fallback]` to the wasm guest via `Component::deliver`. The
//! framework dispatcher reads from the trampoline's `NativeBinding`;
//! un-handled kinds reach `forward_to_wasm`; the guest's
//! `send_mail_p32` / `reply_mail_p32` host fns
//! route through the same binding.
//!
//! ## Handler-kind imports
//!
//! `DropComponent` and `ReplaceComponent` are re-imported at file
//! root because the `#[bridge]` macro lifts `HandlesKind<K>` marker
//! impls outside the cfg-gated `mod native` block (always-on, so the
//! type-system sees the markers from wasm too). The native impl
//! re-imports them inside its own use list so the handler signatures
//! resolve there too.
//!
//! ## Lifecycle
//!
//! - **Load**: `crate::component::ComponentHostCapability::on_load_component`
//!   spawns a trampoline via the runtime spawn machinery (subname = the
//!   agent-supplied component name); the spawn path runs
//!   `WasmTrampoline::init` which instantiates the wasm `Component`
//!   against the trampoline's binding.
//! - **Drop**: `DropComponent` mail addressed to the trampoline's
//!   mailbox lands on `Self::on_drop_component`, which calls
//!   `ctx.shutdown()`. The framework drains the inbox, runs `unwire`,
//!   and the dispatcher exits.
//! - **Replace**: `ReplaceComponent` mail lands on
//!   `Self::on_replace_component`, which instantiates a new
//!   `Component` against the same binding and swaps `self.component`.
//!   ADR-0022 + ADR-0038 invariants hold because the inbox channel is
//!   the trampoline's `NativeBinding` and outlives the swap.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root so the
// `#[bridge]`-emitted `impl HandlesKind<K> for WasmTrampoline {}`
// markers (always-on, outside the cfg gate) resolve.
use aether_kinds::{DropComponent, ReplaceComponent};

#[cfg(not(target_arch = "wasm32"))]
pub use native::WasmTrampolineConfig;

#[aether_actor::bridge(instanced, one_per = "component")]
mod native {
    use std::sync::Arc;

    use aether_actor::actor;
    use aether_kinds::{
        ComponentCapabilities, DropComponent, DropResult, ReplaceComponent, ReplaceResult,
    };
    use wasmtime::{Engine, Linker, Module};

    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::actor::native::spawn::Subname;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::actor::wasm::component::{Component, ComponentCtx, PendingSpawn};
    use aether_substrate::actor::wasm::kind_manifest;
    use aether_substrate::actor::wasm::kind_manifest::ActorInputs;
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::capability::MailboxCaps;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;
    use aether_substrate::mail::{KindId, Mail, MailboxId};
    use std::io;

    use aether_actor::Local as _;
    use aether_actor::cost::CostCells;

    /// Configuration handed to [`WasmTrampoline::init`] by the spawn
    /// path. Carries the wasmtime engine / linker plus the parsed
    /// module bytes; `init` instantiates the `Component` against the
    /// trampoline's binding.
    pub struct WasmTrampolineConfig {
        pub engine: Arc<Engine>,
        pub linker: Arc<Linker<ComponentCtx>>,
        pub module: Module,
        pub registry: Arc<Registry>,
        pub outbound: Arc<HubOutbound>,
        /// Component capabilities parsed from the wasm's
        /// `aether.kinds.inputs` custom section, surfaced through
        /// `LoadResult::Ok.capabilities` at the cap. The trampoline
        /// keeps a handle so it can rehydrate after a replace.
        pub capabilities: ComponentCapabilities,
        /// ADR-0090 (issue 1257): init-config bytes from the
        /// `aether.component.load` mail, handed to the guest's typed
        /// `FfiActor::init` via `Component::instantiate`. Empty means
        /// "no config" — a `Config = ()` guest decodes `&[]` uniformly.
        pub config: Vec<u8>,
        /// ADR-0096: the selected export's actor-type tag
        /// (`mailbox_id_from_name(NAMESPACE)`), threaded through to
        /// `Component::instantiate` so it calls `init_typed_p32`.
        /// `None` instantiates the module's entry type via the legacy
        /// `init_with_config_p32` path — the only type a single-actor
        /// module has. Stored on the trampoline so a later
        /// `ReplaceComponent` rebuilds the same export.
        pub type_tag: Option<u64>,
        /// ADR-0097: every exported type's capability group, parsed once
        /// at load. The trampoline keeps it so a `spawn_child::<Sibling>`
        /// host-fn request can register the spawned sibling's *own*
        /// handler set (looked up by actor-type tag), and so each
        /// spawned sibling carries the same map for its own spawns.
        pub actor_caps: Vec<ActorInputs>,
    }

    /// Per-component trampoline. Holds the wasm `Component`
    /// optionally — `None` means the wasm has been unloaded by
    /// `DropComponent` but the trampoline (and its mailbox name) is
    /// still alive, ready to be refilled by `ReplaceComponent` or
    /// recycled by a future load. Distinction matters: dropping the
    /// **component** is a wasm unload that preserves the addressable
    /// name; dropping the **trampoline** would kill the actor and
    /// tombstone the subname. The cap's `DropComponent` handler does
    /// the former; the latter happens at substrate teardown.
    pub struct WasmTrampoline {
        /// `Some` while wasm is loaded; `None` after a `DropComponent`.
        /// Mail arriving in the `None` state warn-drops via the
        /// fallback (the trampoline is just an empty named slot).
        component: Option<Component>,
        /// Held for [`Self::on_replace_component`] so a fresh
        /// `Component::instantiate` against the same engine + linker
        /// is reachable from the handler.
        engine: Arc<Engine>,
        linker: Arc<Linker<ComponentCtx>>,
        registry: Arc<Registry>,
        mailer: Arc<Mailer>,
        outbound: Arc<HubOutbound>,
        /// The trampoline's own mailbox id
        /// (== `MailboxId::from_name(full_name)`). Cached because
        /// `NativeCtx` only exposes `self_id()` via the
        /// `NativeInitCtx` flavour today; storing it here avoids
        /// reaching into `ctx.binding().self_mailbox()` on every
        /// handler call.
        mailbox: MailboxId,
        /// ADR-0096: the selected export's actor-type tag, or `None`
        /// for the entry type. Held so [`Self::handle_replace`]
        /// re-instantiates the same exported type from the new wasm
        /// and re-reads that type's capability group.
        type_tag: Option<u64>,
        /// ADR-0097: the resident `Module`, retained so a sibling spawn
        /// re-instantiates it (a cheap `Arc` clone — wasmtime shares the
        /// compiled code) without a re-compile, and refreshed on replace.
        module: Module,
        /// ADR-0097: every exported type's capability group (see
        /// [`WasmTrampolineConfig::actor_caps`]). A spawned sibling looks
        /// up its own handler set here by actor-type tag.
        actor_caps: Vec<ActorInputs>,
    }

    #[actor]
    impl NativeActor for WasmTrampoline {
        type Config = WasmTrampolineConfig;
        /// The embedding-host class namespace (ADR-0099 §5/§6),
        /// **forward-fed** from [`EmbeddedHost`](aether_actor::EmbeddedHost)
        /// — the sole owner of the `"aether.embedded"` literal. The
        /// trampoline references the class const rather than re-declaring
        /// the name, so an embeddable actor's id depends on what the code
        /// is, not how it is hosted, and the namespace is written only on
        /// its owner. Reachable on every target because the bridge stub
        /// emits the always-on `Actor` impl at file root. ADR-0097: the
        /// substrate's `TRAMPOLINE_NAMESPACE` forward-feeds the same const,
        /// collapsing the former two-literal mirror into one source; the
        /// `trampoline_namespace_matches_substrate` test guards the match.
        const NAMESPACE: &'static str =
            <aether_actor::EmbeddedHost as aether_actor::Actor>::NAMESPACE;

        fn init(
            config: WasmTrampolineConfig,
            ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            let mailbox = ctx.self_id();
            let mailer = ctx.mailer();
            let mut substrate_ctx = ComponentCtx::new(
                mailbox,
                Arc::clone(&config.registry),
                Arc::clone(&mailer),
                Arc::clone(&config.outbound),
            );
            // Wire the trampoline's binding so the guest's reply /
            // outbound-mail host fns route through *this* trampoline's
            // binding (issue 634 Phase 4 PR 3 — single source of inbox
            // truth lives on `NativeBinding`, not on `ComponentCtx`).
            substrate_ctx.install_binding(Arc::clone(ctx.binding()));
            // ADR-0090 (issue 1257): thread the load mail's config bytes
            // into the guest's typed `init`. An empty slice ("no config")
            // is decoded uniformly by a `Config = ()` guest via
            // `impl Kind for ()`; a typed-config guest decodes its
            // `Self::Config` from these bytes.
            let component = Component::instantiate(
                &config.engine,
                &config.linker,
                &config.module,
                substrate_ctx,
                &config.config,
                config.type_tag,
            )
            .map_err(|e| {
                BootError::Other(io::Error::other(format!("wasm instantiation failed: {e}")).into())
            })?;

            // iamacoffeepot/aether#1128: seed this component's per-handler
            // cost cells from the guest's declared handler set
            // (`config.capabilities`, parsed from the wasm's
            // `aether.kinds.inputs` section). `init` runs inside the spawn
            // path's `with_stamped(&slots, …)`, so the per-actor
            // `CostCells` cache is stamped directly here — the cap's
            // thread vs the trampoline's is irrelevant: the stamp binds to
            // the actor's `ActorSlots`, not to a thread. The same
            // `Arc<CostCell>`s seed the global `CostTable` for the cold
            // `cost.tail` dump and the iamacoffeepot/aether#1178
            // producer-side read. Replace re-seeds on the trampoline's own
            // dispatch (`on_replace_component`); drop clears both indexes.
            let handler_kinds: Vec<KindId> =
                config.capabilities.handlers.iter().map(|h| h.id).collect();
            let seeded = mailer.cost_table().seed(mailbox, &handler_kinds);
            CostCells::try_with_mut(|cells| cells.seed(seeded));

            Ok(Self {
                component: Some(component),
                engine: config.engine,
                linker: config.linker,
                registry: config.registry,
                mailer,
                outbound: config.outbound,
                mailbox,
                type_tag: config.type_tag,
                module: config.module,
                actor_caps: config.actor_caps,
            })
        }

        /// Issue 640 Phase 2: fire the wasm guest's `wire` hook
        /// post-registration. The cap-side spawn flow registers the
        /// trampoline mailbox in step 5–7; this hook runs after that
        /// as part of the dispatcher's lifecycle, so a wire-time
        /// `subscribe_input` mail validates against a live closure
        /// entry. Pre-issue-640 the call lived inside
        /// `Component::instantiate` (step 4, before registration) and
        /// races the input cap's `validate_subscriber_mailbox`,
        /// silently dropping subscribes.
        fn wire(&mut self, _ctx: &mut NativeCtx<'_>) {
            if let Some(component) = self.component.as_mut()
                && let Err(e) = component.wire()
            {
                tracing::error!(
                    target: "aether_capabilities::trampoline",
                    error = %e,
                    "wasm guest `wire` hook returned error",
                );
            }
        }

        /// Drop the **wasm component**. Runs the guest's `unwire`
        /// pre-shutdown hook, then drops the `Component`. The trampoline itself
        /// stays alive — the mailbox `aether.embedded:NAME`
        /// remains addressable and reusable: agents can refill it via
        /// `ReplaceComponent` without minting a new name. To kill
        /// the trampoline (tombstone the subname), terminate the
        /// substrate.
        ///
        /// Mail arriving in the dropped state falls through to
        /// [`Self::forward_to_wasm`], which warn-drops because
        /// `self.component` is `None`.
        #[handler]
        fn on_drop_component(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            _payload: DropComponent,
        ) -> DropResult {
            if let Some(mut component) = self.component.take() {
                // Issue 584 Phase 3 (ADR-0079 amended): unwire is the
                // single pre-shutdown hook — the legacy `on_drop`
                // retired alongside `FfiActor::on_drop`. Component
                // drops at end of scope, tearing down linear memory.
                component.unwire();
            }
            // iamacoffeepot/aether#1037: clear the trampoline's
            // capabilities — the wasm is unloaded, so the mailbox now
            // accepts nothing until a `replace` refills it. The
            // trampoline (and its mailbox name) survives as an empty
            // slot, but it has no accept-set while empty.
            self.mailer.capability_registry().remove(self.mailbox);
            // iamacoffeepot/aether#1128: drop this mailbox's per-handler
            // cost cells from the global table and the per-actor cache.
            // `on_drop_component` runs on the trampoline's own thread
            // inside `with_stamped`, so both indexes clear together.
            self.mailer.cost_table().drop_mailbox(self.mailbox);
            CostCells::try_with_mut(|cells| cells.seed(Vec::new()));
            DropResult::Ok
        }

        /// Replace the wasm component with a fresh module. ADR-0022 +
        /// ADR-0038 splice invariants hold because the trampoline's
        /// inbox is the framework binding, which outlives the
        /// `Component` swap. `on_dehydrate` runs on the old instance,
        /// `take_saved_state` lifts any rehydration bundle, the new
        /// module instantiates against the same binding, and
        /// `on_rehydrate` runs on the fresh side.
        #[handler]
        fn on_replace_component(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            payload: ReplaceComponent,
        ) -> ReplaceResult {
            self.handle_replace(payload)
        }

        /// Forward un-handled mail to the wasm guest.
        ///
        /// The framework dispatcher pulled this envelope from the
        /// trampoline's binding, dispatched against typed handlers
        /// (none matched), and called this fallback. We synthesise a
        /// `Mail` with the trampoline's own id as recipient, hand it
        /// to `Component::deliver`, and let the guest's `receive_p32`
        /// dispatch shim do the rest.
        #[fallback]
        fn forward_to_wasm(&mut self, ctx: &mut NativeCtx<'_>, env: &Envelope) -> bool {
            // ADR-0097: deliver the inbound, then drain any sibling spawn
            // the guest staged during `deliver`. The block scopes the
            // `&mut component` borrow so `spawn_sibling` can read the
            // trampoline's other fields afterward.
            let pending = {
                let Some(component) = self.component.as_mut() else {
                    tracing::warn!(
                        target: "aether_capabilities::trampoline",
                        mailbox = %self.mailbox,
                        kind = %env.kind_name,
                        "mail to trampoline with no wasm loaded (post-drop); discarded — re-load via aether.component.replace",
                    );
                    return true;
                };
                // Issue iamacoffeepot/aether#722: carry the inbound's
                // lineage through to the synthetic `Mail`.
                // `Component::deliver` reads `mail.mail_id` and `mail.root`
                // to populate `ComponentCtx`'s in-flight cells, so any
                // guest-triggered `send_mail_p32` / `reply_mail_p32` stamps
                // `parent_mail = Some(env.mail_id)` and inherits the chain
                // `root`. Without this, the trampoline's wrapped Mail
                // defaults to `MailId::NONE` and the guest's outbound looks
                // like a fresh root.
                // ADR-0114 §2: deliver the *routed* recipient as the guest
                // `Mail`'s recipient, not the trampoline's own id. For a
                // normally-addressed actor `env.recipient` equals
                // `self.mailbox`, so this is a no-op; for an inline-child
                // alias it carries the child's address, which
                // `Component::deliver` threads to the guest's `receive`
                // frame + the `ComponentCtx` dispatch identity so the
                // membrane demuxes to the child and the child's sends stamp
                // its address as origin.
                let mail = Mail::new(
                    env.recipient,
                    env.kind,
                    env.payload.bytes().to_vec(),
                    env.count,
                )
                .with_reply_to(env.sender)
                .with_lineage(env.mail_id, env.root, env.parent_mail);
                if let Err(e) = component.deliver(&mail) {
                    // ADR-0063 fail-fast: a wasm trap (or host-fn error
                    // returned through `Component::deliver`) kills the
                    // substrate. Wedge detection (CPU-loop guests) waits
                    // on a future epoch-deadline ADR — symmetric with
                    // native actors, which have no wedge guard either
                    // today.
                    ctx.fatal_abort(format!(
                        "component {} (kind {}) trapped: {e}",
                        self.mailbox, env.kind_name,
                    ));
                }
                component.take_pending_spawn()
            };
            if let Some(pending) = pending {
                self.spawn_sibling(ctx, pending);
            }
            true
        }
    }

    impl WasmTrampoline {
        /// ADR-0097: perform the sibling spawn the guest staged via the
        /// `spawn_sibling` host fn during `Component::deliver`. The
        /// trampoline runs the typed `spawn_child::<WasmTrampoline>` the
        /// substrate host fn couldn't (it can't name this type), reusing
        /// the resident `Module` and registering the spawned sibling's
        /// own capability group (looked up by actor-type tag). A
        /// spawn-time failure surfaces here, asynchronously to the guest
        /// (which already received the `MailboxId`): logged, not fatal.
        fn spawn_sibling(&self, ctx: &mut NativeCtx<'_>, pending: PendingSpawn) {
            let capabilities =
                self.actor_caps
                    .iter()
                    .find(|actor| {
                        // Runtime-name match: hash each loaded actor's declared
                        // namespace (from module metadata) to find the one whose
                        // tag the spawn requested — not a hardcoded sibling.
                        #[allow(clippy::disallowed_methods)]
                        actor.namespace.as_deref().is_some_and(|ns| {
                            aether_data::mailbox_id_from_name(ns).0 == pending.tag
                        })
                    })
                    .map(|actor| actor.capabilities.clone())
                    .unwrap_or_default();
            let config = WasmTrampolineConfig {
                engine: Arc::clone(&self.engine),
                linker: Arc::clone(&self.linker),
                module: self.module.clone(),
                registry: Arc::clone(&self.registry),
                outbound: Arc::clone(&self.outbound),
                capabilities,
                config: pending.config,
                type_tag: Some(pending.tag),
                actor_caps: self.actor_caps.clone(),
            };
            if let Err(e) = ctx
                .spawn_child::<Self>(Subname::Named(&pending.subname), config)
                .finish()
            {
                tracing::warn!(
                    target: "aether_capabilities::trampoline",
                    parent = %self.mailbox,
                    subname = %pending.subname,
                    "sibling spawn failed: {e:?}",
                );
            }
        }

        fn handle_replace(&mut self, payload: ReplaceComponent) -> ReplaceResult {
            // `payload.wasm` is the new module bytes; `mailbox_id` is
            // the trampoline's own id (the agent already addressed
            // this mail to us, so the field is informational).
            let _ = payload.mailbox_id;

            let module = match Module::new(&self.engine, &payload.wasm) {
                Ok(m) => m,
                Err(e) => {
                    return ReplaceResult::Err {
                        error: format!("invalid wasm module: {e}"),
                    };
                }
            };

            // ADR-0033 / ADR-0096 / ADR-0097: parse every exported type's
            // capability group from the new wasm. `capabilities` is the
            // group for the type THIS trampoline hosts (entry when
            // `type_tag` is None), so the reply carries the post-replace
            // handler vocabulary; the full `actors` set refreshes
            // `self.actor_caps` below so post-replace sibling spawns see
            // the new module's types. A tag absent from the new module is
            // an `Err` — the replacement doesn't export the loaded type.
            let actors = match kind_manifest::read_actor_inputs_from_bytes(&payload.wasm) {
                Ok(a) => a,
                Err(error) => return ReplaceResult::Err { error },
            };
            let capabilities = match self.type_tag {
                None => actors
                    .first()
                    .map(|a| a.capabilities.clone())
                    .unwrap_or_default(),
                Some(tag) => match actors.iter().find(|a| {
                    // Runtime-name match: hash each replacement actor's declared
                    // namespace to find the one whose tag was loaded — not a
                    // hardcoded sibling.
                    #[allow(clippy::disallowed_methods)]
                    a.namespace
                        .as_deref()
                        .is_some_and(|ns| aether_data::mailbox_id_from_name(ns).0 == tag)
                }) {
                    Some(group) => group.capabilities.clone(),
                    None => {
                        return ReplaceResult::Err {
                            error: format!(
                                "replace: new module does not export the actor type (tag {tag:#x}) this trampoline loaded"
                            ),
                        };
                    }
                },
            };

            // Run unwire then on_dehydrate on the old instance and lift
            // any saved-state bundle. If the trampoline is currently
            // empty (post-DropComponent — load-after-drop refill),
            // there's no prior wasm to drain; the new instance starts
            // from scratch. Issue 584 Phase 2b: unwire fires first so
            // the old instance can announce its retirement before the
            // swap.
            let saved = if let Some(mut old) = self.component.take() {
                old.unwire();
                old.on_dehydrate();
                if let Some(err) = old.take_save_error() {
                    // Restore the old component so the trampoline isn't
                    // accidentally emptied by a save-state failure.
                    self.component = Some(old);
                    return ReplaceResult::Err { error: err };
                }
                let saved = old.take_saved_state();
                // Old component drops at end of scope — the `Component`'s
                // own `Drop` releases the wasm store.
                drop(old);
                saved
            } else {
                None
            };

            // Build a fresh `ComponentCtx` for the new instance — same
            // mailer + registry/outbound/input references, new
            // ReplyTable since wasm-side state resets. Mailbox id is
            // preserved across replace per ADR-0022 §4.
            let substrate_ctx = ComponentCtx::new(
                self.mailbox,
                Arc::clone(&self.registry),
                Arc::clone(&self.mailer),
                Arc::clone(&self.outbound),
            );

            // ADR-0090 (issue 1257): thread the replace mail's config
            // bytes into the new instance's typed `init`, the same way
            // the load path does. Empty means "no config"; a typed-config
            // guest decodes its `Self::Config` from these bytes.
            let mut new_component = match Component::instantiate(
                &self.engine,
                &self.linker,
                &module,
                substrate_ctx,
                &payload.config,
                self.type_tag,
            ) {
                Ok(c) => c,
                Err(e) => {
                    return ReplaceResult::Err {
                        error: format!("wasm instantiation failed: {e}"),
                    };
                }
            };

            // ADR-0097: the new module is now resident — retain it (and
            // the refreshed per-type cap map) so sibling spawns after this
            // replace re-instantiate the new code, not the old.
            self.module = module;
            self.actor_caps = actors;

            // ADR-0016 §4: rehydrate the new instance if the old one
            // produced a bundle. A failed rehydrate still installs the
            // new component (the old one is already gone) and surfaces
            // the error so the agent decides whether to roll forward.
            if let Some(bundle) = saved
                && let Err(e) = new_component.call_on_rehydrate(&bundle)
            {
                self.component = Some(new_component);
                return ReplaceResult::Err {
                    error: format!("on_rehydrate failed: {e}"),
                };
            }

            self.component = Some(new_component);

            // iamacoffeepot/aether#1037: re-register the trampoline's
            // capabilities against the post-replace handler set. The
            // mailbox id is stable across replace (ADR-0022 §4), so
            // `register` overwrites the prior entry — the validator
            // sees the new accept-set immediately.
            self.mailer.capability_registry().register(
                self.mailbox,
                MailboxCaps::from_component_capabilities(&capabilities),
            );

            // iamacoffeepot/aether#1128: re-seed the per-handler cost
            // cells against the post-replace handler set, into BOTH
            // indexes. The mailbox id is stable across replace
            // (ADR-0022 §4), so the global `seed` reuses the prior cell
            // for an unchanged kind (keeping its accumulated EWMA) and
            // adds a neutral cell for a new one. `on_replace_component`
            // runs on the trampoline's own dispatch thread inside
            // `with_stamped`, so we can re-stamp the per-actor `CostCells`
            // cache directly with the freshly-returned `Arc`s — keeping
            // the cache exact across replace (a new kind's cell would
            // otherwise miss until the cache happened to re-pull).
            let handler_kinds: Vec<KindId> = capabilities.handlers.iter().map(|h| h.id).collect();
            let seeded = self.mailer.cost_table().seed(self.mailbox, &handler_kinds);
            CostCells::try_with_mut(|cells| cells.seed(seeded));

            ReplaceResult::Ok { capabilities }
        }
    }

    #[cfg(test)]
    mod tests {
        use aether_actor::{Actor, EmbeddedHost};
        use aether_substrate::actor::wasm::component::TRAMPOLINE_NAMESPACE;

        /// ADR-0099 §5/§6: both `WasmTrampoline::NAMESPACE` (capabilities)
        /// and the substrate's `TRAMPOLINE_NAMESPACE` forward-feed the
        /// embedding-host class const `EmbeddedHost::NAMESPACE` — the sole
        /// owner of the `"aether.embedded"` literal, which sits below both
        /// crates. The former two-literal mirror is now one source; this
        /// guards that the forward-feed stays wired, so an embedded
        /// component registers under and resolves to the same class
        /// namespace on both layers.
        #[test]
        fn trampoline_namespace_matches_substrate() {
            assert_eq!(
                <super::WasmTrampoline as Actor>::NAMESPACE,
                EmbeddedHost::NAMESPACE,
            );
            assert_eq!(TRAMPOLINE_NAMESPACE, EmbeddedHost::NAMESPACE);
            assert_eq!(EmbeddedHost::NAMESPACE, "aether.embedded");
        }
    }
}
