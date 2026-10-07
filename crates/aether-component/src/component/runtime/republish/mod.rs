//! A publish routed by its pre-checks, and a successor's publish as one
//! group republish (ADR-0241 §7).
//!
//! Every publish, a `Publish`'s or a load's, runs the
//! pre-checks ([`precheck`]), which decide whether the module is unchanged,
//! published for the first time ([`super::publish`]), or republished. A
//! republish's pre-checks refuse before any member is touched. Then
//! every member prepares a candidate beside its running guest; once all are
//! ready the module publishes, and every member commits on a chain of its
//! own, whose settlement the answer waits for. A member's refusal, or a
//! publish failure, aborts every member that prepared, and the answer waits
//! for each to reinstate its old guest.
//!
//! While a republish is in flight its namespaces are held: a load or a spawn
//! of one waits in the republish and runs once the republish has answered,
//! a drop of a member waits the same way, and a second republish of the
//! module is refused. A publish that arrives while a spawn or a first
//! publish of its namespaces is in flight queues until those settle.
//!
//! One exception keeps the hold from deadlocking its own republish: a load,
//! spawn, or drop that arrives on one of the republish's commit chains runs at
//! once. A committing candidate flushes the mail its `on_rehydrate` held on
//! its commit's chain, and the republish answers only once that chain
//! settles; parked, such a request's held reply would keep the chain open,
//! and the republish would never answer. Running it is sound because commits
//! go out only after the successor published: a load is admitted against
//! the winning code, and a drop handed to a member lands behind that
//! member's `Commit`, so the member commits, answers `Committed`, and then
//! closes.

mod precheck;

use std::mem;
use std::sync::Arc;

use aether_actor::{ErasedActorRef, ProtocolRef, ReplyMode};
use aether_data::{Blob, ErasedActorPath, MailId};
use aether_kinds::trace::Settled;
use aether_kinds::{DropComponent, DropResult, InstanceConfig, LoadResult, Spawn, SpawnResult};
use aether_substrate::actor::native::{Held, NativeCtx, RegistryBatch, RegistryBatchResult};
use aether_substrate::actor::wasm::module::Module;

use crate::component::runtime::load::PreparedLoad;
use crate::component::runtime::publish::Publisher;
use crate::component::runtime::{ComponentHostCapabilityState, GuestControl, HostCtx};
use crate::component::{Abort, Commit, Prepare, Prepared};
use crate::kinds::{RepublishMember, RepublishPublished};

use self::precheck::Plan;

/// The key of a [`Republish`], carried by its prepare, commit, abort and
/// publish contexts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct RepublishId(u64);

/// One republish in flight: what waits on its publish, the successor
/// module, the members moving to it, and what waits for it to answer.
pub(super) struct Republish {
    publisher: Publisher,
    module: Module,
    namespaces: Vec<String>,
    members: Vec<Member>,
    /// Each member's refusal, or the publish's, in arrival order. Any entry
    /// aborts the group.
    refusals: Vec<String>,
    parked_loads: Vec<(Held<LoadResult>, Arc<PreparedLoad>, Blob)>,
    parked_spawns: Vec<(Held<SpawnResult>, Spawn)>,
    parked_drops: Vec<(Held<DropResult>, DropComponent)>,
}

impl Republish {
    /// Hold a load of one of this republish's namespaces, with its code,
    /// until the republish answers.
    pub(super) fn park_load(&mut self, held: Held<LoadResult>, load: Arc<PreparedLoad>, code: Blob) {
        self.parked_loads.push((held, load, code));
    }

    /// Hold a spawn of one of this republish's namespaces until the
    /// republish answers. It is prepared again then, against the code that
    /// won.
    pub(super) fn park_spawn(&mut self, held: Held<SpawnResult>, spawn: Spawn) {
        self.parked_spawns.push((held, spawn));
    }

    fn is_member(&self, actor: ErasedActorRef) -> bool {
        self.members.iter().any(|member| member.actor == actor)
    }

    /// Whether no member still owes an answer to the step it was sent.
    fn quiet(&self) -> bool {
        self.members.iter().all(|member| !matches!(member.step, Step::Preparing | Step::Aborting))
    }
}

/// One live instance a republish moves.
pub(super) struct Member {
    actor: ErasedActorRef,
    control: ProtocolRef<GuestControl>,
    path: ErasedActorPath,
    /// The config its candidate is built with, `None` for its stored one;
    /// taken when the prepare is sent.
    config: Option<Vec<u8>>,
    step: Step,
}

impl Member {
    fn new(
        actor: ErasedActorRef,
        control: ProtocolRef<GuestControl>,
        path: ErasedActorPath,
        config: Option<Vec<u8>>,
    ) -> Self {
        Self { actor, control, path, config, step: Step::Preparing }
    }
}

/// Where one member stands in its republish.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Preparing,
    Ready,
    Refused,
    /// Its commit was sent; the republish answers once it has committed and
    /// the commit's chain has settled.
    Committing {
        committed: bool,
        settled: bool,
    },
    Aborting,
    Aborted,
}

/// A commit whose chain's `Settled` a republish waits for.
pub(super) struct CommitRoot {
    republish: RepublishId,
    member: usize,
}

/// A publish that waits for the spawns and first publishes of its
/// namespaces to settle before its pre-checks run. It keeps its checked-in
/// module, so the module's cache entry stays live meanwhile.
pub(super) struct QueuedPublish {
    publisher: Publisher,
    code: Blob,
    module: Module,
    configs: Vec<InstanceConfig>,
}

impl QueuedPublish {
    /// A publish of `module`, checked in from `code`, for `publisher`, with
    /// the configs a successor's instances are built with.
    pub(super) fn new(publisher: Publisher, code: Blob, module: Module, configs: Vec<InstanceConfig>) -> Self {
        Self { publisher, code, module, configs }
    }
}

impl ComponentHostCapabilityState {
    /// Run every queued publish whose namespaces nothing in flight holds any
    /// more, in arrival order.
    pub(super) fn release_queued_publishes<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>) {
        for queued in mem::take(&mut self.queued_publishes) {
            self.publish_or_queue(ctx, queued);
        }
    }

    /// Run a publish's pre-checks and route it, or queue it while a spawn, a
    /// first publish, or a staged unpublish of its namespaces is in flight:
    /// an unchanged module is bound already, a first publish binds it, and a
    /// successor republishes its group.
    pub(super) fn publish_or_queue<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, queued: QueuedPublish) {
        let busy = queued.module.published_groups().any(|(namespace, _)| {
            self.loads.values().any(|load| load.published() == namespace)
                || self.publishes.values().any(|publish| publish.publishes(&namespace))
                || self.unpublishes.values().any(|unpublish| unpublish.publishes(&namespace))
        });
        if busy {
            self.queued_publishes.push(queued);
            return;
        }

        let QueuedPublish { publisher, code, module, configs } = queued;
        match self.plan_republish(ctx, &module, configs) {
            Ok(Plan::Unchanged) => self.conclude_publish(ctx, publisher, &module),
            Ok(Plan::Publish) => self.first_publish(ctx, publisher, code, module),
            Ok(Plan::Group(members)) => self.start_republish(ctx, publisher, code, module, members),
            Err(error) => publisher.refuse(ctx, &error),
        }
    }

    /// Hold the module's namespaces and send every member its prepare. A
    /// module with no live instance publishes at once.
    fn start_republish<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        publisher: Publisher,
        code: Blob,
        module: Module,
        mut members: Vec<Member>,
    ) {
        let id = RepublishId(self.next_republish);
        self.next_republish =
            self.next_republish.checked_add(1).expect("the component host's republish ids cannot overflow");
        let namespaces: Vec<String> = module.published_groups().map(|(namespace, _)| namespace.into_owned()).collect();
        for namespace in &namespaces {
            self.republishing.insert(namespace.clone(), id);
        }

        for (index, member) in members.iter_mut().enumerate() {
            let prepare = Prepare { code: code.clone(), config: member.config.take() };
            let _ = ctx.send_to_with_context(member.control, &prepare, member_context(id, index));
        }
        let empty = members.is_empty();
        let republish = Republish {
            publisher,
            module,
            namespaces,
            members,
            refusals: Vec::new(),
            parked_loads: Vec::new(),
            parked_spawns: Vec::new(),
            parked_drops: Vec::new(),
        };
        self.republishes.insert(id, republish);
        if empty {
            self.publish_republish(ctx, id);
        }
    }

    /// A member answered its prepare. Once every member has, a group that is
    /// all ready publishes, and any refusal aborts the group.
    pub(super) fn finish_prepare<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, prepared: Prepared) {
        let Some((id, index)) = take_member(ctx) else {
            return;
        };
        let Some(republish) = self.republishes.get_mut(&id) else {
            return;
        };
        let member = &mut republish.members[index];
        member.step = match prepared {
            Prepared::Ready => Step::Ready,
            Prepared::Refused { error } => {
                republish.refusals.push(format!("{}: {error}", member.path));
                Step::Refused
            }
        };
        if !republish.quiet() {
            return;
        }
        if republish.refusals.is_empty() {
            self.publish_republish(ctx, id);
        } else {
            self.abort_republish(ctx, id);
        }
    }

    /// Publish the successor module (ADR-0241 §3): admission runs again
    /// against the table as the owner stages it, and the module's kinds
    /// register.
    fn publish_republish<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, id: RepublishId) {
        let module = &self.republishes[&id].module;
        let _ = ctx.stage_registry_batch(RegistryBatch::publish_module(module), RepublishPublished { republish: id.0 });
    }

    /// The successor's publish settled. A commit rewrites each member's
    /// module and sends each its commit on a chain of its own, whose
    /// settlement the answer waits for; a refusal aborts the group.
    pub(super) fn finish_republish_publish<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        republish: u64,
        published: RegistryBatchResult,
    ) {
        let id = RepublishId(republish);
        let Some(republish) = self.republishes.get_mut(&id) else {
            return;
        };
        if let Err(error) = published {
            republish.refusals.push(format!("module publish refused: {error}"));
            self.abort_republish(ctx, id);
            return;
        }

        for (index, member) in republish.members.iter_mut().enumerate() {
            if let Some(guest) = self.drop_targets.get_mut(&member.actor) {
                guest.module = republish.module.clone();
            }
            let root = ctx.send_detached_to_with_context(member.control, &Commit, member_context(id, index));
            // Without a settlement registry there is no chain to wait on, so
            // the member's `Committed` alone completes it.
            let settled = !ctx.subscribe_settlement::<Settled>(root);
            if !settled {
                self.commit_roots.insert(root, CommitRoot { republish: id, member: index });
            }
            member.step = Step::Committing { committed: false, settled };
        }
        self.conclude_commit_if_done(ctx, id);
    }

    /// A member committed.
    pub(super) fn finish_commit<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>) {
        let Some((id, index)) = take_member(ctx) else {
            return;
        };
        if let Some(Step::Committing { committed, .. }) =
            self.republishes.get_mut(&id).map(|republish| &mut republish.members[index].step)
        {
            *committed = true;
        }
        self.conclude_commit_if_done(ctx, id);
    }

    /// A commit's chain settled.
    pub(super) fn settle_commit<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, root: MailId) {
        let Some(CommitRoot { republish: id, member }) = self.commit_roots.remove(&root) else {
            return;
        };
        if let Some(Step::Committing { settled, .. }) =
            self.republishes.get_mut(&id).map(|republish| &mut republish.members[member].step)
        {
            *settled = true;
        }
        self.conclude_commit_if_done(ctx, id);
    }

    /// Conclude the publish once every member has committed and its
    /// commit's chain has settled.
    fn conclude_commit_if_done<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, id: RepublishId) {
        let done = self.republishes.get(&id).is_some_and(|republish| {
            republish.members.iter().all(|member| member.step == Step::Committing { committed: true, settled: true })
        });
        if done {
            let republish = self.republishes.remove(&id).expect("a concluded republish is in flight");
            self.release_republish(ctx, republish, Ok(()));
        }
    }

    /// Send every ready member its abort. The group answers `Err` once each
    /// has reinstated its old guest.
    fn abort_republish<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, id: RepublishId) {
        let republish = self.republishes.get_mut(&id).expect("an aborting republish is in flight");
        for (index, member) in republish.members.iter_mut().enumerate() {
            if member.step == Step::Ready {
                let _ = ctx.send_to_with_context(member.control, &Abort, member_context(id, index));
                member.step = Step::Aborting;
            }
        }
        self.conclude_abort_if_done(ctx, id);
    }

    /// A member reinstated its old guest.
    pub(super) fn finish_abort<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>) {
        let Some((id, index)) = take_member(ctx) else {
            return;
        };
        if let Some(republish) = self.republishes.get_mut(&id) {
            republish.members[index].step = Step::Aborted;
        }
        self.conclude_abort_if_done(ctx, id);
    }

    fn conclude_abort_if_done<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, id: RepublishId) {
        if self.republishes.get(&id).is_some_and(Republish::quiet) {
            let mut republish = self.republishes.remove(&id).expect("a concluded republish is in flight");
            let refusals = mem::take(&mut republish.refusals).join("; ");
            self.release_republish(ctx, republish, Err(refusals));
        }
    }

    /// Release the republish's namespaces, conclude its publish with its
    /// `outcome`, and run what waited for it: each parked load publishes, each
    /// parked spawn and drop runs, and each queued publish is retried.
    fn release_republish<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        republish: Republish,
        outcome: Result<(), String>,
    ) {
        let Republish { publisher, module, namespaces, parked_loads, parked_spawns, parked_drops, .. } = republish;
        for namespace in namespaces {
            self.republishing.remove(&namespace);
        }
        match outcome {
            Ok(()) => self.conclude_publish(ctx, publisher, &module),
            Err(refusals) => publisher.refuse(ctx, &refusals),
        }
        for (held, load, code) in parked_loads {
            self.publish_then_spawn(ctx, held, load, code);
        }
        for (held, payload) in parked_spawns {
            self.begin_spawn(ctx, held, payload);
        }
        for (held, payload) in parked_drops {
            self.begin_drop(ctx, held, payload);
        }
        self.release_queued_publishes(ctx);
    }

    /// Drop the guest a `DropComponent` names: hand the drop to it, and it
    /// answers and closes (ADR-0241 §8). A drop of a member of a republish
    /// in flight waits until the republish answers.
    pub(super) fn begin_drop<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        held: Held<DropResult>,
        payload: DropComponent,
    ) {
        // ADR-0230: prove the address at receipt. An address with no live
        // route has no trampoline to drop, so it answers `Err` now rather
        // than handing off into nothing; no position leaves the verb.
        let actor = match ctx.resolve_path(&payload.target) {
            Ok(proven) => proven,
            Err(error) => {
                let error = format!("no component to drop at {}: {error}", payload.target);
                held.answer(ctx, &DropResult::Err { error });
                return;
            }
        };
        // The drop waits for its member's republish, unless it arrives on
        // one of that republish's own commit chains (see the module docs).
        let committing = self.committing_republish(ctx);
        if let Some((_, republish)) =
            self.republishes.iter_mut().find(|(id, republish)| republish.is_member(actor) && committing != Some(**id))
        {
            republish.parked_drops.push((held, payload));
            return;
        }
        // The drop closes the trampoline, so its entry leaves now: a second
        // drop that proves the path before the owner applies its `Dropped`
        // route finds no entry and is refused.
        let Some(guest) = self.drop_targets.remove(&actor) else {
            let error = format!("no live component to drop at {}", payload.target);
            held.answer(ctx, &DropResult::Err { error });
            return;
        };
        held.hand_off(ctx, guest.control, &payload);
    }
}

impl ComponentHostCapabilityState {
    /// The republish that holds `namespace`, which a load or spawn of it
    /// waits in, unless the request arrives on that republish's own commit
    /// chain, where the successor has already published and waiting would
    /// hold the chain the republish waits on open (see the module docs).
    pub(super) fn holding_republish<M: ReplyMode>(
        &mut self,
        ctx: &HostCtx<'_, M>,
        namespace: &str,
    ) -> Option<&mut Republish> {
        let committing = self.committing_republish(ctx);
        let republish = *self.republishing.get(namespace).filter(|republish| committing != Some(**republish))?;
        Some(self.republishes.get_mut(&republish).expect("a republishing namespace names a republish in flight"))
    }

    /// The republish whose commit chain the handled mail rides, read from
    /// the ctx's in-flight root: a request a committing candidate held and
    /// its commit flushed. Such a request runs at once rather than waiting
    /// for the republish it would otherwise hold open (see the module docs).
    pub(super) fn committing_republish<A, S, M: ReplyMode>(&self, ctx: &NativeCtx<'_, A, S, M>) -> Option<RepublishId> {
        ctx.in_flight_root().and_then(|root| self.commit_roots.get(&root)).map(|commit| commit.republish)
    }
}

fn member_context(id: RepublishId, index: usize) -> RepublishMember {
    let member = u32::try_from(index).expect("a republish has fewer than 2^32 members");
    RepublishMember { republish: id.0, member }
}

/// The republish and member an answer is for, from its stored context.
fn take_member<M: ReplyMode>(ctx: &mut HostCtx<'_, M>) -> Option<(RepublishId, usize)> {
    let RepublishMember { republish, member } = ctx.take_context()?;
    Some((RepublishId(republish), usize::try_from(member).expect("a member index fits the platform")))
}
