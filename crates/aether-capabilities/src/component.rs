//! `aether.component` cap (issue 603, renamed in issue 638 phase 3
//! from `aether.control`). The wasm-component lifecycle endpoint:
//! receives [`LoadComponent`] mail and spawns a per-component
//! [`WasmTrampoline`] (issue 634 Phase 4 PR 1) addressed at
//! `aether.component.trampoline:NAME`. [`DropComponent`] and
//! [`ReplaceComponent`] mail flow through the cap as well — it
//! forwards each to the addressed trampoline preserving the
//! original `reply_to`, so the trampoline replies directly to the
//! agent. The cap holds no per-component bookkeeping; the
//! trampoline manages its own lifecycle as an instanced [`NativeActor`].
//!
//! Pre-Phase-4 the cap also owned the wasm dispatcher infrastructure
//! (the retired `ComponentEntry`, `dispatcher_loop`, `kill_actor`,
//! `splice_inbox`, etc.) and installed itself as the `Mailer`'s
//! `ComponentRouter` for component-bound routing. All of that
//! retired with the trampoline migration: dispatch lives on the
//! framework's `NativeActor` loop, replace is `Component`-swap
//! inside the trampoline, drop flows through `ctx.shutdown()`.
//!
//! [`NativeActor`]: aether_substrate::NativeActor
//!
//! The cap is a `#[bridge] mod native { ... }` per the ADR-0076 /
//! issue 565 pattern. Plain fields (no `Arc<Inner>` wrapper) per
//! ADR-0078 — the cap is single-threaded, every handler runs on the
//! cap's dispatcher thread.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

use aether_actor::{Actor, FfiActorMailbox};
#[cfg(not(target_arch = "wasm32"))]
use aether_kinds::UnsubscribeAll;
use aether_kinds::{DropComponent, LoadComponent, ReplaceComponent};
#[cfg(not(target_arch = "wasm32"))]
use aether_substrate::actor::native::NativeActorMailbox;

use crate::trampoline::WasmTrampoline;

#[cfg(not(target_arch = "wasm32"))]
pub use native::ComponentHostConfig;

/// Sender-side facade for FFI guests addressing a loaded peer
/// component through [`ComponentHostCapability`].
///
/// "Sending mail to a loaded component" isn't a SDK primitive — it
/// only exists *because* this cap loaded a wasm component and gave it
/// a trampoline address. So the helper lives here, attached to the
/// cap's FFI mailbox, mirroring [`crate::fs::FsMailboxExt`]'s
/// cap-owned facade pattern (issue 580).
///
/// `.loaded::<R>(name)` resolves a typed peer handle. The trampoline
/// prefix lives in exactly one place in the workspace —
/// [`WasmTrampoline::NAMESPACE`] (issue 654) — and this method reads
/// from it, so a future rename of the convention touches one constant
/// and propagates everywhere.
///
/// `R: Actor` is the peer's actor type, supplied by the caller (same
/// as today's `FfiCtx::resolve_actor` surface). Type-checks at the
/// send site — `peer.send::<K>(&mail)` compiles only when
/// `R: HandlesKind<K>`.
pub trait ComponentHostFfiExt {
    /// Resolve a typed peer-component mailbox for the loaded component
    /// named `name`. The full mailbox address is
    /// `format!("{}:{}", WasmTrampoline::NAMESPACE, name)`.
    fn loaded<R: Actor>(&self, name: &str) -> FfiActorMailbox<R>;
}

impl ComponentHostFfiExt for FfiActorMailbox<ComponentHostCapability> {
    fn loaded<R: Actor>(&self, name: &str) -> FfiActorMailbox<R> {
        self.resolve_peer::<R>(&format!("{}:{}", WasmTrampoline::NAMESPACE, name))
    }
}

/// Sender-side facade for native cap-to-cap callers addressing a
/// loaded peer component through [`ComponentHostCapability`]. Same
/// shape as [`ComponentHostFfiExt`] for the native transport — the
/// returned handle inherits the parent mailbox's `'a` binding ref so
/// `.send::<K>(&mail)` dispatches through the same `NativeBinding`
/// without re-threading the ctx.
#[cfg(not(target_arch = "wasm32"))]
pub trait ComponentHostNativeExt {
    /// Resolve a typed peer-component mailbox for the loaded component
    /// named `name`. The full mailbox address is
    /// `format!("{}:{}", WasmTrampoline::NAMESPACE, name)`.
    fn loaded<R: Actor>(&self, name: &str) -> NativeActorMailbox<'_, R>;
}

#[cfg(not(target_arch = "wasm32"))]
impl ComponentHostNativeExt for NativeActorMailbox<'_, ComponentHostCapability> {
    fn loaded<R: Actor>(&self, name: &str) -> NativeActorMailbox<'_, R> {
        self.resolve_peer::<R>(&format!("{}:{}", WasmTrampoline::NAMESPACE, name))
    }
}

#[aether_actor::bridge(singleton)]
mod native {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use aether_actor::Actor;
    use aether_actor::actor;
    use aether_actor::actor::ctx::OutboundReply;
    use aether_data::Kind;
    use aether_kinds::LoadResult;
    use wasmtime::{Engine, Linker, Module};

    use super::{DropComponent, LoadComponent, ReplaceComponent, UnsubscribeAll};

    use aether_substrate::actor::native::spawn::Subname;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::actor::wasm::component::ComponentCtx;
    use aether_substrate::actor::wasm::kind_manifest;
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::capability::MailboxCaps;
    use aether_substrate::mail::helpers::register_or_match_all;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;
    use aether_substrate::mail::{KindId, Mail, MailboxId};

    use crate::input::InputCapability;
    use crate::trampoline::{WasmTrampoline, WasmTrampolineConfig};

    /// Configuration for [`ComponentHostCapability`]. `engine` and
    /// `linker` are the wasmtime instances every load instantiates
    /// against (handed through to the trampoline's
    /// `Component::instantiate` call); `hub_outbound` is the egress
    /// handle the cap uses for `aether.kinds.changed` announcements
    /// after each load. ADR-0021 fan-out is mail-driven post-issue-640
    /// — the cap mails subscribe / unsubscribe to `aether.input`
    /// rather than mutating shared state.
    pub struct ComponentHostConfig {
        pub engine: Arc<Engine>,
        pub linker: Arc<Linker<ComponentCtx>>,
        pub hub_outbound: Arc<HubOutbound>,
    }

    /// `aether.component` cap. Plain-fields shape — single-threaded
    /// owner running on its dispatcher thread; no shared state. Input
    /// subscribe / unsubscribe go through `aether.input` via mail
    /// (post-issue-640) — the cap doesn't carry an `input_mailbox`
    /// field; `ctx.actor::<InputCapability>().send(...)` resolves it
    /// inline at the call site.
    pub struct ComponentHostCapability {
        engine: Arc<Engine>,
        linker: Arc<Linker<ComponentCtx>>,
        registry: Arc<Registry>,
        mailer: Arc<Mailer>,
        outbound: Arc<HubOutbound>,
        /// Monotonic counter for `component_N` default names when an
        /// agent passes `name: None` and the wasm doesn't declare an
        /// `aether.namespace`. `AtomicU64` because the bridge macro
        /// emits handlers behind `&self` for some patterns; the
        /// counter is fine either way.
        default_name_counter: AtomicU64,
    }

    #[actor]
    impl NativeActor for ComponentHostCapability {
        type Config = ComponentHostConfig;
        const NAMESPACE: &'static str = "aether.component";

        fn init(
            config: ComponentHostConfig,
            ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            let mailer = ctx.mailer();
            let registry = Arc::clone(mailer.registry());
            Ok(Self {
                engine: config.engine,
                linker: config.linker,
                registry,
                mailer,
                outbound: config.hub_outbound,
                default_name_counter: AtomicU64::new(0),
            })
        }

        /// Load a fresh wasm component into the substrate.
        ///
        /// # Agent
        /// Pass the wasm bytes plus an optional `name`. On Ok the cap
        /// registers the kinds the wasm declared in its `aether.kinds`
        /// section, picks a final name (caller value > wasm's
        /// `aether.namespace` > `component_N`), spawns a
        /// [`WasmTrampoline`] under `aether.component.trampoline:NAME`,
        /// and replies `LoadResult::Ok { mailbox_id, name,
        /// capabilities }` where `name` is the full trampoline
        /// address — agents send subsequent mail to that name.
        /// Errors (bad postcard, kind conflict, name conflict,
        /// invalid wasm, instantiation trap) come back as
        /// `LoadResult::Err`.
        #[handler]
        fn on_load_component(&mut self, ctx: &mut NativeCtx<'_>, payload: LoadComponent) {
            let result = self.handle_load(ctx, payload);
            ctx.reply(&result);
        }

        /// Drop a component by its mailbox id. Forwards
        /// [`DropComponent`] mail to the addressed trampoline; the
        /// trampoline's [`WasmTrampoline::on_drop_component`] handler
        /// replies `DropResult::Ok` and shuts itself down.
        ///
        /// # Agent
        /// `DropComponent { mailbox_id }`. The `mailbox_id` is the
        /// trampoline's id from the `LoadResult.mailbox_id` field.
        #[handler]
        fn on_drop_component(&mut self, ctx: &mut NativeCtx<'_>, payload: DropComponent) {
            // Cap-side cleanup: ask the input cap to drop the dying
            // trampoline from every fan-out set. Mail rather than
            // direct mutation post-issue-640 — `aether.input` is the
            // sole owner of the subscriber table.
            ctx.actor::<InputCapability>().send(&UnsubscribeAll {
                mailbox: payload.mailbox_id,
            });
            self.forward_to_trampoline(ctx, payload.mailbox_id, DropComponent::ID, &payload);
        }

        /// Replace the component at `mailbox_id` with a fresh wasm
        /// binary. Forwards [`ReplaceComponent`] to the trampoline;
        /// the trampoline's [`WasmTrampoline::on_replace_component`]
        /// handler swaps `Component` internally and replies
        /// `ReplaceResult`. ADR-0022 + ADR-0038 splice invariants
        /// hold because the inbox channel is the trampoline's
        /// `NativeBinding`, which outlives the swap.
        ///
        /// # Agent
        /// `ReplaceComponent { mailbox_id, wasm, drain_timeout_ms }`.
        /// `drain_timeout_ms` is accepted for wire compatibility but
        /// ignored under the trampoline's binding-stable replace.
        #[handler]
        fn on_replace_component(&mut self, ctx: &mut NativeCtx<'_>, payload: ReplaceComponent) {
            self.forward_to_trampoline(ctx, payload.mailbox_id, ReplaceComponent::ID, &payload);
        }
    }

    impl ComponentHostCapability {
        fn handle_load(&mut self, ctx: &mut NativeCtx<'_>, payload: LoadComponent) -> LoadResult {
            // 1. Parse + register kind descriptors (ADR-0028).
            let descriptors = match kind_manifest::read_from_bytes(&payload.wasm) {
                Ok(d) => d,
                Err(error) => return LoadResult::Err { error },
            };
            if let Err(error) = register_or_match_all(&self.registry, &descriptors) {
                return LoadResult::Err { error };
            }

            // 2. Parse capabilities manifest (ADR-0033).
            let capabilities = match kind_manifest::read_inputs_from_bytes(&payload.wasm) {
                Ok(c) => c,
                Err(error) => return LoadResult::Err { error },
            };

            // 3. Compile module.
            let module = match Module::new(&self.engine, &payload.wasm) {
                Ok(m) => m,
                Err(e) => {
                    return LoadResult::Err {
                        error: format!("invalid wasm module: {e}"),
                    };
                }
            };

            // 4. Resolve the component name. Caller > wasm-declared >
            // monotonic default.
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

            // 5. Spawn the trampoline. The framework spawn machinery
            // claims the namespace, registers the closure-bound
            // mailbox at `aether.component.trampoline:NAME`, runs
            // `WasmTrampoline::init` (which instantiates `Component`
            // against the trampoline's binding), and starts the
            // dispatcher thread. The returned id is the trampoline's
            // mailbox.
            let trampoline_config = WasmTrampolineConfig {
                engine: Arc::clone(&self.engine),
                linker: Arc::clone(&self.linker),
                module,
                registry: Arc::clone(&self.registry),
                outbound: Arc::clone(&self.outbound),
                capabilities: capabilities.clone(),
                // ADR-0090 (issue 1257): carry the load mail's init-config
                // bytes into the trampoline; `WasmTrampoline::init` hands
                // them to the guest's typed `init`.
                config: payload.config,
            };
            let mailbox_id = match ctx
                .spawn_child::<WasmTrampoline>(Subname::Named(&name), trampoline_config)
                .finish()
            {
                Ok(id) => id,
                Err(e) => {
                    return LoadResult::Err {
                        error: format!("trampoline spawn failed: {e:?}"),
                    };
                }
            };

            // 6. iamacoffeepot/aether#1037: register the trampoline's
            // ADR-0033 receive-side capabilities into the queryable
            // `CapabilityRegistry` so the DAG validator can ask
            // "does this mailbox accept kind K?". Same registry the
            // native-cap-boot path populates — one source of truth for
            // both transport flavours. `aether.component.replace`
            // re-registers (same mailbox id); `aether.component.drop`
            // clears.
            self.mailer.capability_registry().register(
                mailbox_id,
                MailboxCaps::from_component_capabilities(&capabilities),
            );

            // iamacoffeepot/aether#1128: the per-handler cost cells are
            // seeded inside `WasmTrampoline::init` (run just above, under
            // the spawn path's `with_stamped`), from the same
            // `capabilities` — both the global `CostTable` and the
            // trampoline's per-actor cache, over one shared `Arc`. Nothing
            // to seed cap-side here: `init` has the `ActorSlots` stamp this
            // thread does not.

            // ADR-0081 retired the chassis-pushed `ConfigureLogDrain`
            // mail. The freshly-spawned trampoline owns its own
            // `ActorLogRing` like every other actor; no drain
            // configuration is needed.

            // 7. Announce the new kind vocabulary AND mailbox inventory
            // upstream so the hub (and attached MCP sessions) see the
            // post-load surface. Mailboxes ship symmetrically with
            // kinds (issue iamacoffeepot/aether#730) — every load adds
            // exactly one trampoline mailbox at
            // `aether.component.trampoline:NAME`, and the snapshot
            // gives the hub the freshly-published name + category.
            self.outbound
                .egress_kinds_changed(self.registry.list_kind_descriptors());
            self.outbound
                .egress_mailboxes_changed(self.registry.list_mailbox_descriptors());

            LoadResult::Ok {
                mailbox_id,
                name: format!("{}:{}", WasmTrampoline::NAMESPACE, name),
                capabilities,
            }
        }

        /// Forward an arbitrary kind to a trampoline's mailbox,
        /// preserving the original `reply_to` so the trampoline's
        /// reply lands at the agent (not the cap). Used for
        /// [`DropComponent`] and [`ReplaceComponent`].
        fn forward_to_trampoline<P>(
            &self,
            ctx: &mut NativeCtx<'_>,
            recipient: MailboxId,
            kind: KindId,
            payload: &P,
        ) where
            P: serde::Serialize,
        {
            let bytes = match postcard::to_allocvec(payload) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(
                        target: "aether_capabilities::component",
                        error = %e,
                        "encode failed forwarding to trampoline; mail dropped",
                    );
                    return;
                }
            };
            let mail = Mail::new(recipient, kind, bytes, 1).with_reply_to(ctx.reply_target());
            self.mailer.push(mail);
        }
    }
}
