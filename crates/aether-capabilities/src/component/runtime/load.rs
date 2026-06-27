//! `handle_load` — the wasm component load sequence.
//!
//! Declared as `mod load;` under `runtime` (a sibling of `config`).
//! Under the ADR-0122 split the sequence is a method on
//! `ComponentHostCapabilityState`; its fields carry
//! `pub(in crate::component::runtime)` visibility so this sibling module retains
//! the same access as an inline impl block would.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aether_actor::Addressable;
use aether_kinds::{ComponentCapabilities, LoadComponent, LoadResult};
use wasmtime::Module;

use aether_substrate::actor::native::{NativeCtx, spawn::Subname};
use aether_substrate::actor::wasm::kind_manifest;
use aether_substrate::mail::capability::MailboxCaps;
use aether_substrate::mail::helpers::register_or_match_all;

use crate::trampoline::{WasmTrampoline, WasmTrampolineConfig};

use crate::component::runtime::ComponentHostCapabilityState;

impl ComponentHostCapabilityState {
    #[allow(
        clippy::too_many_lines,
        reason = "one cohesive load sequence: parse + register kinds, resolve the export, \
                  compile, name, spawn the trampoline, register caps, announce. Splitting it \
                  would thread the load payload + registry/engine handles through a helper \
                  for no clarity gain."
    )]
    pub(in crate::component::runtime) fn handle_load(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        payload: LoadComponent,
    ) -> LoadResult {
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
}
