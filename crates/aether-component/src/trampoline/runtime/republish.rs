//! One member's side of a republish (ADR-0241 §7): prepare a candidate beside
//! the running guest, then commit it or abort it. While prepared, mail for the
//! guest waits at the inbox gate (`forward_to_wasm`) and nothing the candidate
//! sends leaves.

use std::collections::{HashSet, VecDeque};
use std::fmt::Display;
use std::mem;
use std::sync::Arc;

use aether_data::{ActorId, Blob};
use aether_kinds::ComponentCapabilities;
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::actor::wasm::asset_manifest;
use aether_substrate::actor::wasm::component::{Component, HookFault, StateBundle};
use aether_substrate::actor::wasm::module::Module;
use aether_substrate::mail::KindId;

use crate::component::Prepared;
use crate::trampoline::WasmTrampoline;

use super::contract;
use super::state::{PreparedSlot, Slot, WasmTrampolineState};

/// What a candidate is built as: its module, its actor-type tag, and the
/// receive surface it registers on commit.
pub(super) struct CandidateType {
    pub(super) module: Module,
    pub(super) type_tag: Option<u64>,
    pub(super) capabilities: ComponentCapabilities,
}

impl WasmTrampolineState {
    /// The candidate a republish builds from `module`: the group of the type
    /// this trampoline hosts, which a republish never changes (ADR-0241 §4),
    /// with its actor-type tag and its receive surface. The hosted type is
    /// the one the resident module's tag names, or the resident module's
    /// first exported type for a guest whose load named no type (its
    /// module's sole export). The candidate's tag names the type explicitly,
    /// so a successor that adds an export ahead of it still instantiates the
    /// hosted type.
    pub(super) fn candidate_type(&self, module: Module) -> Result<CandidateType, String> {
        let resident = self.module.manifest();
        let hosted = self
            .type_tag
            .map_or_else(
                || resident.exported_groups().next(),
                |tag| resident.exported_groups().find(|(namespace, _)| ActorId::singleton(namespace).0 == tag),
            )
            .map(|(namespace, _)| namespace.to_owned());
        let manifest = module.manifest();
        let group = hosted
            .as_deref()
            .map_or_else(
                || manifest.actors().first(),
                |hosted| manifest.exported_groups().find(|(namespace, _)| *namespace == hosted).map(|(_, group)| group),
            )
            .ok_or_else(|| {
                format!("the module does not export the hosted type {}", hosted.as_deref().unwrap_or("?"))
            })?;

        let type_tag = group.namespace.as_deref().map(|namespace| ActorId::singleton(namespace).0);
        let mut capabilities = group.capabilities.clone();
        capabilities.assets = manifest.asset_catalog().to_vec();
        Ok(CandidateType { module, type_tag, capabilities })
    }

    /// Build a candidate of `candidate` beside the running guest and hold it
    /// for a commit or an abort. `code` is the bytes the candidate's module
    /// was checked in from, which its load window reads assets from until
    /// it closes; `config` is the candidate's init config, or `None` for the
    /// stored one; `target` names this actor in a refusal.
    ///
    /// The candidate instantiates first, with its outbox held, while the
    /// running guest is still wired: `init` cannot send mail, so a failed
    /// `init` drops the candidate before the running guest runs any hook
    /// (#6134). The running guest then runs `unwire` and `on_dehydrate`, its
    /// correlation cursor, reply table and watches move to the candidate, and
    /// the candidate rehydrates. A refusal after the hooks, either guest's
    /// replace hook returning an error among them (ADR-0249 §1), reinstates
    /// the running guest with the state it saved. A slot that is not live has
    /// nothing to prepare and refuses.
    pub(super) fn prepare(
        &mut self,
        ctx: &mut NativeCtx<'_, WasmTrampoline>,
        target: &impl Display,
        candidate: CandidateType,
        code: Blob,
        config: Option<Vec<u8>>,
    ) -> Prepared {
        let mut old = match mem::replace(&mut self.slot, Slot::Released) {
            Slot::Live(old) => *old,
            other => {
                let error = match &other {
                    Slot::Prepared(_) => "a republish is already prepared",
                    _ => "the guest is released",
                };
                self.slot = other;
                return Prepared::Refused { error: format!("{target}: {error}") };
            }
        };
        let CandidateType { module, type_tag, capabilities } = candidate;
        let config = config.unwrap_or_else(|| self.config.clone());

        let mut new_component = match self.instantiate(ctx, &module, code, &config, type_tag) {
            Ok(component) => component,
            Err(error) => {
                self.slot = Slot::Live(Box::new(old));
                return Prepared::Refused { error };
            }
        };

        let (saved, started) =
            self.start_candidate(ctx, target, &mut old, &mut new_component, module.manifest().kind_ids());
        match started {
            Ok(()) => {
                self.slot = Slot::Prepared(Box::new(PreparedSlot {
                    old,
                    saved,
                    candidate: new_component,
                    module,
                    type_tag,
                    capabilities,
                    config,
                    gated: VecDeque::new(),
                }));
                Prepared::Ready
            }
            Err(error) => {
                self.reinstate(ctx, old, new_component, saved, VecDeque::new());
                Prepared::Refused { error }
            }
        }
    }

    /// Install the prepared candidate. Its held mail is sent on this turn's
    /// chain, so the chain settles only after that mail does, and its staged
    /// aliases publish. The module, hosted type, receive surface and config
    /// become the candidate's, the kept guest drops, and the mail the gate
    /// queued is delivered to the candidate in order.
    ///
    /// A commit with nothing prepared is a host bug and aborts the substrate
    /// (ADR-0063).
    pub(super) fn commit(&mut self, ctx: &mut NativeCtx<'_, WasmTrampoline>) {
        let prepared = match mem::replace(&mut self.slot, Slot::Released) {
            Slot::Prepared(prepared) => *prepared,
            other => {
                self.slot = other;
                ctx.fatal_abort(format!("component {} committed a republish it never prepared", ctx.path()));
            }
        };
        let PreparedSlot { old, saved: _, mut candidate, module, type_tag, capabilities, config, gated } = prepared;

        candidate.flush_held_outbox(ctx);
        Self::stage_inline_aliases(ctx, candidate.drain_pending_aliases());
        Self::stage_inline_alias_retirements(ctx, candidate.drain_pending_alias_retirements());

        // The retired guest drops here: the `Component`'s own `Drop` releases
        // its wasm store.
        drop(old);
        self.module = module;
        // ADR-0096: track the actor type this trampoline now hosts, so a
        // later republish with no explicit type override reuses the
        // *current* type rather than reverting to the original load's.
        self.type_tag = type_tag;
        self.capabilities = capabilities;
        self.config = config;
        self.slot = Slot::Live(Box::new(candidate));

        // Sync the committed declaration. iamacoffeepot/aether#1037: the
        // mailbox id is stable across replace (ADR-0022 §4), so the accept set
        // is overwritten in place and the validator sees it immediately.
        // iamacoffeepot/aether#1128: the cost cells re-seed into both indexes,
        // reusing the prior cell for an unchanged kind (keeping its
        // accumulated EWMA) and adding a neutral cell for a new one; this
        // handler runs on the trampoline's own dispatch thread inside
        // `with_stamped`, so the per-actor cache stays exact across replace.
        // The trampoline's own framework arms ride along
        // (iamacoffeepot/aether#4269), so this very handler's cost stays
        // measured.
        ctx.sync_guest(self);
        self.release_gated(ctx, gated);
    }

    /// Discard the prepared candidate and reinstate the kept guest with the
    /// state it saved (see [`Self::reinstate`]). A slot with nothing prepared
    /// has nothing to abort.
    pub(super) fn abort(&mut self, ctx: &mut NativeCtx<'_, WasmTrampoline>) {
        match mem::replace(&mut self.slot, Slot::Released) {
            Slot::Prepared(prepared) => {
                let PreparedSlot { old, candidate, saved, gated, .. } = *prepared;
                self.reinstate(ctx, old, candidate, saved, gated);
            }
            other => self.slot = other,
        }
    }

    /// Instantiate a candidate of `module` against this trampoline's binding,
    /// with its outbox held from before `init`. The binding carries the
    /// mailbox id, so it is preserved across replace per ADR-0022 §4.
    fn instantiate(
        &self,
        ctx: &NativeCtx<'_, WasmTrampoline>,
        module: &Module,
        code: Blob,
        config: &[u8],
        type_tag: Option<u64>,
    ) -> Result<Component, String> {
        let mut substrate_ctx = ctx.guest_ctx(Arc::clone(&self.outbound));
        // ADR-0241 §7: nothing the candidate sends leaves before commit.
        substrate_ctx.hold_outbox();
        // ADR-0163 §3 (#3984): install the load window over the republish's
        // code before instantiate so the candidate's `init` can pull assets;
        // closed, letting go of the code, once it rehydrated (a republish
        // re-runs `init`, not `wire`).
        substrate_ctx.install_load_window(asset_manifest::LoadWindow::open(module, Some(code)));
        // ADR-0231 §4: an inline child the candidate spawns publishes its
        // own namespace and rows, read from the candidate's module.
        substrate_ctx.install_inline_children(contract::inline_children(module.manifest()));

        // ADR-0090 (issue 1257): the config bytes reach the candidate's typed
        // `init` the way the load path's do. Empty means "no config"; a
        // typed-config guest decodes its `Self::Config` from these bytes.
        Component::instantiate(&self.engine, &self.linker, module.compiled(), substrate_ctx, config, type_tag)
            .map_err(|e| format!("wasm instantiation failed: {e}"))
    }

    /// Retire `old` into `candidate`: run `unwire` then `on_dehydrate` on the
    /// old guest, move its correlation cursor and reply table to the
    /// candidate, and rehydrate the candidate from the old guest's saved
    /// bundle. Issue 584 Phase 2b: `unwire` fires first so the old guest can
    /// announce its retirement before the swap. An `on_dehydrate` that
    /// returned an error, a save the host refused, a carried context the
    /// replacement does not declare (#6429), or a failed rehydrate refuses;
    /// the caller then reinstates the old guest.
    ///
    /// A trap in the old guest's `on_dehydrate` aborts the substrate
    /// (ADR-0063, ADR-0249 §2): it is the live guest, which would have to
    /// keep running on a store left wherever the trap found it.
    ///
    /// Returns the bundle the old guest saved beside the outcome, on a
    /// refusal too, so a reinstated old guest gets it back (issue 7125). An
    /// `on_dehydrate` that returned an error still saved what its hooks
    /// captured; a `save_state` the host rejected deposited none.
    fn start_candidate(
        &self,
        ctx: &NativeCtx<'_, WasmTrampoline>,
        target: &impl Display,
        old: &mut Component,
        candidate: &mut Component,
        replacement: &HashSet<KindId>,
    ) -> (Option<StateBundle>, Result<(), String>) {
        old.unwire();
        let dehydrated = match old.on_dehydrate() {
            Ok(()) => Ok(()),
            Err(HookFault::Returned(error)) => Err(error),
            Err(HookFault::Trapped(trap)) => {
                ctx.fatal_abort(format!("component {} trapped in on_dehydrate: {trap:#}", ctx.path()))
            }
        };
        let saved = old.take_saved_state();
        Self::hand_over(old, candidate);

        // The host's own record of a save it refused comes first: it stands
        // whether or not the hook passed the save's error on.
        let started = match (old.take_save_error(), dehydrated) {
            (Some(error), _) => Err(error),
            (None, Err(error)) => Err(format!("on_dehydrate failed: {error}")),
            (None, Ok(())) => self.rehydrate_candidate(ctx, target, candidate, saved.as_ref(), replacement),
        };
        (saved, started)
    }

    /// Move what belongs to the mailbox from the old guest to the candidate,
    /// after the old guest's hooks and before the candidate's `on_rehydrate`
    /// and every delivery. It moves on a refusal too: the reinstatement moves
    /// it back.
    fn hand_over(old: &mut Component, candidate: &mut Component) {
        // ADR-0139 §3 (#6400, #6422, #6409): after `unwire`, which may still
        // send or answer handles, and `on_dehydrate`, the candidate continues
        // the mailbox's correlation and reply-lineage sequences and takes
        // over the reply table, so it reuses neither a request id nor a reply
        // `MailId` and answers each held handle to its own requester.
        candidate.resume_correlations(old.correlation_cursor());
        candidate.resume_replies(old.take_pending_replies());
        // ADR-0079 §8: the old guest's watches move with the mailbox, after
        // its `unwire` had the chance to end any. Nothing is registered or
        // released: a registration is keyed by the watcher's mailbox, which a
        // republish does not change. Their contexts reach the candidate in
        // the saved bundle's request-context table, restored before its
        // `on_rehydrate` runs.
        candidate.resume_watches(old.take_watches());
        candidate.close_load_window();
    }

    /// The part of [`Self::start_candidate`] after the hand-over: check the
    /// contexts `saved` carries, then rehydrate the candidate from it. A
    /// candidate whose `on_rehydrate` returned an error or trapped refuses
    /// the same way (ADR-0249 §2): it is dropped either way.
    fn rehydrate_candidate(
        &self,
        ctx: &NativeCtx<'_, WasmTrampoline>,
        target: &impl Display,
        candidate: &mut Component,
        saved: Option<&StateBundle>,
        replacement: &HashSet<KindId>,
    ) -> Result<(), String> {
        // #6429: the carried contexts are checked against the replacement's
        // kind vocabulary once the old guest's `on_dehydrate` surfaced them.
        self.check_carried_contexts(ctx, target, saved, replacement)?;

        // ADR-0016 §4: a failed rehydrate refuses the prepare.
        saved.map_or(Ok(()), |bundle| {
            candidate.call_on_rehydrate(bundle).map_err(|fault| format!("on_rehydrate failed: {fault}"))
        })
    }

    /// ADR-0139 §4 (#6429): every context the old instance carries in its
    /// saved bundle, a request's or a watch's (ADR-0079 §8), must have a kind
    /// the replacement module declares.
    /// Only kinds the predecessor module declares are judged: a context kind
    /// defined in a shared kinds crate may be missing from both sections, and
    /// the host has no record to judge it by. The contexts come out of
    /// `on_dehydrate`, so no admission preview can see them: the check runs
    /// here, and its refusal refuses the prepare.
    fn check_carried_contexts(
        &self,
        ctx: &NativeCtx<'_, WasmTrampoline>,
        target: &impl Display,
        saved: Option<&StateBundle>,
        replacement: &HashSet<KindId>,
    ) -> Result<(), String> {
        let Some(bundle) = saved else {
            return Ok(());
        };
        let (table, _, _) = aether_actor::split_state_envelope(bundle.version, &bundle.bytes);
        if table.is_empty() {
            return Ok(());
        }

        contract::undeclared_context(table.kinds(), self.module.manifest().kind_ids(), replacement)
            .map_or(Ok(()), |kind| Err(contract::context_refusal(target, &ctx.kind_label(kind))))
    }
}
