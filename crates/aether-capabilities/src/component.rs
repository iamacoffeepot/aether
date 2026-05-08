//! `aether.component` cap (issue 603, renamed in issue 638 phase 3
//! from `aether.control`). The wasm-component lifecycle endpoint:
//! receives [`LoadComponent`] mail and spawns a per-component
//! [`WasmTrampoline`] (issue 634 Phase 4 PR 1) addressed at
//! `aether.component.trampoline:NAME`. [`DropComponent`] and
//! [`ReplaceComponent`] mail flow through the cap as well — it
//! forwards each to the addressed trampoline preserving the
//! original `reply_to`, so the trampoline replies directly to the
//! agent. The cap holds no per-component bookkeeping; the
//! trampoline manages its own lifecycle as an instanced
//! [`NativeActor`].
//!
//! Pre-Phase-4 the cap also owned the wasm dispatcher infrastructure
//! ([`ComponentEntry`], `dispatcher_loop`, `kill_actor`,
//! `splice_inbox`, etc.) and installed itself as the `Mailer`'s
//! [`ComponentRouter`] for component-bound routing. All of that
//! retired with the trampoline migration: dispatch lives on the
//! framework's `NativeActor` loop, replace is `Component`-swap
//! inside the trampoline, drop flows through `ctx.shutdown()`.
//!
//! The cap is a `#[bridge] mod native { ... }` per the ADR-0076 /
//! issue 565 pattern. Plain fields (no `Arc<Inner>` wrapper) per
//! ADR-0078 — the cap is single-threaded, every handler runs on the
//! cap's dispatcher thread.

use aether_kinds::{DropComponent, LoadComponent, ReplaceComponent};

#[cfg(not(target_arch = "wasm32"))]
pub use native::ComponentHostConfig;

#[aether_actor::bridge(singleton)]
mod native {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use aether_actor::actor;
    use aether_actor::actor::ctx::OutboundReply;
    use aether_data::Kind;
    use aether_kinds::LoadResult;
    use wasmtime::{Engine, Linker, Module};

    use super::{DropComponent, LoadComponent, ReplaceComponent};

    use crate::wasm_trampoline::{self, WasmTrampoline, WasmTrampolineConfig};
    use aether_substrate::actor::native::spawn::Subname;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::actor::wasm::component::ComponentCtx;
    use aether_substrate::actor::wasm::kind_manifest;
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::input::{self, InputSubscribers};
    use aether_substrate::mail::helpers::register_or_match_all;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;
    use aether_substrate::mail::{KindId, Mail, MailboxId};

    /// Configuration for [`ComponentHostCapability`]. `engine` and
    /// `linker` are the wasmtime instances every load instantiates
    /// against (handed through to the trampoline's
    /// [`Component::instantiate`] call); `hub_outbound` is the egress
    /// handle the cap uses for `aether.kinds.changed` announcements
    /// after each load; `input_subscribers` is the ADR-0021 fan-out
    /// table (shared with the platform thread + trampolines).
    pub struct ComponentHostConfig {
        pub engine: Arc<Engine>,
        pub linker: Arc<Linker<ComponentCtx>>,
        pub hub_outbound: Arc<HubOutbound>,
        pub input_subscribers: InputSubscribers,
    }

    /// `aether.component` cap. Plain-fields shape — single-threaded
    /// owner running on its dispatcher thread; no shared state.
    pub struct ComponentHostCapability {
        engine: Arc<Engine>,
        linker: Arc<Linker<ComponentCtx>>,
        registry: Arc<Registry>,
        mailer: Arc<Mailer>,
        outbound: Arc<HubOutbound>,
        input_subscribers: InputSubscribers,
        /// Monotonic counter for `component_N` default names when an
        /// agent passes `name: None` and the wasm doesn't declare an
        /// `aether.namespace`. AtomicU64 because the bridge macro
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
                input_subscribers: config.input_subscribers,
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
            // Cap-side cleanup: remove from input subscriber set so
            // the platform thread stops fanning input events at the
            // dying trampoline. The trampoline's own shutdown will
            // release any other registry state it owns.
            input::remove_from_all(&self.input_subscribers, payload.mailbox_id);
            self.forward_to_trampoline(ctx, payload.mailbox_id, DropComponent::ID, &payload);
        }

        /// Replace the component at `mailbox_id` with a fresh wasm
        /// binary. Forwards [`ReplaceComponent`] to the trampoline;
        /// the trampoline's [`WasmTrampoline::on_replace_component`]
        /// handler swaps `Component` internally and replies
        /// `ReplaceResult`. ADR-0022 + ADR-0038 splice invariants
        /// hold because the inbox channel is the trampoline's
        /// framework transport, which outlives the swap.
        ///
        /// # Agent
        /// `ReplaceComponent { mailbox_id, wasm, drain_timeout_ms }`.
        /// `drain_timeout_ms` is accepted for wire compatibility but
        /// ignored under the trampoline's transport-stable replace.
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
            // against the trampoline's transport), and starts the
            // dispatcher thread. The returned id is the trampoline's
            // mailbox.
            let trampoline_config = WasmTrampolineConfig {
                engine: Arc::clone(&self.engine),
                linker: Arc::clone(&self.linker),
                module,
                registry: Arc::clone(&self.registry),
                outbound: Arc::clone(&self.outbound),
                input_subscribers: Arc::clone(&self.input_subscribers),
                capabilities: capabilities.clone(),
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

            // 6. Push the chassis-current log drain to the freshly-
            // spawned trampoline. The trampoline's `#[handlers]`-
            // emitted `ConfigureLogDrain` handler stamps its per-
            // actor slot. Issue #601.
            if let Some(drain) = aether_actor::log::current_drain() {
                let cfg = aether_kinds::ConfigureLogDrain { mailbox: drain };
                let cfg_payload = bytemuck::bytes_of(&cfg).to_vec();
                let kind = <aether_kinds::ConfigureLogDrain as aether_data::Kind>::ID;
                self.mailer
                    .push(Mail::new(mailbox_id, kind, cfg_payload, 1));
            }

            // 7. Auto-subscribe stream-shaped handlers to their input
            // streams (ADR-0021 + ADR-0033).
            auto_subscribe_inputs(
                &self.input_subscribers,
                &self.registry,
                mailbox_id,
                &capabilities,
            );

            // 8. Announce the new kind vocabulary upstream so the hub
            // (and attached MCP sessions) see the post-load surface.
            self.outbound
                .egress_kinds_changed(self.registry.list_kind_descriptors());

            LoadResult::Ok {
                mailbox_id,
                name: wasm_trampoline::full_name(&name),
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

    /// Wire the freshly-spawned trampoline's mailbox into the
    /// subscriber set for every stream kind the component declares a
    /// `#[handler]` for (ADR-0068, issue #403). Lives at the cap
    /// because the cap parses the capabilities manifest and knows
    /// which kinds are streams; the trampoline itself doesn't need
    /// to inspect this map.
    fn auto_subscribe_inputs(
        input_subscribers: &InputSubscribers,
        registry: &Registry,
        mailbox: MailboxId,
        capabilities: &aether_kinds::ComponentCapabilities,
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
}
