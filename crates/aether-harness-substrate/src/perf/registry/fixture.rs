//! The staging fixture that puts the registry owner under load
//! (iamacoffeepot/aether#4176).
//!
//! # Why a fixture rather than threads
//!
//! Measuring the owner's *loaded* ceiling needs its queue to be deep — a
//! drainer that retires one item per drain cannot amortize, so it reports a
//! service rate rather than a ceiling. Depth needs commits submitted faster
//! than they retire, which the benchmark's original populate phase could never
//! do: sequential `spawn_actor(..).finish()` blocks until each birth lands, so
//! the queue holds one item and `drain_max` is 1.
//!
//! The way to submit without blocking is the one production already uses.
//! `aether-tcp`'s listener is the reference: its accept thread cannot spawn
//! (no dispatcher ctx), so it hands the connection to actor-owned state, fires
//! a typed wake, and the *handler* stages the session child —
//! `ctx.spawn_child(..).stage()` returns a receipt immediately and the
//! authoritative result arrives later as `TaskDone<SpawnOutcome, _>`. A handler
//! can therefore stage N births back-to-back, and all N sit in the owner queue
//! at once. That is exactly the load this fixture exists to create, and it
//! reaches the owner through the path production drives it through, so the
//! ceiling is measured against the real effect mix rather than a synthetic one.
//!
//! # The completion is not optional
//!
//! A stager owes every staged birth its ADR-0093 completion. `stage()` takes a
//! settlement hold; dropping the `TaskDone` without discharging it leaves that
//! hold outstanding, and because the harness's own send path is settle-gated,
//! the benchmark would block forever waiting for a chain that can never settle.
//! [`CommitParent`] discharges each one with `release_no_reply`, the same call
//! `TcpListenerActor::on_session_spawn_done` makes.
//!
//! Dispatch is hand-written rather than `#[actor]`-generated, matching the
//! sibling actors in `perf::harness`. Completions arrive as the single
//! substrate-internal `TaskCompletionWake` kind carrying a `DispatchId`, which
//! the handler trades for its `TaskDone` through `NativeCtx::take_task_done` —
//! the documented hand-wired form of `#[handler(task)]`.

use aether_actor::OutboundReply;
use aether_data::{Kind, KindId, MailboxId, ReplyContract};
use aether_kinds::{ComponentCapabilities, HandlerCapability};
use aether_substrate::actor::native::offload::blocking::TaskCompletionWake;
use aether_substrate::actor::native::{DispatchId, SpawnOutcome};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Dispatch, NativeActor, NativeCtx, NativeInitCtx, Subname};

/// Ask [`CommitParent`] to stage `count` births in one handler pass — the
/// burst that makes the owner queue deep. Every birth is submitted before the
/// handler returns, so the queue sees them together rather than one at a time.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "perf.registry.stage_burst")]
pub struct StageBurst {
    pub count: u32,
}

/// Ask [`CommitParent`] to close every child it currently holds — the second
/// half of a churn cycle, so repeated bursts republish the route table instead
/// of growing it without bound.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "perf.registry.close_burst")]
pub struct CloseBurst {
    /// Unused; present only so the request carries a non-empty `Pod` body.
    pub nonce: u32,
}

/// Read [`CommitParent`]'s birth tally.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "perf.registry.commit_query")]
pub struct CommitQuery {
    /// Unused; present only so the query carries a non-empty `Pod` body.
    pub nonce: u32,
}

/// [`CommitParent`]'s reply to a [`CommitQuery`]. `staged` counts births
/// submitted, `succeeded` + `failed` count completions discharged; the
/// benchmark asserts they agree, because a shortfall means a completion was
/// dropped and its settlement hold leaked.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "perf.registry.commit_report")]
pub struct CommitReport {
    pub staged: u64,
    pub succeeded: u64,
    pub failed: u64,
    /// Children currently held open — the amount the route table is inflated
    /// by, so a read-scaling cell can report the table size it actually swept.
    pub live: u64,
}

/// Tell a [`CommitChild`] to shut itself down.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "perf.registry.close_child")]
pub struct CloseChild {
    /// Unused; present only so the request carries a non-empty `Pod` body.
    pub nonce: u32,
}

/// A staged birth's only job is to exist: it occupies a route, which is the
/// registry work whose commit rate is under measurement. It handles
/// [`CloseChild`] so the parent can retire it and keep the table from growing
/// across churn cycles.
pub struct CommitChild;

impl aether_actor::Addressable for CommitChild {
    const NAMESPACE: &'static str = "perf.registry.child";
    type Resolver = aether_actor::Many;
}
impl aether_actor::ChildOf<CommitParent> for CommitChild {}
impl aether_actor::HandlesKind<CloseChild> for CommitChild {}
impl aether_actor::Lifecycle<Self> for CommitChild {
    type Config = ();
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a>;
    fn init((): (), (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }
}
impl NativeActor for CommitChild {
    type State = Self;
}
impl Dispatch<Self> for CommitChild {
    /// Declared so the spawn path seeds this actor's cost cells
    /// (iamacoffeepot/aether#4236) — a hand-written `Dispatch` gets no
    /// generated override, and an actor with no cost cells measures a dispatch
    /// path with cost-awareness disabled.
    fn capabilities() -> ComponentCapabilities {
        ComponentCapabilities {
            handlers: vec![HandlerCapability {
                id: CloseChild::ID,
                name: <CloseChild as Kind>::NAME.to_owned(),
                doc: None,
                reply: ReplyContract::None,
            }],
            ..ComponentCapabilities::default()
        }
    }

    fn dispatch(
        _state: &mut Self,
        ctx: &mut NativeCtx<'_, aether_substrate::Manual, Self>,
        kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        if kind.0 != CloseChild::ID.0 {
            return None;
        }
        ctx.shutdown();
        Some(())
    }
}

/// Stages births in bursts so the registry owner's queue goes deep, and
/// discharges every resulting completion. See the module docs for why this
/// shape rather than concurrent embedder threads.
pub struct CommitParent {
    /// Children staged and not yet closed. The parent holds these so a churn
    /// cycle can retire exactly what it created.
    live: Vec<MailboxId>,
    staged: u64,
    succeeded: u64,
    failed: u64,
}

impl aether_actor::Addressable for CommitParent {
    const NAMESPACE: &'static str = "perf.registry.parent";
    /// `Many` rather than `One` so the benchmark spawns it the same instanced
    /// way it spawns the sibling `perf::harness` actors — the singleton-ness is
    /// the benchmark's convention, not an addressing constraint.
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for CommitParent {}
impl aether_actor::HandlesKind<StageBurst> for CommitParent {}
impl aether_actor::HandlesKind<CloseBurst> for CommitParent {}
impl aether_actor::HandlesKind<CommitQuery> for CommitParent {}
impl aether_actor::Lifecycle<Self> for CommitParent {
    type Config = ();
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a>;
    fn init((): (), (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { live: Vec::new(), staged: 0, succeeded: 0, failed: 0 })
    }
}
impl NativeActor for CommitParent {
    type State = Self;
}
impl Dispatch<Self> for CommitParent {
    /// Declared for the same cost-cell reason as [`CommitChild`]'s.
    fn capabilities() -> ComponentCapabilities {
        ComponentCapabilities {
            handlers: vec![
                HandlerCapability {
                    id: StageBurst::ID,
                    name: <StageBurst as Kind>::NAME.to_owned(),
                    doc: None,
                    reply: ReplyContract::None,
                },
                HandlerCapability {
                    id: CloseBurst::ID,
                    name: <CloseBurst as Kind>::NAME.to_owned(),
                    doc: None,
                    reply: ReplyContract::None,
                },
                HandlerCapability {
                    id: CommitQuery::ID,
                    name: <CommitQuery as Kind>::NAME.to_owned(),
                    doc: None,
                    reply: ReplyContract::One(CommitReport::ID),
                },
            ],
            ..ComponentCapabilities::default()
        }
    }

    /// Widen the cost-table seed past [`Self::capabilities`]
    /// (iamacoffeepot/aether#4266). Every ADR-0093 completion arrives as the
    /// single framework kind `TaskCompletionWake`, so this actor services a
    /// kind no caller can address and `capabilities` therefore must not
    /// advertise. Without this the completion arm owns no cost cell and the
    /// dispatch path warns on every burst — the `#[actor]` macro emits the same
    /// override for any actor carrying a task handler.
    fn measured_kinds() -> Vec<KindId> {
        vec![StageBurst::ID, CloseBurst::ID, CommitQuery::ID, TaskCompletionWake::ID]
    }

    fn dispatch(
        state: &mut Self,
        ctx: &mut NativeCtx<'_, aether_substrate::Manual, Self>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        if kind.0 == StageBurst::ID.0 {
            let burst = StageBurst::decode_from_bytes(payload)?;
            state.stage_burst(ctx, burst.count);
            return Some(());
        }
        if kind.0 == CloseBurst::ID.0 {
            for child in state.live.drain(..) {
                let _ = ctx.send_envelope_tracked(child, CloseChild::ID, &CloseChild::default().encode_into_bytes());
            }
            return Some(());
        }
        if kind.0 == CommitQuery::ID.0 {
            ctx.reply(&CommitReport {
                staged: state.staged,
                succeeded: state.succeeded,
                failed: state.failed,
                live: state.live.len() as u64,
            });
            return Some(());
        }
        if kind == TaskCompletionWake::ID {
            let wake = TaskCompletionWake::decode_from_bytes(payload)?;
            let done = ctx.take_task_done::<SpawnOutcome, ()>(DispatchId(wake.dispatch_id))?;
            match &done.output().result {
                Ok(()) => state.succeeded += 1,
                Err(_) => state.failed += 1,
            }
            // The discharge #4176 called a blocking contract. Without it the
            // staged birth's settlement hold is never released and the
            // benchmark's settle-gated send waits forever.
            done.release_no_reply();
            return Some(());
        }
        None
    }
}

impl CommitParent {
    /// Stage `count` births without waiting on any of them. `Subname::Counter`
    /// draws from the spawner's monotonic sequence, so bursts never collide
    /// with each other and a name conflict cannot be mistaken for owner
    /// backpressure.
    fn stage_burst(&mut self, ctx: &mut NativeCtx<'_, aether_substrate::Manual, Self>, count: u32) {
        for _ in 0..count {
            match ctx.spawn_child::<CommitChild>(Subname::Counter, (), ()).stage() {
                Ok(receipt) => {
                    self.live.push(receipt.mailbox_id);
                    self.staged += 1;
                }
                // A local preparation failure never reached the owner, so it
                // owes no completion and must not be counted as one.
                Err(error) => {
                    tracing::warn!(target: "aether_perf", ?error, "staged birth refused before submission");
                    self.failed += 1;
                }
            }
        }
    }
}
