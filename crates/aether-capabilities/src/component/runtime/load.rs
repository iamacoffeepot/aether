//! `handle_load` — the wasm component load sequence.
//!
//! Declared as `mod load;` at the `component` level (a sibling of `runtime`).
//! Under the ADR-0122 split the sequence is a method on
//! `ComponentHostCapabilityState`; its fields carry
//! `pub` visibility so this sibling module retains the
//! same access as an inline impl block would.

use std::sync::Arc;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_kinds::{ComponentCapabilities, DropComponent, LoadComponent, LoadResult};
use wasmtime::Module;

use aether_substrate::actor::native::{NativeCtx, spawn::Subname};
use aether_substrate::actor::wasm::kind_manifest::{self, ActorInputs};
use aether_substrate::mail::MailboxId;
use aether_substrate::mail::helpers::register_or_match_all;

use crate::trampoline::{WasmTrampoline, WasmTrampolineConfig};

use crate::component::runtime::{BootEntry, ComponentHostCapabilityState};

/// sha256 hex over `wasm` — the ADR-0147 module-boot dedup key. A small local
/// helper rather than a reach into the hub's private
/// `engine::store::persistence::hash_hex`: that function is scoped to the
/// content-addressed binary store (a hub-only domain), so the load path owns
/// its own six-line hash rather than coupling the component loader to hub
/// bookkeeping. Both call sites hash the same way; neither depends on the
/// other.
fn content_hash_hex(wasm: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(wasm);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl ComponentHostCapabilityState {
    #[allow(
        clippy::too_many_lines,
        reason = "one cohesive load sequence: parse + register kinds, resolve the export, \
                  compile, name, spawn the trampoline, register caps, announce. Splitting it \
                  would thread the load payload + registry/engine handles through a helper \
                  for no clarity gain."
    )]
    pub fn handle_load(&mut self, ctx: &mut NativeCtx<'_>, payload: LoadComponent) -> LoadResult {
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
        //    `export: None` selects the default (first) type — the
        //    only type a single-actor module has — so the legacy
        //    load is unchanged. A named selector must match one of
        //    the module's `ActorBoundary` namespaces, else the load
        //    fails cleanly. The selected type's `type_tag` drives
        //    `init_typed_p32` at instantiate; `None` keeps the
        //    legacy default-init path.
        let actors = match kind_manifest::read_actor_inputs_from_bytes(&payload.wasm) {
            Ok(a) => a,
            Err(error) => return LoadResult::Err { error },
        };
        let (capabilities, type_tag, selected_namespace): (ComponentCapabilities, Option<u64>, Option<String>) =
            if let Some(requested) = &payload.export {
                let Some(group) = actors.iter().find(|a| a.namespace.as_deref() == Some(requested.as_str())) else {
                    let available: Vec<&str> = actors.iter().filter_map(|a| a.namespace.as_deref()).collect();
                    return LoadResult::Err {
                        error: format!("export {requested:?} not found in module; exported types: {available:?}"),
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
            } else if kind_manifest::read_no_default_marker(&payload.wasm) {
                // ADR-0138: a defaultless multi-actor module (built with a bare
                // `export!(A, B, …)`) carries no bare-load default. An unselected
                // load is a hard error that names the exports so the caller can
                // pick one, rather than instantiating an actor by list position.
                let available: Vec<&str> = actors.iter().filter_map(|a| a.namespace.as_deref()).collect();
                return LoadResult::Err {
                    error: format!(
                        "module has no default (ADR-0138): load one of its exports \
                     by name via the export selector; exported types: {available:?}"
                    ),
                };
            } else {
                let default_actor = actors.first();
                (
                    default_actor.map(|a| a.capabilities.clone()).unwrap_or_default(),
                    None,
                    default_actor.and_then(|a| a.namespace.clone()),
                )
            };

        // 2b. ADR-0147: does this module declare an unconditional `boot =`
        // slot? Read it before compiling so the non-selectability guard can
        // reject a load that tries to select the boot type by name before any
        // trampoline is spawned. A bootless module (the common case) reads
        // `None` here and skips all boot machinery below, byte-for-byte the
        // pre-ADR-0147 path.
        let boot_namespace = match kind_manifest::read_boot_namespace_from_bytes(&payload.wasm) {
            Ok(b) => b,
            Err(error) => return LoadResult::Err { error },
        };
        if let Some(boot_ns) = &boot_namespace {
            // Non-selectability (ADR-0147 §1): boot is instantiated
            // unconditionally, once per loaded module, and is never reachable
            // through the export selector. Reject the selection before the
            // ordinary export-resolution match could spawn a second, untracked
            // boot-type trampoline alongside the singleton.
            if payload.export.as_deref() == Some(boot_ns.as_str()) {
                return LoadResult::Err {
                    error: format!(
                        "export {boot_ns:?} names this module's boot actor, which is not selectable \
                         (ADR-0147): the boot actor is instantiated unconditionally, once per loaded \
                         module content hash, and cannot be loaded by an export selector"
                    ),
                };
            }
        }

        // 3. Compile module.
        let module = match Module::new(&self.engine, &payload.wasm) {
            Ok(m) => m,
            Err(e) => {
                return LoadResult::Err { error: format!("invalid wasm module: {e}") };
            }
        };

        // 3b. ADR-0147: ensure the module's boot singleton exists before
        // spawning the requested actor, reusing the just-compiled module for
        // the boot spawn (a cheap `Arc`-backed `Module::clone`). First sight of
        // a module's content hash on this engine spawns the boot; a later load
        // of any export of the same content finds it already present and only
        // refcounts against it. The returned hash keys the refcount bump after
        // the requested-actor spawn succeeds.
        let boot_hash: Option<String> = match &boot_namespace {
            Some(boot_ns) => {
                let hash = content_hash_hex(&payload.wasm);
                if !self.boot_registry.contains_key(&hash) {
                    let boot_mailbox = match self.spawn_boot_singleton(ctx, &module, boot_ns, &actors) {
                        Ok(id) => id,
                        Err(error) => return LoadResult::Err { error },
                    };
                    self.boot_registry.insert(hash.clone(), BootEntry { mailbox_id: boot_mailbox, refcount: 0 });
                }
                Some(hash)
            }
            None => None,
        };

        // 4. Resolve the component name. Caller > selected export's
        // namespace > wasm-declared default namespace > monotonic
        // default. A non-default export defaults its mailbox name to
        // the selected type's namespace, the multi-actor analog of
        // the single-actor `aether.namespace` fallback.
        let name = match payload.name.or(selected_namespace) {
            Some(n) => n,
            None => match kind_manifest::read_namespace_from_bytes(&payload.wasm) {
                Ok(Some(declared)) => declared,
                Ok(None) => {
                    let n = self.default_name_counter;
                    self.default_name_counter += 1;
                    format!("component_{n}")
                }
                Err(error) => return LoadResult::Err { error },
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
            // `init_typed_p32`. `None` = default type (single-actor
            // modules and unselected loads keep the legacy init path).
            type_tag,
            // ADR-0097: the full per-type capability map, so a guest
            // `spawn_child::<Sibling>` can register the spawned
            // sibling's own handler set (looked up by actor-type tag).
            actor_caps: actors,
        };
        let mailbox_id = match ctx.spawn_child::<WasmTrampoline>(Subname::Named(&name), trampoline_config).finish() {
            Ok(id) => id,
            Err(e) => {
                return LoadResult::Err { error: format!("trampoline spawn failed: {e:?}") };
            }
        };

        // 5b. ADR-0147: refcount this non-boot actor against its module's boot
        // singleton and record its mailbox → content-hash so a later drop /
        // replace can find and decrement the right entry. Runs only for a
        // boot-bearing module; a bootless load leaves both tables untouched.
        if let Some(hash) = &boot_hash {
            if let Some(entry) = self.boot_registry.get_mut(hash) {
                entry.refcount += 1;
            }
            self.boot_hash_by_actor.insert(mailbox_id, hash.clone());
        }

        // 6. iamacoffeepot/aether#1037: register the trampoline's
        // ADR-0033 receive-side capabilities into the queryable
        // `CapabilityRegistry` so the DAG validator can ask
        // "does this mailbox accept kind K?". Same registry the
        // native-cap-boot path populates — one source of truth for
        // both transport flavours. `aether.component.replace`
        // re-registers (same mailbox id); `aether.component.drop`
        // clears.
        self.mailer.capability_registry().register(mailbox_id, &capabilities);

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
        self.outbound.egress_kinds_changed(self.registry.list_kind_descriptors());
        self.outbound.egress_mailboxes_changed(self.registry.list_mailbox_descriptors());

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

    /// ADR-0147: spawn a module's boot trampoline and register its
    /// capabilities. The caller supplies the just-compiled `module` (reused via
    /// a cheap `Arc`-backed clone) and the parsed `actors` list, and has already
    /// confirmed the content hash is unregistered. Boot is spawned through the
    /// same `WasmTrampoline` path as any named export — its type tag is its
    /// `NAMESPACE` hash and it is constructed by `init_typed_p32` — but it
    /// receives no caller config (it is not the export the caller asked for).
    fn spawn_boot_singleton(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        module: &Module,
        boot_namespace: &str,
        actors: &[ActorInputs],
    ) -> Result<MailboxId, String> {
        let boot_caps = actors
            .iter()
            .find(|a| a.namespace.as_deref() == Some(boot_namespace))
            .map(|a| a.capabilities.clone())
            .unwrap_or_default();
        // Runtime-name routing: the boot type tag is the fold of its NAMESPACE,
        // the same resolution `handle_load` performs for a requested export.
        #[allow(
            clippy::disallowed_methods,
            reason = "runtime-name routing: the boot actor-type tag is derived from its NAMESPACE, \
                      exactly as the requested export's tag is at the export-resolution step above"
        )]
        let boot_tag = aether_data::mailbox_id_from_name(boot_namespace).0;
        let boot_config = WasmTrampolineConfig {
            engine: Arc::clone(&self.engine),
            linker: Arc::clone(&self.linker),
            module: module.clone(),
            registry: Arc::clone(&self.registry),
            outbound: Arc::clone(&self.outbound),
            capabilities: boot_caps.clone(),
            // Boot receives no caller-supplied config — it is unconditional, not
            // the export the load selected.
            config: Vec::new(),
            type_tag: Some(boot_tag),
            actor_caps: actors.to_vec(),
        };
        let boot_mailbox = ctx
            .spawn_child::<WasmTrampoline>(Subname::Named(boot_namespace), boot_config)
            .finish()
            .map_err(|e| format!("boot trampoline spawn failed: {e:?}"))?;
        self.mailer.capability_registry().register(boot_mailbox, &boot_caps);
        Ok(boot_mailbox)
    }

    /// ADR-0147: account a departing non-boot actor against its module's boot
    /// singleton. Decrements the owning module's refcount; when it reaches zero
    /// (the last non-boot actor from the module is gone), self-sends a
    /// fire-and-forget [`DropComponent`] to the boot trampoline — tearing it
    /// down through the same handler any component drop takes — and forgets the
    /// registry entry. A no-op for an actor from a bootless module (nothing was
    /// tracked). The teardown send is detached, not a `forward_to_trampoline`:
    /// the boot's `DropResult` must not route back to whoever originated the
    /// external drop, so it starts a fresh chain and its reply lands harmlessly
    /// at the cap.
    pub fn release_boot_ref(&mut self, ctx: &mut NativeCtx<'_>, actor_mailbox: MailboxId) {
        let Some(hash) = self.boot_hash_by_actor.remove(&actor_mailbox) else {
            return;
        };
        let Some(entry) = self.boot_registry.get_mut(&hash) else {
            return;
        };
        entry.refcount = entry.refcount.saturating_sub(1);
        if entry.refcount == 0 {
            let boot_mailbox = entry.mailbox_id;
            self.boot_registry.remove(&hash);
            let bytes = DropComponent { mailbox_id: boot_mailbox }.encode_into_bytes();
            let _ = ctx.send_envelope_detached(boot_mailbox, DropComponent::ID, &bytes);
        }
    }

    /// ADR-0147: rebind an actor's boot bookkeeping across an
    /// `aether.component.replace` (ADR-0022 in-place module swap). The actor's
    /// old module refcount is decremented exactly like a drop (tearing its boot
    /// down if it was the last), and — if the replacement's `new_wasm` declares
    /// a boot slot — the new module's boot singleton is spawned-if-absent and
    /// incremented exactly like a load, with `boot_hash_by_actor` repointed at
    /// the new content hash (or cleared if the replacement is bootless). The new
    /// content is validated (manifest parse + compile) before the old refcount
    /// is touched, so a replace onto malformed wasm leaves the old boot state
    /// intact rather than tearing it down for a swap that will fail downstream.
    pub fn rebind_boot_ref(&mut self, ctx: &mut NativeCtx<'_>, actor_mailbox: MailboxId, new_wasm: &[u8]) {
        // Resolve + validate the replacement's boot slot up front, before
        // touching the old refcount. On any parse/compile error leave all boot
        // bookkeeping untouched and let the forwarded replace surface the wasm
        // error to the caller — a replace onto malformed wasm must not tear the
        // old boot down for a swap that will fail downstream. The spawn decision
        // is deferred to after the decrement (a same-content replace may drop
        // the shared entry to zero and back).
        let new_boot: Option<(String, String, Vec<ActorInputs>, Module)> =
            match kind_manifest::read_boot_namespace_from_bytes(new_wasm) {
                Ok(Some(boot_ns)) => {
                    match (kind_manifest::read_actor_inputs_from_bytes(new_wasm), Module::new(&self.engine, new_wasm)) {
                        (Ok(actors), Ok(module)) => Some((boot_ns, content_hash_hex(new_wasm), actors, module)),
                        _ => return,
                    }
                }
                Ok(None) => None,
                Err(_) => return,
            };

        // Decrement the old module refcount (drop semantics), tearing its boot
        // down if this was its last non-boot actor.
        self.release_boot_ref(ctx, actor_mailbox);

        // Increment the new module refcount (load semantics), spawning its boot
        // singleton if the content is not (or is no longer) resident.
        if let Some((boot_ns, hash, actors, module)) = new_boot {
            if !self.boot_registry.contains_key(&hash) {
                match self.spawn_boot_singleton(ctx, &module, &boot_ns, &actors) {
                    Ok(boot_mailbox) => {
                        self.boot_registry.insert(hash.clone(), BootEntry { mailbox_id: boot_mailbox, refcount: 0 });
                        // Announce the freshly-spawned boot mailbox so
                        // `ListComponents` and the hub see it (a load egresses
                        // this; a replace otherwise would not).
                        self.outbound.egress_mailboxes_changed(self.registry.list_mailbox_descriptors());
                    }
                    Err(_) => return,
                }
            }
            if let Some(entry) = self.boot_registry.get_mut(&hash) {
                entry.refcount += 1;
            }
            self.boot_hash_by_actor.insert(actor_mailbox, hash);
        }
    }
}
