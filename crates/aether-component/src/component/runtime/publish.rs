//! A publish binds a module's namespaces to it (ADR-0241 §3, §9), and it is
//! the one path every door that brings code takes: `Publish` and a load.
//!
//! The pre-checks decide what a publish does ([`super::republish`]): a module
//! that already publishes every namespace it exports changes nothing, a
//! first publish binds it in one registry-owner batch, and a successor
//! republishes every live instance of its namespaces as one group (§7). What
//! waits on the publish is its [`Publisher`]: a `Publish` answers with the
//! bound namespaces, and a load spawns its guest. A first publish by
//! `Publish` also spawns the module's boot once (ADR-0147).

use std::sync::Arc;

use aether_actor::ReplyMode;
use aether_kinds::{ComponentCapabilities, LoadResult, Publish, PublishResult, PublishedType};
use aether_substrate::actor::native::{Held, RegistryBatch, RegistryBatchResult};
use aether_substrate::actor::wasm::module::Module;

use super::load::{PreparedLoad, Requester};
use super::republish::QueuedPublish;
use crate::component::runtime::{ComponentHostCapabilityState, HostCtx};
use crate::kinds::ModulePublished;

/// What waits on a publish, and how it hears the outcome.
pub(super) enum Publisher {
    Publish(Held<PublishResult>),
    /// A load, which spawns its prepared guest once the module is bound.
    Load {
        held: Held<LoadResult>,
        load: Arc<PreparedLoad>,
    },
}

impl Publisher {
    /// Answer that the publish was refused with `error`, and nothing was
    /// bound or moved.
    pub(super) fn refuse<M: ReplyMode>(self, ctx: &mut HostCtx<'_, M>, error: &str) {
        match self {
            Self::Publish(held) => held.answer(ctx, &PublishResult::Err { error: error.to_owned() }),
            Self::Load { held, .. } => held.answer(ctx, &LoadResult::Err { error: error.to_owned() }),
        }
    }
}

/// A `Publish`'s first publish of a module, from its staged batch until the
/// owner answers it.
pub(super) struct PublishInFlight {
    held: Held<PublishResult>,
    module: Module,
}

impl PublishInFlight {
    /// Whether the module publishes `namespace`, which a publish of another
    /// module waits on until this one settles.
    pub(super) fn publishes(&self, namespace: &str) -> bool {
        self.module.published_groups().any(|(published, _)| published == namespace)
    }
}

/// Each namespace `module` publishes with its type's receive surface, the
/// module's asset catalog included.
pub(super) fn published_surfaces(module: &Module) -> impl Iterator<Item = (String, ComponentCapabilities)> + '_ {
    let assets = module.manifest().asset_catalog();
    module.published_groups().map(move |(namespace, group)| {
        let mut capabilities = group.capabilities.clone();
        capabilities.assets = assets.to_vec();
        (namespace.into_owned(), capabilities)
    })
}

impl ComponentHostCapabilityState {
    /// Check a `Publish`'s code in, then publish it, or queue it while a
    /// spawn or publish of its namespaces is in flight.
    pub(super) fn begin_publish<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        held: Held<PublishResult>,
        payload: Publish,
    ) {
        let Publish { code, configs } = payload;
        match self.modules.check_in(&ctx.blob_check_in(), &code) {
            Ok(module) => {
                self.publish_or_queue(ctx, QueuedPublish::new(Publisher::Publish(held), code, module, configs));
            }
            Err(error) => held.answer(ctx, &PublishResult::Err { error }),
        }
    }

    /// Bind `module` for the first time: no predecessor holds any of its
    /// namespaces. A `Publish` stages the owner batch, and a load publishes
    /// and then spawns.
    pub(super) fn first_publish<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        publisher: Publisher,
        module: Module,
    ) {
        match publisher {
            Publisher::Load { held, load } => self.publish_load(ctx, held, load),
            Publisher::Publish(held) => {
                let id = self.next_publish;
                self.next_publish =
                    self.next_publish.checked_add(1).expect("the component host's publish ids cannot overflow");
                let batch = RegistryBatch::publish_module(&module);
                self.publishes.insert(id, PublishInFlight { held, module });
                ctx.stage_registry_batch(batch, ModulePublished { publish: id });
            }
        }
    }

    /// A `Publish`'s first publish settled (ADR-0241 §3): a commit records
    /// the module, spawns its boot once (ADR-0147), and answers with every
    /// namespace it bound; a refusal answers with the owner's reason.
    pub(super) fn finish_module_publish<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        publish: u64,
        published: RegistryBatchResult,
    ) {
        let PublishInFlight { held, module } =
            self.publishes.remove(&publish).expect("a staged publish waits in state until it settles");
        match published {
            Ok(()) => {
                self.record_published(&module);
                self.boot_published(ctx, &module);
                self.conclude_publish(ctx, Publisher::Publish(held), &module);
            }
            Err(error) => Publisher::Publish(held).refuse(ctx, &format!("module publish refused: {error}")),
        }
    }

    /// `module` is bound, unchanged or committed: a `Publish` answers with
    /// its namespaces, and a load spawns its guest.
    pub(super) fn conclude_publish<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        publisher: Publisher,
        module: &Module,
    ) {
        match publisher {
            Publisher::Publish(held) => {
                let types = published_surfaces(module)
                    .map(|(namespace, capabilities)| PublishedType { namespace, capabilities })
                    .collect();
                held.answer(ctx, &PublishResult::Ok { types });
            }
            Publisher::Load { held, load } => self.spawn_prepared(ctx, Requester::Load(held), load),
        }
    }
}
