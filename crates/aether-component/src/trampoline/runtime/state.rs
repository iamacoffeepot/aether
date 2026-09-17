use std::collections::VecDeque;
use std::sync::Arc;

use aether_actor::Single;
use aether_kinds::ReplaceResult;
use aether_substrate::actor::native::{NativeBinding, NativeCtx};
use aether_substrate::actor::wasm::component::{Component, ComponentCtx};
use aether_substrate::actor::wasm::kind_manifest::ActorInputs;
use aether_substrate::chassis::inbox::InboundMail;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use aether_substrate::mail::{Mail, MailboxId};
use wasmtime::{Engine, Linker, Module};

use crate::ComponentRestrictions;
use crate::trampoline::WasmTrampoline;

use super::admission::{
    CancelSlot, CommitSlot, EvaluateResident, PrepareSlot, ResidentDelivered, SlotAttempt, SlotCancelled,
    SlotCommitted, SlotPrepared,
};
use super::replace::PreparedReplacement;

pub enum PendingPhase {
    Prepared,
    ResidentDelivered,
    ResidentFailed,
}

pub struct PendingReplacement {
    pub attempt: SlotAttempt,
    pub prepared: PreparedReplacement,
    pub phase: PendingPhase,
    pub held_mail: VecDeque<InboundMail>,
}

/// Per-component trampoline **runtime state** (ADR-0122 identity/runtime
/// split — the addressing identity is the distinct ZST
/// [`WasmTrampoline`](crate::trampoline::WasmTrampoline)). Holds the wasm
/// `Component` optionally — `None` means the wasm has been unloaded by
/// `DropComponent` but the trampoline (and its mailbox name) is
/// still alive, ready to be refilled by `ReplaceComponent` or
/// recycled by a future load. Distinction matters: dropping the
/// **component** is a wasm unload that preserves the addressable
/// name; dropping the **trampoline** would kill the actor and
/// tombstone the subname. The cap's `DropComponent` handler does
/// the former; the latter happens at substrate teardown.
pub struct WasmTrampolineState {
    /// Bootstrap policy for this slot, retained even when its guest is
    /// replaced or individually dropped.
    pub prohibit: ComponentRestrictions,
    pub admission_authority: Option<MailboxId>,
    pub pending_admission: Option<PendingReplacement>,
    pub last_attempt: Option<SlotAttempt>,
    pub last_cancelled: Option<SlotAttempt>,
    /// `Some` while wasm is loaded; `None` after a `DropComponent`.
    /// Mail arriving in the `None` state warn-drops via the
    /// fallback (the trampoline is just an empty named slot).
    pub component: Option<Component>,
    /// Held for [`Self::handle_replace`] so a fresh
    /// `Component::instantiate` against the same engine + linker
    /// is reachable from the handler.
    pub engine: Arc<Engine>,
    pub linker: Arc<Linker<ComponentCtx>>,
    pub registry: Arc<Registry>,
    pub mailer: Arc<Mailer>,
    pub outbound: Arc<HubOutbound>,
    /// The trampoline's own mailbox id — the registry's depth-1
    /// derivation over `full_name`. Cached because
    /// `NativeCtx` only exposes `self_id()` via the
    /// `NativeInitCtx` flavour today; storing it here avoids
    /// reaching into `ctx.binding().self_mailbox()` on every
    /// handler call.
    pub mailbox: MailboxId,
    /// ADR-0096: the selected export's actor-type tag, or `None`
    /// for the entry type. Held so [`Self::handle_replace`]
    /// re-instantiates the same exported type from the new wasm
    /// and re-reads that type's capability group.
    pub type_tag: Option<u64>,
    /// ADR-0097: the resident `Module`, retained so a sibling spawn
    /// re-instantiates it (a cheap `Arc` clone — wasmtime shares the
    /// compiled code) without a re-compile, and refreshed on replace.
    pub module: Module,
    /// ADR-0097: every exported type's capability group (see
    /// [`super::WasmTrampolineConfig::actor_caps`]). A spawned sibling looks
    /// up its own handler set here by actor-type tag.
    pub actor_caps: Vec<ActorInputs>,
    /// ADR-0163 §3 (#3984): the resident module's raw wasm bytes, retained
    /// so a `spawn_child::<Sibling>` from this module can index its own
    /// asset load window, and refreshed on replace. Shared `Arc` — indexed,
    /// never mutated.
    pub wasm_bytes: Arc<[u8]>,
}

impl WasmTrampolineState {
    pub fn has_pending_admission(&self) -> bool {
        self.pending_admission.is_some()
    }

    fn authorize_admission(&self, source: Option<MailboxId>) -> Result<(), String> {
        if self.admission_authority.is_some() && self.admission_authority == source {
            Ok(())
        } else {
            Err("slot admission sender is not the native authority".to_owned())
        }
    }

    pub fn prepare_slot(
        &mut self,
        source: Option<MailboxId>,
        binding: Arc<NativeBinding>,
        request: PrepareSlot,
    ) -> SlotPrepared {
        let attempt = request.attempt;
        let mut claimed = false;
        let prepared = (|| {
            self.authorize_admission(source)?;
            if self.pending_admission.is_some() {
                return Err("slot already has a pending replacement".to_owned());
            }
            if self.last_attempt.is_some_and(|last| attempt <= last) {
                return Err("slot admission attempt is stale".to_owned());
            }
            if request.replacement.mailbox_id != self.mailbox {
                return Err("slot admission targets a different trampoline".to_owned());
            }
            if self.component.is_none() {
                return Err("slot admission requires a resident predecessor".to_owned());
            }
            self.last_attempt = Some(attempt);
            claimed = true;
            let mut prepared = self.prepare_replace(binding, request.replacement)?;
            let warmup = Mail::new(self.mailbox, request.warmup_kind, request.warmup_bytes, 1);
            let ack_bytes = prepared.warm(warmup, request.ack_recipient, request.ack_kind, self.mailbox)?;
            self.pending_admission = Some(PendingReplacement {
                attempt,
                prepared,
                phase: PendingPhase::Prepared,
                held_mail: VecDeque::new(),
            });
            Ok(ack_bytes)
        })();
        match prepared {
            Ok(ack_bytes) => SlotPrepared::Ok { attempt, ack_bytes },
            Err(error) => {
                if claimed {
                    self.last_cancelled = Some(attempt);
                }
                SlotPrepared::Err { attempt, error }
            }
        }
    }

    pub fn evaluate_resident(
        &mut self,
        ctx: &mut NativeCtx<'_, Single, WasmTrampoline>,
        request: EvaluateResident,
    ) -> ResidentDelivered {
        let attempt = request.attempt;
        let result = (|| {
            self.authorize_admission(ctx.source_mailbox())?;
            let pending = self.pending_admission.as_mut().ok_or("slot has no pending replacement")?;
            if pending.attempt != attempt || !matches!(pending.phase, PendingPhase::Prepared) {
                return Err("slot evaluation attempt is stale or already delivered".to_owned());
            }
            pending.phase = PendingPhase::ResidentFailed;
            let event = Mail::new(self.mailbox, request.event_kind, request.event_bytes, 1);
            let status = self.deliver_guest(ctx, &event)?;
            if status != 0 {
                return Err(format!("resident event returned status {status}"));
            }
            if let Some(pending) = self.pending_admission.as_mut() {
                pending.phase = PendingPhase::ResidentDelivered;
            }
            Ok(())
        })();
        match result {
            Ok(()) => ResidentDelivered::Ok { attempt },
            Err(error) => ResidentDelivered::Err { attempt, error },
        }
    }

    pub fn commit_slot(
        &mut self,
        ctx: &mut NativeCtx<'_, Single, WasmTrampoline>,
        request: CommitSlot,
    ) -> SlotCommitted {
        let attempt = request.attempt;
        let result = (|| {
            self.authorize_admission(ctx.source_mailbox())?;
            let pending = self.pending_admission.as_ref().ok_or("slot has no pending replacement")?;
            if pending.attempt != attempt || !matches!(pending.phase, PendingPhase::ResidentDelivered) {
                return Err("slot commit attempt is stale or resident evaluation is incomplete".to_owned());
            }
            let pending = self.pending_admission.take().expect("checked pending replacement");
            let capabilities = match self.commit_replace(pending.prepared) {
                ReplaceResult::Ok { capabilities } => capabilities,
                ReplaceResult::Err { error } => ctx.fatal_abort(format!("committed slot failed to install: {error}")),
            };
            self.replay_held_mail(ctx, pending.held_mail);
            Ok(capabilities)
        })();
        match result {
            Ok(capabilities) => SlotCommitted::Ok { attempt, capabilities },
            Err(error) => SlotCommitted::Err { attempt, error },
        }
    }

    pub fn cancel_slot(
        &mut self,
        ctx: &mut NativeCtx<'_, Single, WasmTrampoline>,
        request: CancelSlot,
    ) -> SlotCancelled {
        let attempt = request.attempt;
        let result = (|| {
            self.authorize_admission(ctx.source_mailbox())?;
            let Some(pending) = self.pending_admission.as_ref() else {
                return if self.last_cancelled == Some(attempt) {
                    Ok(())
                } else {
                    Err("slot has no matching pending replacement".to_owned())
                };
            };
            if pending.attempt != attempt || !matches!(pending.phase, PendingPhase::Prepared) {
                return Err("slot cancellation attempt is stale or event is already delivered".to_owned());
            }
            let pending = self.pending_admission.take().expect("checked pending replacement");
            drop(pending.prepared);
            self.last_cancelled = Some(attempt);
            self.replay_held_mail(ctx, pending.held_mail);
            Ok(())
        })();
        match result {
            Ok(()) => SlotCancelled::Ok { attempt },
            Err(error) => SlotCancelled::Err { attempt, error },
        }
    }

    pub fn hold_guest_mail(&mut self, inbound: InboundMail) {
        if let Some(pending) = self.pending_admission.as_mut() {
            pending.held_mail.push_back(inbound);
        }
    }

    fn replay_held_mail(
        &mut self,
        ctx: &mut NativeCtx<'_, Single, WasmTrampoline>,
        mut held_mail: VecDeque<InboundMail>,
    ) {
        while let Some(inbound) = held_mail.pop_front() {
            if let Err(error) = self.deliver_envelope(ctx, inbound.envelope()) {
                ctx.fatal_abort(error);
            }
        }
    }

    pub fn discard_pending_on_shutdown(&mut self) {
        self.pending_admission = None;
    }
}
