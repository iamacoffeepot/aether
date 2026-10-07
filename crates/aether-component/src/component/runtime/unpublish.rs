//! Withdraw one published namespace (ADR-0250 §5).
//!
//! An `Unpublish` names a single row of the publication table. The host
//! refuses it while the namespace is unpublishing or republishing, is native,
//! is unpublished, has a load or a first publish in flight, or still has a
//! live instance, then stages one owner batch carrying the hash it saw, so a
//! concurrent republish cannot be withdrawn under the caller. The commit
//! answers `Ok` once the row is gone; a publish that waited on the namespace
//! then runs as a first publish.

use aether_actor::ReplyMode;
use aether_data::name_inventory::native_type_entries;
use aether_data::{ErasedActorPath, MailboxCategory};
use aether_kinds::{Unpublish, UnpublishResult};
use aether_substrate::actor::native::{Held, RegistryBatch, RegistryBatchResult};

use super::{ComponentHostCapabilityState, HostCtx};
use crate::kinds::Unpublished;

/// An `Unpublish` whose withdrawal batch is staged, from staging until the
/// owner answers it: its held reply and the namespace the batch withdraws.
pub(super) struct UnpublishInFlight {
    held: Held<UnpublishResult>,
    namespace: String,
}

impl UnpublishInFlight {
    /// Whether the staged withdrawal names `namespace`, which a publish of
    /// another module waits on until it settles.
    pub(super) fn publishes(&self, namespace: &str) -> bool {
        self.namespace.as_str() == namespace
    }
}

impl ComponentHostCapabilityState {
    /// Withdraw the publication of `payload`'s namespace, or refuse it. The
    /// refusals run in order: an unpublish or republish already holding it, a
    /// native namespace, a namespace no module publishes, a load or staged
    /// first-publish of it in flight, and a live instance of it still running.
    pub(super) fn begin_unpublish<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        held: Held<UnpublishResult>,
        payload: Unpublish,
    ) {
        let Unpublish { namespace } = payload;
        let unpublishing = self.unpublishes.values().any(|unpublish| unpublish.publishes(&namespace));
        if unpublishing {
            let error = format!("{namespace} is already unpublishing");
            held.answer(ctx, &UnpublishResult::Err { error });
            return;
        }
        if self.republishing.contains_key(&namespace) {
            let error = format!("{namespace} is already republishing: one republish of a module runs at a time");
            held.answer(ctx, &UnpublishResult::Err { error });
            return;
        }
        let native = native_type_entries().any(|entry| entry.namespace == namespace);
        if native {
            let error = format!(
                "{namespace} is published by a native actor linked into this engine, which is never unpublished"
            );
            held.answer(ctx, &UnpublishResult::Err { error });
            return;
        }
        let Some(module) = ctx.published_module(&namespace) else {
            let error = format!("no module publishes {namespace}, so there is nothing to withdraw");
            held.answer(ctx, &UnpublishResult::Err { error });
            return;
        };
        let loading = self.loads.values().any(|load| load.published() == namespace);
        let staged = self.publishes.values().any(|publish| publish.publishes(&namespace));
        let publishing = loading || staged;
        if publishing {
            let error = format!("{namespace} has a publish in flight; unpublish once it settles");
            held.answer(ctx, &UnpublishResult::Err { error });
            return;
        }
        let live = self.live_paths(ctx, &namespace);
        if !live.is_empty() {
            let error = format!("{namespace} still has live instances: {}; drop them first", live.join(", "));
            held.answer(ctx, &UnpublishResult::Err { error });
            return;
        }
        let id = self.next_unpublish;
        self.next_unpublish =
            self.next_unpublish.checked_add(1).expect("the component host's unpublish ids cannot overflow");
        let hash = module.hash();
        self.unpublishes.insert(id, UnpublishInFlight { held, namespace: namespace.clone() });
        ctx.stage_registry_batch(RegistryBatch::unpublish_namespace(&namespace, hash), Unpublished { unpublish: id });
    }

    /// An `Unpublish`'s withdrawal batch settled. On commit the namespace
    /// leaves the boot set, so a later republish of it is checked as a first
    /// publish, the held reply answers, and queued publishes run.
    pub(super) fn finish_unpublish<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        unpublish: u64,
        unpublished: RegistryBatchResult,
    ) {
        let UnpublishInFlight { held, namespace } =
            self.unpublishes.remove(&unpublish).expect("a staged unpublish waits in state until it settles");
        match unpublished {
            Ok(()) => {
                self.boot_namespaces.remove(&namespace);
                held.answer(ctx, &UnpublishResult::Ok { namespace });
            }
            Err(error) => {
                held.answer(ctx, &UnpublishResult::Err { error: format!("module unpublish refused: {error}") });
            }
        }
        self.release_queued_publishes(ctx);
    }

    /// Every live instance of `namespace`, by path, in path order so refusals
    /// read the same on every run: the host's loaded guests plus the inline
    /// children beneath a parent the host never loaded, read from the registry
    /// inventory the way the republish pre-checks read them.
    fn live_paths<M: ReplyMode>(&self, ctx: &HostCtx<'_, M>, namespace: &str) -> Vec<String> {
        let mut live: Vec<String> = self
            .drop_targets
            .iter()
            .filter(|(_, guest)| guest.namespace.as_str() == namespace)
            .map(|(actor, _)| ctx.actor_path(*actor).to_string())
            .collect();
        live.extend(
            self.subscription()
                .inventory()
                .mailboxes
                .into_iter()
                .filter(|mailbox| mailbox.category == Some(MailboxCategory::Trampoline))
                .filter(|mailbox| names_instance_of(&mailbox.name, namespace))
                .map(|mailbox| mailbox.name),
        );
        live.sort();
        live.dedup();
        live
    }
}

/// Whether the inventory name `name` is the path of an instance of
/// `namespace`. A name that is no actor path names no instance.
fn names_instance_of(name: &str, namespace: &str) -> bool {
    ErasedActorPath::new(name).is_ok_and(|path| path.leaf_namespace() == namespace)
}
