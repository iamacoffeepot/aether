//! `WasmTrampoline` — a `NativeActor` that delegates to a wasm
//! `Component`. Each loaded wasm component is one trampoline instance
//! addressed at `aether.component.trampoline:NAME` (issue 634 Phase 4
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
    use aether_actor::actor::ctx::OutboundReply;
    use aether_kinds::{
        ComponentCapabilities, DropComponent, DropResult, ReplaceComponent, ReplaceResult,
    };
    use wasmtime::{Engine, Linker, Module};

    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::actor::wasm::component::{Component, ComponentCtx};
    use aether_substrate::actor::wasm::kind_manifest;
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
    }

    #[actor]
    impl NativeActor for WasmTrampoline {
        type Config = WasmTrampolineConfig;
        /// Sole declaration of the trampoline namespace (issue 654).
        /// Wasm-component senders read this via
        /// `WasmTrampoline::NAMESPACE` (reachable on every target
        /// because the bridge stub emits the always-on `Actor` impl
        /// at file root).
        const NAMESPACE: &'static str = "aether.component.trampoline";

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
            // ADR-0090 (issue 1256): config bytes are not yet threaded
            // from the load mail; c2 of the rollout wires them. For
            // now pass `&[]` — guests whose `Config = ()` decode this
            // uniformly via `impl Kind for ()`.
            let component = Component::instantiate(
                &config.engine,
                &config.linker,
                &config.module,
                substrate_ctx,
                &[],
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

        /// Drop the **wasm component**. Runs the guest's `on_drop`
        /// hook, then drops the `Component`. The trampoline itself
        /// stays alive — the mailbox `aether.component.trampoline:NAME`
        /// remains addressable and reusable: agents can refill it via
        /// `ReplaceComponent` without minting a new name. To kill
        /// the trampoline (tombstone the subname), terminate the
        /// substrate.
        ///
        /// Mail arriving in the dropped state falls through to
        /// [`Self::forward_to_wasm`], which warn-drops because
        /// `self.component` is `None`.
        #[handler]
        fn on_drop_component(&mut self, ctx: &mut NativeCtx<'_>, _payload: DropComponent) {
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
            ctx.reply(&DropResult::Ok);
        }

        /// Replace the wasm component with a fresh module. ADR-0022 +
        /// ADR-0038 splice invariants hold because the trampoline's
        /// inbox is the framework binding, which outlives the
        /// `Component` swap. `on_replace` runs on the old instance,
        /// `take_saved_state` lifts any rehydration bundle, the new
        /// module instantiates against the same binding, and
        /// `on_rehydrate` runs on the fresh side.
        #[handler]
        fn on_replace_component(&mut self, ctx: &mut NativeCtx<'_>, payload: ReplaceComponent) {
            let result = self.handle_replace(payload);
            ctx.reply(&result);
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
            let mail = Mail::new(
                self.mailbox,
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
            true
        }
    }

    impl WasmTrampoline {
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

            // ADR-0033: parse capabilities from the new wasm so the
            // reply carries the post-replace handler vocabulary.
            let capabilities = match kind_manifest::read_inputs_from_bytes(&payload.wasm) {
                Ok(c) => c,
                Err(error) => return ReplaceResult::Err { error },
            };

            // Run unwire then on_replace on the old instance and lift
            // any saved-state bundle. If the trampoline is currently
            // empty (post-DropComponent — load-after-drop refill),
            // there's no prior wasm to drain; the new instance starts
            // from scratch. Issue 584 Phase 2b: unwire fires first so
            // the old instance can announce its retirement before the
            // swap.
            let saved = if let Some(mut old) = self.component.take() {
                old.unwire();
                old.on_replace();
                if let Some(err) = old.take_save_error() {
                    // Restore the old component so the trampoline isn't
                    // accidentally emptied by a save-state failure.
                    self.component = Some(old);
                    return ReplaceResult::Err { error: err };
                }
                let saved = old.take_saved_state();
                // Old component drops at end of scope — its `on_drop`
                // hook runs as part of `Drop`.
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

            // ADR-0090 (issue 1256): replace does not yet thread config
            // bytes through (c2). Pass `&[]` so `Config = ()` guests
            // decode cleanly; typed-config replaces will surface once
            // the replace ABI carries config alongside the wasm bytes.
            let mut new_component =
                match Component::instantiate(&self.engine, &self.linker, &module, substrate_ctx, &[])
                {
                    Ok(c) => c,
                    Err(e) => {
                        return ReplaceResult::Err {
                            error: format!("wasm instantiation failed: {e}"),
                        };
                    }
                };

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
}
