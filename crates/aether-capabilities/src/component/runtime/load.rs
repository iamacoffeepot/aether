//! `handle_load` — the wasm component load sequence.
//!
//! Declared as `mod load;` at the `component` level (a sibling of `runtime`).
//! Under the ADR-0122 split the sequence is a method on
//! `ComponentHostCapabilityState`; its fields carry
//! `pub` visibility so this sibling module retains the
//! same access as an inline impl block would.

use std::sync::Arc;

use aether_actor::{Addressable, Manual, OutboundReply, ReplyMode};
use aether_data::Kind;
use aether_kinds::{ComponentCapabilities, DropComponent, LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};
use wasmtime::Module;

use aether_substrate::actor::native::{NativeCtx, spawn::Subname};
use aether_substrate::actor::wasm::kind_manifest::{self, ActorInputs};
use aether_substrate::mail::MailboxId;
use aether_substrate::mail::helpers::register_or_match_all;

use crate::trampoline::{WasmTrampoline, WasmTrampolineConfig};

use crate::component::runtime::{BootEntry, ComponentHostCapabilityState, PendingReplace};

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

        // 2a. ADR-0147: read the module's optional `boot =` slot up front. It is
        // needed here, before export resolution, because the boot actor is a
        // member of `actors` yet is instantiated unconditionally — never the
        // bare-load default and never selectable — so the default-actor branch
        // below must exclude it and the selectability guard (step 2b) must
        // reject a load that names it. A bootless module (the common case) reads
        // `None` and keeps the pre-ADR-0147 path byte-for-byte.
        let boot_namespace = match kind_manifest::read_boot_namespace_from_bytes(&payload.wasm) {
            Ok(b) => b,
            Err(error) => return LoadResult::Err { error },
        };

        // 2b. ADR-0147 non-selectability guard (§1): boot is instantiated
        // unconditionally, once per loaded module, and is never reachable through
        // the export selector. Reject a boot-naming selection up front, before
        // export resolution runs, so no code path can treat the boot type as a
        // selectable export.
        if let Some(boot_ns) = &boot_namespace
            && payload.export.as_deref() == Some(boot_ns.as_str())
        {
            return LoadResult::Err {
                error: format!(
                    "export {boot_ns:?} names this module's boot actor, which is not selectable \
                     (ADR-0147): the boot actor is instantiated unconditionally, once per loaded \
                     module content hash, and cannot be loaded by an export selector"
                ),
            };
        }

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
                // ADR-0147: the boot actor is a member of `actors` but is never
                // the bare-load default (it is instantiated unconditionally, not
                // selected), so exclude it from the default candidate — else
                // `actors.first()` could hand the caller the boot actor as the
                // loaded component. A bootless module keeps `actors.first()`.
                let default_actor = boot_namespace.as_deref().map_or_else(
                    || actors.first(),
                    |boot_ns| actors.iter().find(|a| a.namespace.as_deref() != Some(boot_ns)),
                );
                (
                    default_actor.map(|a| a.capabilities.clone()).unwrap_or_default(),
                    None,
                    default_actor.and_then(|a| a.namespace.clone()),
                )
            };

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
        // the requested-actor spawn succeeds. `boot_freshly_inserted` records
        // whether *this* load was the one that spawned the boot (as opposed to
        // finding it already resident), so a later requested-actor spawn failure
        // can tear the just-spawned boot back down instead of leaking it.
        let (boot_hash, boot_freshly_inserted): (Option<String>, bool) = match &boot_namespace {
            Some(boot_ns) => {
                let hash = content_hash_hex(&payload.wasm);
                let fresh = !self.boot_registry.contains_key(&hash);
                if fresh {
                    let boot_mailbox = match self.spawn_boot_singleton(ctx, &module, boot_ns, &actors) {
                        Ok(id) => id,
                        Err(error) => return LoadResult::Err { error },
                    };
                    self.boot_registry.insert(hash.clone(), BootEntry { mailbox_id: boot_mailbox, refcount: 0 });
                }
                (Some(hash), fresh)
            }
            None => (None, false),
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
                // ADR-0147: if this load freshly spawned the module's boot
                // singleton (first sight of its content hash), the requested
                // actor never spawned, so nothing will ever refcount against
                // that boot. Tear it back down — the same cleanup
                // `release_boot_ref` performs at refcount zero (a detached
                // `DropComponent` to the boot trampoline, whose drop handler
                // vacates the mailbox's registrations, plus registry removal)
                // — rather than leaking the boot singleton and its
                // registrations. A load that only found an existing boot
                // leaves it untouched: its other actors still hold the
                // refcount.
                if boot_freshly_inserted
                    && let Some(hash) = &boot_hash
                    && let Some(entry) = self.boot_registry.remove(hash)
                {
                    let bytes = DropComponent { mailbox_id: entry.mailbox_id }.encode_into_bytes();
                    let _ = ctx.send_envelope_detached(entry.mailbox_id, DropComponent::ID, &bytes);
                }
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
    fn spawn_boot_singleton<M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, M>,
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
    /// down through the same cleanup any component drop takes (the trampoline's
    /// drop handler vacates the mailbox's registrations, ADR-0079 §8 amended)
    /// — then forgets the registry entry.
    /// A no-op for an actor from a bootless module (nothing was tracked). The
    /// teardown send is detached, not a `forward_to_trampoline`: the boot's
    /// `DropResult` must not route back to whoever originated the external drop,
    /// so it starts a fresh chain and its reply lands harmlessly at the cap. It
    /// is also sent straight to the boot trampoline rather than self-routed
    /// through `on_drop_component`, whose ADR-0147 guard rejects boot mailboxes —
    /// this internal path is exactly how the boot is *legitimately* dropped.
    pub fn release_boot_ref<M: ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, M>, actor_mailbox: MailboxId) {
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
    ///
    /// Returns `Err` only when the new module's boot singleton failed to spawn
    /// (the old refcount was already decremented by then); the caller logs it
    /// rather than swallowing it. Malformed / bootless replacement wasm is a
    /// clean `Ok(())` no-op — the forwarded replace surfaces any wasm error, and
    /// this must run only after that replace has actually succeeded (ADR-0147),
    /// so a valid `Ok` swap always presents valid wasm here.
    pub fn rebind_boot_ref<M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, M>,
        actor_mailbox: MailboxId,
        new_wasm: &[u8],
    ) -> Result<(), String> {
        // Resolve + validate the replacement's boot slot up front, before
        // touching the old refcount. On any parse/compile error leave all boot
        // bookkeeping untouched (an `Ok(())` no-op) — the forwarded replace
        // surfaces the wasm error to the caller; a replace onto malformed wasm
        // must not tear the old boot down. The spawn decision is deferred to
        // after the decrement (a same-content replace may drop the shared entry
        // to zero and back).
        let new_boot: Option<(String, String, Vec<ActorInputs>, Module)> =
            match kind_manifest::read_boot_namespace_from_bytes(new_wasm) {
                Ok(Some(boot_ns)) => {
                    match (kind_manifest::read_actor_inputs_from_bytes(new_wasm), Module::new(&self.engine, new_wasm)) {
                        (Ok(actors), Ok(module)) => Some((boot_ns, content_hash_hex(new_wasm), actors, module)),
                        _ => return Ok(()),
                    }
                }
                Ok(None) => None,
                Err(_) => return Ok(()),
            };

        // Decrement the old module refcount (drop semantics), tearing its boot
        // down if this was its last non-boot actor.
        self.release_boot_ref(ctx, actor_mailbox);

        // Increment the new module refcount (load semantics), spawning its boot
        // singleton if the content is not (or is no longer) resident.
        if let Some((boot_ns, hash, actors, module)) = new_boot {
            if !self.boot_registry.contains_key(&hash) {
                // Propagate a boot spawn failure rather than discarding it: the
                // old refcount is already decremented, so the new boot is missing
                // and the caller must know. Announce the freshly-spawned boot
                // mailbox on success so `ListComponents` and the hub see it (a
                // load egresses this; a replace otherwise would not).
                let boot_mailbox = self.spawn_boot_singleton(ctx, &module, &boot_ns, &actors)?;
                self.boot_registry.insert(hash.clone(), BootEntry { mailbox_id: boot_mailbox, refcount: 0 });
                self.outbound.egress_mailboxes_changed(self.registry.list_mailbox_descriptors());
            }
            if let Some(entry) = self.boot_registry.get_mut(&hash) {
                entry.refcount += 1;
            }
            self.boot_hash_by_actor.insert(actor_mailbox, hash);
        }
        Ok(())
    }

    /// ADR-0147 (finding: refcount desync on failed replace): begin an
    /// `aether.component.replace` by forwarding the [`ReplaceComponent`] to the
    /// target trampoline, but route its `ReplaceResult` reply back to this cap
    /// (default `reply_to`) rather than straight to the caller, and park the
    /// caller's reply target plus the replacement wasm under the forward's
    /// correlation id. The boot-refcount transfer is deferred to
    /// [`Self::finish_replace`], which commits it only if the swap actually
    /// succeeded. The tracked forward keeps the caller's call open across the
    /// hop (its `in_flight` count is bumped), and `finish_replace` settles it
    /// with the trampoline's verdict. Mirrors the audio cap's fs-read
    /// forward/intercept (ADR-0103 §2).
    pub fn begin_replace(&mut self, ctx: &mut NativeCtx<'_>, payload: ReplaceComponent) {
        let source = ctx.reply_target();
        let actor_mailbox = payload.mailbox_id;
        let bytes = payload.encode_into_bytes();
        let mail_id = ctx.send_envelope_tracked(actor_mailbox, ReplaceComponent::ID, &bytes);
        self.pending_replace
            .insert(mail_id.correlation_id, PendingReplace { source, actor_mailbox, new_wasm: payload.wasm });
    }

    /// ADR-0147: settle a forwarded replace once the trampoline's
    /// `ReplaceResult` lands back at the cap. Recovers the parked
    /// [`PendingReplace`] by the reply's correlation id, commits the
    /// boot-refcount transfer via [`Self::rebind_boot_ref`] only on a successful
    /// swap (on failure the trampoline still hosts the old module, so the old
    /// boot must stay and no new boot is spawned — the refcount is left exactly
    /// as it was), then re-replies the verdict to the original caller so its
    /// `replace` call settles.
    pub fn finish_replace(&mut self, ctx: &mut NativeCtx<'_, Manual>, result: ReplaceResult) {
        let Some(correlation) = ctx.in_reply_to().map(|request| request.0) else {
            return;
        };
        let Some(pending) = self.pending_replace.remove(&correlation) else {
            return;
        };
        // `&&` short-circuits: `rebind_boot_ref` runs only on a successful swap
        // (never on a failed replace, where the old module is still hosted), and
        // its `Err` — a boot spawn failure after the old refcount was already
        // decremented — is logged rather than swallowed (ADR-0147).
        if matches!(result, ReplaceResult::Ok { .. })
            && let Err(error) = self.rebind_boot_ref(ctx, pending.actor_mailbox, &pending.new_wasm)
        {
            tracing::warn!(
                target: "aether_capabilities::component",
                actor = %pending.actor_mailbox,
                %error,
                "ADR-0147: replace succeeded but the new module's boot singleton failed to spawn",
            );
        }
        ctx.reply_to(pending.source, &result);
    }
}
