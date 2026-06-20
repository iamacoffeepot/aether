//! `aether.component` cap (issue 603, renamed in issue 638 phase 3
//! from `aether.control`). The wasm-component lifecycle endpoint:
//! receives [`LoadComponent`] mail and spawns a per-component
//! [`WasmTrampoline`] (issue 634 Phase 4 PR 1) addressed at
//! `aether.embedded:NAME`. [`DropComponent`] and
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

use aether_actor::{Addressable, WasmActorMailbox};
use aether_kinds::{DropComponent, ListComponents, LoadComponent, ReplaceComponent};
#[cfg(not(target_arch = "wasm32"))]
use aether_kinds::{LifecycleUnsubscribeAll, UnsubscribeAll};
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
/// `R: Addressable` is the peer's actor type, supplied by the caller (same
/// as today's `WasmCtx::resolve_actor` surface). Type-checks at the
/// send site — `peer.send::<K>(&mail)` compiles only when
/// `R: HandlesKind<K>`.
pub trait ComponentHostWasmExt {
    /// Resolve a typed peer-component mailbox for the loaded component
    /// named `name`. The full mailbox address is
    /// `format!("{}:{}", WasmTrampoline::NAMESPACE, name)`. The resolved
    /// handle inherits this handle's ctx binding (`sender` + inline
    /// registry), so its sends stamp the same origin (issue 1987).
    fn loaded<R: Addressable>(&self, name: &str) -> WasmActorMailbox<'_, R>;
}

impl ComponentHostWasmExt for WasmActorMailbox<'_, ComponentHostCapability> {
    fn loaded<R: Addressable>(&self, name: &str) -> WasmActorMailbox<'_, R> {
        self.resolve_peer_scoped::<R>(WasmTrampoline::NAMESPACE, name)
    }
}

/// Sender-side facade for native cap-to-cap callers addressing a
/// loaded peer component through [`ComponentHostCapability`]. Same
/// shape as [`ComponentHostWasmExt`] for the native transport — the
/// returned handle inherits the parent mailbox's `'a` binding ref so
/// `.send::<K>(&mail)` dispatches through the same `NativeBinding`
/// without re-threading the ctx.
#[cfg(not(target_arch = "wasm32"))]
pub trait ComponentHostNativeExt {
    /// Resolve a typed peer-component mailbox for the loaded component
    /// named `name`. The full mailbox address is
    /// `format!("{}:{}", WasmTrampoline::NAMESPACE, name)`.
    fn loaded<R: Addressable>(&self, name: &str) -> NativeActorMailbox<'_, R>;
}

#[cfg(not(target_arch = "wasm32"))]
impl ComponentHostNativeExt for NativeActorMailbox<'_, ComponentHostCapability> {
    fn loaded<R: Addressable>(&self, name: &str) -> NativeActorMailbox<'_, R> {
        self.resolve_peer_scoped::<R>(WasmTrampoline::NAMESPACE, name)
    }
}

/// Resolve the [`MailboxId`](aether_data::MailboxId) of the embeddable
/// component loaded under `name`, by folding the instance node
/// `aether.embedded:<name>` (the [`Embedded`](aether_actor::Embedded)
/// resolver) onto the `aether.component` host cap's carry (ADR-0099 §5/§6,
/// ADR-0119).
///
/// This is the by-name carry-supplier. `aether-actor`'s `Embedded` resolver
/// owns the fold and the reserved scope
/// ([`EMBEDDED_SCOPE`](aether_actor::EMBEDDED_SCOPE)); this fn supplies the
/// `aether.component` carry, read only from its owner
/// [`ComponentHostCapability`]. Equal by construction to a component's own
/// `type Resolver = Embedded` and to the by-name verb
/// [`loaded::<R>(name)`](ComponentHostWasmExt::loaded), so bare-type and
/// by-name addressing agree. Available on every target — a wasm peer resolves
/// an embeddable the same way a native one does, no transport branch
/// (ADR-0029 client-side no-lookup).
#[must_use]
pub fn resolve_embedded(name: &str) -> aether_data::MailboxId {
    use aether_actor::{Addressable, Embedded, Resolve};
    Embedded::resolve(
        <ComponentHostCapability as Addressable>::resolve(0, ()).0,
        name,
        (),
    )
}

#[aether_actor::bridge(singleton)]
mod native {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use aether_actor::Addressable;
    use aether_actor::actor;
    use aether_data::Kind;
    use aether_data::MailboxCategory;
    use aether_kinds::{ComponentCapabilities, ListComponentsResult, LoadResult};
    use wasmtime::{Engine, Linker, Module};

    use super::{
        DropComponent, LifecycleUnsubscribeAll, ListComponents, LoadComponent, ReplaceComponent,
        UnsubscribeAll,
    };

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
    use aether_substrate::mail::{KindId, MailboxId};

    use crate::input::InputCapability;
    use crate::lifecycle::LifecycleCapability;
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
        /// [`WasmTrampoline`] under `aether.embedded:NAME`,
        /// and replies `LoadResult::Ok { mailbox_id, name,
        /// capabilities }` where `name` is the full trampoline
        /// address — agents send subsequent mail to that name.
        /// Errors (bad wire bytes, kind conflict, name conflict,
        /// invalid wasm, instantiation trap) come back as
        /// `LoadResult::Err`.
        #[handler]
        fn on_load_component(
            &mut self,
            ctx: &mut NativeCtx<'_>,
            payload: LoadComponent,
        ) -> LoadResult {
            // ADR-0109: the return type is the reply contract — the
            // `#[actor]` macro routes this `LoadResult` back to the sender
            // through `OutboundReply::reply`, so no manual `ctx.reply`.
            self.handle_load(ctx, payload)
        }

        /// Drop a component by its mailbox id. Forwards
        /// [`DropComponent`] mail to the addressed trampoline; the
        /// trampoline's [`WasmTrampoline::on_drop_component`] handler
        /// replies `DropResult::Ok` and shuts itself down.
        ///
        /// Before forwarding, purges the dying trampoline's mailbox from
        /// every fan-out subscriber table so no cap keeps firing at a
        /// dropped mailbox: `aether.input`'s input-stream tables (via
        /// [`UnsubscribeAll`]) and `aether.lifecycle`'s per-stage tables
        /// (via [`LifecycleUnsubscribeAll`]).
        ///
        /// # Agent
        /// `DropComponent { mailbox_id }`. The `mailbox_id` is the
        /// trampoline's id from the `LoadResult.mailbox_id` field.
        #[handler]
        fn on_drop_component(&mut self, ctx: &mut NativeCtx<'_>, payload: DropComponent) {
            // Cap-side cleanup: ask each owning cap to drop the dying
            // trampoline from its fan-out sets. Mail rather than direct
            // mutation post-issue-640 — each cap is the sole owner of its
            // own subscriber table.
            ctx.actor::<InputCapability>().send(&UnsubscribeAll {
                mailbox: payload.mailbox_id,
            });
            ctx.actor::<LifecycleCapability>()
                .send(&LifecycleUnsubscribeAll {
                    mailbox: payload.mailbox_id.0,
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
        /// `ReplaceComponent { mailbox_id, wasm, drain_timeout_ms, config, export }`.
        /// `drain_timeout_ms` is accepted for wire compatibility but
        /// ignored under the trampoline's binding-stable replace.
        /// `export` (ADR-0096) names which exported actor type of the
        /// replacement module to instantiate; `None` reuses the type the
        /// trampoline currently hosts.
        #[handler]
        fn on_replace_component(&mut self, ctx: &mut NativeCtx<'_>, payload: ReplaceComponent) {
            self.forward_to_trampoline(ctx, payload.mailbox_id, ReplaceComponent::ID, &payload);
        }

        /// Enumerate the components this engine has actually loaded and
        /// registered, by their ADR-0099 lineage names (issue 2020).
        ///
        /// Reads the registry's live mailbox snapshot — the same list
        /// already egressed to the hub after each load — and keeps only the
        /// [`MailboxCategory::Trampoline`] entries, the loaded-component set.
        /// Chassis caps are boot-present and static, so the trampolines are
        /// the only registry membership a readiness poll cares about. The
        /// reply is names only: the mailbox id is a deterministic hash-chain
        /// over the lineage the name renders (ADR-0099) and routing is the
        /// substrate's job, so the caller never needs the handle.
        ///
        /// # Agent
        /// Fieldless `ListComponents` to the `aether.component` mailbox —
        /// guaranteed present from boot, so the send always resolves and the
        /// reply is a definitive snapshot. Reply `ListComponentsResult {
        /// names }` lists every currently-loaded component's full lineage
        /// address (`aether.component/aether.embedded:NAME`). Poll it after a
        /// boot-manifest spawn (ADR-0116) to learn deterministically when a
        /// requested component is loaded, instead of inferring liveness by
        /// proxy.
        #[handler]
        fn on_list_components(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            _payload: ListComponents,
        ) -> ListComponentsResult {
            let names = self
                .registry
                .list_mailbox_descriptors()
                .into_iter()
                .filter(|d| d.category == Some(MailboxCategory::Trampoline))
                .map(|d| d.name)
                .collect();
            ListComponentsResult { names }
        }
    }

    impl ComponentHostCapability {
        #[allow(
            clippy::too_many_lines,
            reason = "one cohesive load sequence: parse + register kinds, resolve the export, \
                      compile, name, spawn the trampoline, register caps, announce. Splitting it \
                      would thread the load payload + registry/engine handles through a helper \
                      for no clarity gain."
        )]
        fn handle_load(&mut self, ctx: &mut NativeCtx<'_>, payload: LoadComponent) -> LoadResult {
            // 1. Parse + register kind descriptors (ADR-0028).
            let descriptors = match kind_manifest::read_from_bytes(&payload.wasm) {
                Ok(d) => d,
                Err(error) => return LoadResult::Err { error },
            };
            if let Err(error) = register_or_match_all(&self.registry, &descriptors) {
                return LoadResult::Err { error };
            }

            // 2. Parse the per-actor capability manifest (ADR-0033 /
            //    ADR-0096) and resolve which exported type to load.
            //    `export: None` selects the entry (first) type — the
            //    only type a single-actor module has — so the legacy
            //    load is unchanged. A named selector must match one of
            //    the module's `ActorBoundary` namespaces, else the load
            //    fails cleanly. The selected type's `type_tag` drives
            //    `init_typed_p32` at instantiate; `None` keeps the
            //    legacy entry-init path.
            let actors = match kind_manifest::read_actor_inputs_from_bytes(&payload.wasm) {
                Ok(a) => a,
                Err(error) => return LoadResult::Err { error },
            };
            let (capabilities, type_tag, selected_namespace): (
                ComponentCapabilities,
                Option<u64>,
                Option<String>,
            ) = if let Some(requested) = &payload.export {
                let Some(group) = actors
                    .iter()
                    .find(|a| a.namespace.as_deref() == Some(requested.as_str()))
                else {
                    let available: Vec<&str> = actors
                        .iter()
                        .filter_map(|a| a.namespace.as_deref())
                        .collect();
                    return LoadResult::Err {
                        error: format!(
                            "export {requested:?} not found in module; exported types: {available:?}"
                        ),
                    };
                };
                (
                    group.capabilities.clone(),
                    // Runtime-name routing: `requested` is the export namespace
                    // from the wire load request, resolved to its actor-type tag.
                    #[allow(clippy::disallowed_methods)]
                    Some(aether_data::mailbox_id_from_name(requested).0),
                    Some(requested.clone()),
                )
            } else {
                let entry = actors.first();
                (
                    entry.map(|a| a.capabilities.clone()).unwrap_or_default(),
                    None,
                    entry.and_then(|a| a.namespace.clone()),
                )
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

            // 4. Resolve the component name. Caller > selected export's
            // namespace > wasm-declared entry namespace > monotonic
            // default. A non-entry export defaults its mailbox name to
            // the selected type's namespace, the multi-actor analog of
            // the single-actor `aether.namespace` fallback.
            let name = match payload.name {
                Some(n) => n,
                None => match selected_namespace {
                    Some(ns) => ns,
                    None => match kind_manifest::read_namespace_from_bytes(&payload.wasm) {
                        Ok(Some(declared)) => declared,
                        Ok(None) => {
                            let n = self.default_name_counter.fetch_add(1, Ordering::Relaxed);
                            format!("component_{n}")
                        }
                        Err(error) => return LoadResult::Err { error },
                    },
                },
            };

            // 5. Spawn the trampoline. The framework spawn machinery
            // claims the namespace, registers the closure-bound
            // mailbox at `aether.embedded:NAME`, runs
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
                // ADR-0096: the selected export's actor-type tag, threaded
                // through to `Component::instantiate` so it calls
                // `init_typed_p32`. `None` = entry type (single-actor
                // modules and unselected loads keep the legacy init path).
                type_tag,
                // ADR-0097: the full per-type capability map, so a guest
                // `spawn_child::<Sibling>` can register the spawned
                // sibling's own handler set (looked up by actor-type tag).
                actor_caps: actors,
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
            // `aether.embedded:NAME`, and the snapshot
            // gives the hub the freshly-published name + category.
            self.outbound
                .egress_kinds_changed(self.registry.list_kind_descriptors());
            self.outbound
                .egress_mailboxes_changed(self.registry.list_mailbox_descriptors());

            LoadResult::Ok {
                mailbox_id,
                // ADR-0099 §3/§4: report the name the spawn machinery
                // actually registered — the `/`-rendered lineage
                // (`aether.component/aether.embedded:NAME`) —
                // read back from the registry so `LoadResult.name` can
                // never disagree with the live entry. The id is the
                // lineage fold, not `hash(name)`.
                name: self
                    .registry
                    .mailbox_name(mailbox_id)
                    .unwrap_or_else(|| format!("{}:{}", WasmTrampoline::NAMESPACE, name)),
                capabilities,
            }
        }

        /// Forward an arbitrary kind to a trampoline's mailbox,
        /// preserving the original `reply_to` so the trampoline's
        /// reply lands at the agent (not the cap). Used for
        /// [`DropComponent`] and [`ReplaceComponent`].
        ///
        /// The forward threads the child mail under the cap's current
        /// in-flight root and bumps that root's `in_flight` count before
        /// this handler returns (`send_envelope_traced_with_reply_to`),
        /// so the originating call stays open across the boundary: the
        /// trampoline's deferred `ctx.reply` streams back under a still-
        /// open root and settlement fires `ReplyEnd` only after it. A
        /// bare enqueue would let the cap handler's return settle the
        /// call before the trampoline replied, dropping the reply (the
        /// deferred-reply hold-open contract).
        ///
        /// Kept a method (not an associated fn) so its two `#[handler]`
        /// call sites read `self`; the macro-dispatched handlers must
        /// take `&mut self`, so dropping the receiver here would only
        /// move the `unused_self` lint onto them.
        #[allow(clippy::unused_self)]
        fn forward_to_trampoline<P>(
            &self,
            ctx: &mut NativeCtx<'_>,
            recipient: MailboxId,
            kind: KindId,
            payload: &P,
        ) where
            P: Kind,
        {
            let bytes = payload.encode_into_bytes();
            let _ =
                ctx.send_envelope_traced_with_reply_to(recipient, kind, &bytes, ctx.reply_target());
        }
    }
}

#[cfg(test)]
mod tests {
    // These tests construct the host carry and assert the canonical
    // trampoline-address fold against the flat name hash — the primitive is
    // the reference value under test, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]
    use aether_actor::wasm::inline::InlineRegistry;
    use aether_actor::{Addressable, WasmActorMailbox};
    use aether_data::{ActorId, MailboxId, Tag, fold_lineage, mailbox_id_from_name, with_tag};

    use super::{ComponentHostCapability, ComponentHostWasmExt};
    use crate::trampoline::WasmTrampoline;

    /// A loaded component's id is the ADR-0099 §3 lineage fold over
    /// `[aether.component, aether.embedded:<name>]`. `loaded`
    /// composes exactly that — folding the trampoline node's `ActorId`
    /// onto the component host's carry — so it agrees with the id the
    /// spawn machinery registers it under. It must **not** resolve the
    /// bare load-name (`ctx.actor::<R>()` hashing the bare `NAMESPACE`),
    /// nor the pre-0099 flat `trampoline:<name>` hash — both reach a
    /// mailbox nothing is registered under (the #1364 footgun). This pins
    /// the one canonical path for a loaded component.
    #[test]
    fn loaded_composes_the_canonical_trampoline_address() {
        // `R` is arbitrary here — the resolved id depends only on the
        // host carry + the trampoline node name. The ctx binding (sender +
        // inline registry) is irrelevant to id resolution, so a throwaway
        // registry and a zero sender suffice (issue 1987).
        let registry = InlineRegistry::new();
        let host = WasmActorMailbox::<ComponentHostCapability>::__new(
            mailbox_id_from_name(ComponentHostCapability::NAMESPACE).0,
            0,
            &registry,
        );
        let camera = host.loaded::<ComponentHostCapability>("aether.camera");

        // The component host is root-pinned (depth-1), so its carry is
        // its own id; fold the trampoline node onto it.
        let host_carry = mailbox_id_from_name(ComponentHostCapability::NAMESPACE).0;
        let node = ActorId::instanced(WasmTrampoline::NAMESPACE, "aether.camera");
        let canonical = MailboxId(with_tag(Tag::Mailbox, fold_lineage(host_carry, node)));
        assert_eq!(camera.mailbox_id(), canonical);

        // Not the pre-0099 flat name-hash, and not the bare load-name.
        assert_ne!(
            camera.mailbox_id(),
            mailbox_id_from_name(&format!("{}:camera", WasmTrampoline::NAMESPACE)),
        );
        assert_ne!(camera.mailbox_id(), mailbox_id_from_name("camera"));
    }

    /// Root singletons (chassis caps) are unchanged by the scoped-name
    /// composition: the cap's own mailbox id is its bare `NAMESPACE`.
    #[test]
    fn root_singleton_id_is_the_bare_namespace() {
        let registry = InlineRegistry::new();
        let host = WasmActorMailbox::<ComponentHostCapability>::__new(
            mailbox_id_from_name(ComponentHostCapability::NAMESPACE).0,
            0,
            &registry,
        );
        assert_eq!(
            host.mailbox_id(),
            mailbox_id_from_name(ComponentHostCapability::NAMESPACE),
        );
    }
}
