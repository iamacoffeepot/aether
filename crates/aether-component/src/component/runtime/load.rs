//! Owner-staged component load, module-boot, and replacement continuations.

use std::sync::Arc;

use aether_actor::{Manual, OutboundReply, ReplyMode};
use aether_data::{Kind, KindDescriptor};
use aether_kinds::{ComponentCapabilities, DropComponent, LoadComponent, ReplaceComponent, ReplaceResult};
use wasmtime::Module;

use aether_substrate::actor::native::{
    NativeCtx, RegistryBatch, RegistryBatchResult, SpawnApplied, SpawnError, TaskContinuation, TaskDone, spawn::Subname,
};
use aether_substrate::actor::wasm::asset_manifest;
use aether_substrate::actor::wasm::kind_manifest::{self, ActorInputs};
use aether_substrate::mail::MailboxId;

use super::LoadResult;
use crate::component::ComponentHostCapability;
use crate::component::runtime::{BootEntry, ComponentHostCapabilityState, PendingReplace};
use crate::trampoline::{WasmTrampoline, WasmTrampolineConfig};

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

pub(super) struct PreparedLoad {
    capabilities: ComponentCapabilities,
    type_tag: Option<u64>,
    actors: Vec<ActorInputs>,
    boot_namespace: Option<String>,
    module: Module,
    wasm_bytes: Arc<[u8]>,
    config: Vec<u8>,
    name: String,
}

impl PreparedLoad {
    fn requested_config(&self, state: &ComponentHostCapabilityState) -> WasmTrampolineConfig {
        WasmTrampolineConfig {
            engine: Arc::clone(&state.engine),
            linker: Arc::clone(&state.linker),
            module: self.module.clone(),
            registry: Arc::clone(&state.registry),
            outbound: Arc::clone(&state.outbound),
            capabilities: self.capabilities.clone(),
            config: self.config.clone(),
            type_tag: self.type_tag,
            actor_caps: self.actors.clone(),
            wasm_bytes: Arc::clone(&self.wasm_bytes),
        }
    }

    fn boot_plan(&self) -> Option<PreparedBoot> {
        let namespace = self.boot_namespace.clone()?;
        Some(PreparedBoot::new(namespace, self.module.clone(), self.actors.clone(), Arc::clone(&self.wasm_bytes)))
    }
}

#[derive(Clone)]
pub(super) struct PreparedBoot {
    hash: String,
    namespace: String,
    capabilities: ComponentCapabilities,
    module: Module,
    actors: Vec<ActorInputs>,
    wasm_bytes: Arc<[u8]>,
}

impl PreparedBoot {
    fn new(namespace: String, module: Module, actors: Vec<ActorInputs>, wasm_bytes: Arc<[u8]>) -> Self {
        let capabilities = actors
            .iter()
            .find(|actor| actor.namespace.as_deref() == Some(namespace.as_str()))
            .map(|actor| actor.capabilities.clone())
            .unwrap_or_default();
        let hash = content_hash_hex(&wasm_bytes);
        Self { hash, namespace, capabilities, module, actors, wasm_bytes }
    }

    #[allow(clippy::disallowed_methods)]
    fn config(&self, state: &ComponentHostCapabilityState) -> WasmTrampolineConfig {
        WasmTrampolineConfig {
            engine: Arc::clone(&state.engine),
            linker: Arc::clone(&state.linker),
            module: self.module.clone(),
            registry: Arc::clone(&state.registry),
            outbound: Arc::clone(&state.outbound),
            capabilities: self.capabilities.clone(),
            config: Vec::new(),
            type_tag: Some(aether_data::mailbox_id_from_name(&self.namespace).0),
            actor_caps: self.actors.clone(),
            wasm_bytes: Arc::clone(&self.wasm_bytes),
        }
    }
}

#[derive(Clone)]
pub(super) struct KindRegistration {
    load: Arc<PreparedLoad>,
}

#[derive(Clone)]
pub(super) enum BootSuccessor {
    Load(Arc<PreparedLoad>),
    Replacement { pending: PendingReplace, result: ReplaceResult },
}

struct BootWaiter {
    continuation: TaskContinuation,
    successor: BootSuccessor,
}

pub(super) struct PendingBoot {
    waiters: Vec<BootWaiter>,
}

impl PendingBoot {
    fn new() -> Self {
        Self { waiters: Vec::new() }
    }
}

impl Drop for PendingBoot {
    fn drop(&mut self) {
        for waiter in self.waiters.drain(..) {
            waiter.continuation.abandon_for_actor_close();
        }
    }
}

#[derive(Clone)]
pub(super) enum SpawnContinuation {
    ModuleBoot { plan: Box<PreparedBoot>, first: Box<BootSuccessor> },
    RequestedActor { load: Arc<PreparedLoad>, boot_hash: Option<String> },
}

impl ComponentHostCapabilityState {
    pub fn begin_load(&mut self, ctx: &mut NativeCtx<'_, Manual>, payload: LoadComponent) {
        let (descriptors, load) = match self.prepare_load(payload) {
            Ok(prepared) => prepared,
            Err(result) => {
                ctx.reply(&result);
                return;
            }
        };
        let _ = ctx.stage_registry_batch(RegistryBatch::register_kinds(descriptors), KindRegistration { load });
    }

    #[allow(
        clippy::too_many_lines,
        clippy::result_large_err,
        reason = "cold synchronous preparation returns the exact public LoadResult error shape"
    )]
    fn prepare_load(&mut self, payload: LoadComponent) -> Result<(Vec<KindDescriptor>, Arc<PreparedLoad>), LoadResult> {
        let descriptors = kind_manifest::read_from_bytes(&payload.wasm).map_err(|error| LoadResult::Err { error })?;
        let actors =
            kind_manifest::read_actor_inputs_from_bytes(&payload.wasm).map_err(|error| LoadResult::Err { error })?;
        let boot_namespace =
            kind_manifest::read_boot_namespace_from_bytes(&payload.wasm).map_err(|error| LoadResult::Err { error })?;

        if let Some(boot_ns) = &boot_namespace
            && payload.export.as_deref() == Some(boot_ns.as_str())
        {
            return Err(LoadResult::Err {
                error: format!("export {boot_ns:?} names this module's boot actor, which is not selectable (ADR-0147)"),
            });
        }

        let (mut capabilities, type_tag, selected_namespace) = if let Some(requested) = &payload.export {
            let Some(group) = actors.iter().find(|actor| actor.namespace.as_deref() == Some(requested.as_str())) else {
                let available: Vec<&str> = actors.iter().filter_map(|actor| actor.namespace.as_deref()).collect();
                return Err(LoadResult::Err {
                    error: format!("export {requested:?} not found in module; exported types: {available:?}"),
                });
            };
            #[allow(clippy::disallowed_methods)]
            let tag = aether_data::mailbox_id_from_name(requested).0;
            (group.capabilities.clone(), Some(tag), Some(requested.clone()))
        } else if kind_manifest::read_no_default_marker(&payload.wasm) {
            let available: Vec<&str> = actors.iter().filter_map(|actor| actor.namespace.as_deref()).collect();
            return Err(LoadResult::Err {
                error: format!(
                    "module has no default (ADR-0138): load one of its exports by name via the export selector; exported types: {available:?}"
                ),
            });
        } else {
            let default_actor = boot_namespace.as_deref().map_or_else(
                || actors.first(),
                |boot_ns| actors.iter().find(|actor| actor.namespace.as_deref() != Some(boot_ns)),
            );
            (
                default_actor.map(|actor| actor.capabilities.clone()).unwrap_or_default(),
                None,
                default_actor.and_then(|actor| actor.namespace.clone()),
            )
        };

        let wasm_bytes: Arc<[u8]> = Arc::from(payload.wasm.as_slice());
        capabilities.assets = asset_manifest::read_assets_from_bytes(&wasm_bytes)
            .map_err(|error| LoadResult::Err { error })?
            .into_iter()
            .map(|record| record.info)
            .collect();
        let module = Module::new(&self.engine, &payload.wasm)
            .map_err(|error| LoadResult::Err { error: format!("invalid wasm module: {error}") })?;
        let name = match payload.name.or(selected_namespace) {
            Some(name) => name,
            None => match kind_manifest::read_namespace_from_bytes(&payload.wasm) {
                Ok(Some(declared)) => declared,
                Ok(None) => {
                    let counter = self.default_name_counter;
                    self.default_name_counter += 1;
                    format!("component_{counter}")
                }
                Err(error) => return Err(LoadResult::Err { error }),
            },
        };

        Ok((
            descriptors,
            Arc::new(PreparedLoad {
                capabilities,
                type_tag,
                actors,
                boot_namespace,
                module,
                wasm_bytes,
                config: payload.config,
                name,
            }),
        ))
    }

    pub(super) fn finish_kind_registration(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<RegistryBatchResult, KindRegistration>,
    ) {
        if let Err(error) = done.output() {
            let error = format!("kind registration failed: {error}");
            done.resolve_with(ctx, move |_, _| LoadResult::Err { error });
            return;
        }
        let load = Arc::clone(&done.context().load);
        self.continue_load(ctx, done.handoff(), load);
    }

    fn continue_load(&mut self, ctx: &mut NativeCtx<'_>, continuation: TaskContinuation, load: Arc<PreparedLoad>) {
        let Some(plan) = load.boot_plan() else {
            self.stage_requested_actor(ctx, continuation, load, None);
            return;
        };
        let hash = plan.hash.clone();
        if self.boot_registry.contains_key(&hash) {
            self.stage_requested_actor(ctx, continuation, load, Some(hash));
        } else if let Some(pending) = self.pending_boots.get_mut(&hash) {
            pending.waiters.push(BootWaiter { continuation, successor: BootSuccessor::Load(load) });
        } else {
            self.stage_module_boot(ctx, continuation, plan, BootSuccessor::Load(load));
        }
    }

    fn stage_module_boot<M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, M>,
        continuation: TaskContinuation,
        plan: PreparedBoot,
        first: BootSuccessor,
    ) {
        let hash = plan.hash.clone();
        let namespace = plan.namespace.clone();
        let config = plan.config(self);
        match ctx
            .spawn_child::<ComponentHostCapability, WasmTrampoline>(Subname::Named(&namespace), config, ())
            .continue_with(
                continuation,
                SpawnContinuation::ModuleBoot { plan: Box::new(plan), first: Box::new(first.clone()) },
            ) {
            Ok(_) => {
                let previous = self.pending_boots.insert(hash, PendingBoot::new());
                debug_assert!(previous.is_none(), "one actor-local reservation owns a module boot hash");
            }
            Err((error, continuation)) => {
                Self::resolve_boot_failure(
                    ctx,
                    continuation,
                    first,
                    format!("boot trampoline spawn failed: {error:?}"),
                );
            }
        }
    }

    fn stage_requested_actor(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        continuation: TaskContinuation,
        load: Arc<PreparedLoad>,
        boot_hash: Option<String>,
    ) {
        let config = load.requested_config(self);
        match ctx
            .spawn_child::<ComponentHostCapability, WasmTrampoline>(Subname::Named(&load.name), config, ())
            .continue_with(
                continuation,
                SpawnContinuation::RequestedActor { load: Arc::clone(&load), boot_hash: boot_hash.clone() },
            ) {
            Ok(_) => {
                if let Some(hash) = &boot_hash {
                    self.boot_registry
                        .get_mut(hash)
                        .expect("requested actor starts only after its boot is Live")
                        .pending_requests += 1;
                }
            }
            Err((error, continuation)) => {
                continuation.resolve(ctx, &LoadResult::Err { error: format!("trampoline spawn failed: {error:?}") });
            }
        }
    }

    pub(super) fn finish_spawn(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<Result<SpawnApplied, SpawnError>, SpawnContinuation>,
    ) {
        match done.context().clone() {
            SpawnContinuation::ModuleBoot { plan, first } => self.finish_module_boot(ctx, done, *plan, *first),
            SpawnContinuation::RequestedActor { load, boot_hash } => {
                self.finish_requested_actor(ctx, done, load, boot_hash);
            }
        }
    }

    fn finish_module_boot(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<Result<SpawnApplied, SpawnError>, SpawnContinuation>,
        plan: PreparedBoot,
        first: BootSuccessor,
    ) {
        let result = match done.output() {
            Ok(applied) => Ok(applied.clone()),
            Err(error) => Err(format!("{error:?}")),
        };
        let mut pending =
            self.pending_boots.remove(&plan.hash).expect("module boot retains its actor-local reservation");
        match result {
            Ok(applied) => {
                self.mailer.capability_registry().register(applied.mailbox_id, &plan.capabilities);
                self.boot_registry.insert(
                    plan.hash.clone(),
                    BootEntry { mailbox_id: applied.mailbox_id, refcount: 0, pending_requests: 0, orphanable: true },
                );
                self.finish_boot_successor(ctx, done.handoff(), first, &plan.hash);
                for waiter in pending.waiters.drain(..) {
                    self.finish_boot_successor(ctx, waiter.continuation, waiter.successor, &plan.hash);
                }
                self.drop_orphan_boot(ctx, &plan.hash);
            }
            Err(error) => {
                Self::resolve_boot_failure(ctx, done.handoff(), first, error.clone());
                for waiter in pending.waiters.drain(..) {
                    Self::resolve_boot_failure(ctx, waiter.continuation, waiter.successor, error.clone());
                }
            }
        }
    }

    fn finish_boot_successor(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        continuation: TaskContinuation,
        successor: BootSuccessor,
        hash: &str,
    ) {
        match successor {
            BootSuccessor::Load(load) => {
                self.stage_requested_actor(ctx, continuation, load, Some(hash.to_owned()));
            }
            BootSuccessor::Replacement { pending, result } => {
                self.commit_replacement_boot(ctx, pending.actor_mailbox, Some(hash.to_owned()));
                continuation.resolve(ctx, &result);
            }
        }
    }

    fn resolve_boot_failure<M: ReplyMode>(
        ctx: &mut NativeCtx<'_, M>,
        continuation: TaskContinuation,
        successor: BootSuccessor,
        error: String,
    ) {
        match successor {
            BootSuccessor::Load(_) => continuation.resolve(
                ctx,
                &LoadResult::Err { error: format!("module boot failed before requested actor: {error}") },
            ),
            BootSuccessor::Replacement { pending, result } => {
                tracing::warn!(
                    target: "aether_component",
                    actor = %pending.actor_mailbox,
                    %error,
                    "replace succeeded but the replacement module boot failed",
                );
                continuation.resolve(ctx, &result);
            }
        }
    }

    fn finish_requested_actor(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<Result<SpawnApplied, SpawnError>, SpawnContinuation>,
        load: Arc<PreparedLoad>,
        boot_hash: Option<String>,
    ) {
        match done.output() {
            Ok(applied) => {
                let applied = applied.clone();
                if let Some(hash) = &boot_hash {
                    let entry =
                        self.boot_registry.get_mut(hash).expect("requested actor's Live boot remains registered");
                    entry.pending_requests = entry.pending_requests.saturating_sub(1);
                    entry.refcount += 1;
                    entry.orphanable = false;
                    self.boot_hash_by_actor.insert(applied.mailbox_id, hash.clone());
                }
                self.mailer.capability_registry().register(applied.mailbox_id, &load.capabilities);
                let capabilities = load.capabilities.clone();
                done.resolve_with(ctx, move |_, _| LoadResult::Ok {
                    mailbox_id: applied.mailbox_id,
                    name: applied.canonical_name.to_string(),
                    capabilities,
                });
            }
            Err(error) => {
                let error = format!("trampoline spawn failed: {error:?}");
                if let Some(hash) = &boot_hash {
                    if let Some(entry) = self.boot_registry.get_mut(hash) {
                        entry.pending_requests = entry.pending_requests.saturating_sub(1);
                    }
                    self.drop_orphan_boot(ctx, hash);
                }
                done.resolve_with(ctx, move |_, _| LoadResult::Err { error });
            }
        }
    }

    fn drop_orphan_boot<M: ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, M>, hash: &str) {
        let removable = self
            .boot_registry
            .get(hash)
            .is_some_and(|entry| entry.orphanable && entry.refcount == 0 && entry.pending_requests == 0);
        if removable {
            let entry = self.boot_registry.remove(hash).expect("orphan boot remains present");
            let bytes = DropComponent { mailbox_id: entry.mailbox_id }.encode_into_bytes();
            let _ = ctx.send_envelope_detached(entry.mailbox_id, DropComponent::ID, &bytes);
        }
    }

    pub fn release_boot_ref<M: ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, M>, actor_mailbox: MailboxId) {
        let Some(hash) = self.boot_hash_by_actor.remove(&actor_mailbox) else {
            return;
        };
        let remove = if let Some(entry) = self.boot_registry.get_mut(&hash) {
            entry.refcount = entry.refcount.saturating_sub(1);
            entry.refcount == 0 && entry.pending_requests == 0
        } else {
            false
        };
        if remove {
            let entry = self.boot_registry.remove(&hash).expect("zero-ref boot remains present");
            let bytes = DropComponent { mailbox_id: entry.mailbox_id }.encode_into_bytes();
            let _ = ctx.send_envelope_detached(entry.mailbox_id, DropComponent::ID, &bytes);
        }
    }

    pub fn begin_replace(&mut self, ctx: &mut NativeCtx<'_>, payload: ReplaceComponent) {
        let source = ctx.reply_target();
        let actor_mailbox = payload.mailbox_id;
        let bytes = payload.encode_into_bytes();
        let mail_id = ctx.send_envelope_tracked(actor_mailbox, ReplaceComponent::ID, &bytes);
        self.pending_replace.insert(
            mail_id.correlation_id,
            PendingReplace { source, actor_mailbox, new_wasm: Arc::from(payload.wasm) },
        );
    }

    pub fn finish_replace(&mut self, ctx: &mut NativeCtx<'_, Manual>, result: ReplaceResult) {
        let Some(correlation) = ctx.in_reply_to().map(|request| request.0) else {
            return;
        };
        let Some(pending) = self.pending_replace.remove(&correlation) else {
            return;
        };
        if !matches!(result, ReplaceResult::Ok { .. }) {
            ctx.reply_to(pending.source, &result);
            return;
        }

        let plan = match self.prepare_replacement_boot(&pending.new_wasm) {
            Ok(plan) => plan,
            Err(error) => {
                tracing::warn!(target: "aether_component", actor = %pending.actor_mailbox, %error, "replacement boot metadata could not be prepared");
                ctx.reply_to(pending.source, &result);
                return;
            }
        };
        let new_hash = plan.as_ref().map(|plan| plan.hash.clone());
        if self.boot_hash_by_actor.get(&pending.actor_mailbox) == new_hash.as_ref() {
            ctx.reply_to(pending.source, &result);
            return;
        }
        let Some(plan) = plan else {
            self.commit_replacement_boot(ctx, pending.actor_mailbox, None);
            ctx.reply_to(pending.source, &result);
            return;
        };
        if self.boot_registry.contains_key(&plan.hash) {
            self.commit_replacement_boot(ctx, pending.actor_mailbox, Some(plan.hash));
            ctx.reply_to(pending.source, &result);
            return;
        }

        let continuation = ctx.continuation_to(pending.source);
        if let Some(inflight) = self.pending_boots.get_mut(&plan.hash) {
            inflight
                .waiters
                .push(BootWaiter { continuation, successor: BootSuccessor::Replacement { pending, result } });
        } else {
            self.stage_module_boot(ctx, continuation, plan, BootSuccessor::Replacement { pending, result });
        }
    }

    fn prepare_replacement_boot(&self, wasm: &[u8]) -> Result<Option<PreparedBoot>, String> {
        let Some(namespace) = kind_manifest::read_boot_namespace_from_bytes(wasm)? else {
            return Ok(None);
        };
        let actors = kind_manifest::read_actor_inputs_from_bytes(wasm)?;
        let module = Module::new(&self.engine, wasm).map_err(|error| format!("invalid wasm module: {error}"))?;
        Ok(Some(PreparedBoot::new(namespace, module, actors, Arc::from(wasm))))
    }

    fn commit_replacement_boot<M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, M>,
        actor_mailbox: MailboxId,
        new_hash: Option<String>,
    ) {
        self.release_boot_ref(ctx, actor_mailbox);
        if let Some(hash) = new_hash {
            if let Some(entry) = self.boot_registry.get_mut(&hash) {
                entry.refcount += 1;
                entry.orphanable = false;
            }
            self.boot_hash_by_actor.insert(actor_mailbox, hash);
        }
    }
}
