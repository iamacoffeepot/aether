//! The tick driver: each `Tick` it offers the frame's load to the producers,
//! under the tick's chain, so a frame settles when its handlers have run.

use std::sync::atomic::Ordering;

use aether_actor::{ActorRef, Here, Publisher, Row, Silent, There};
use aether_data::{Kind, KindId, ReplyContract};
use aether_kinds::{ComponentCapabilities, HandlerCapability, Tick};
use aether_lifecycle::{LifecycleCapability, LifecycleSubscribeResult};
use aether_substrate::{BootError, Dispatch, NativeActor, NativeCtx, NativeInitCtx};

use crate::kinds::{Job, OP_KICK, OP_LOOP, OP_WORK};
use crate::producer::Producer;
use crate::stats::{LOAD_DIRECT, LOAD_HANDLERS, LOAD_LINES, LOAD_QUIET, LOOP_START};

pub struct Driver {
    producers: Vec<ActorRef<Producer>>,
    lifecycle: ActorRef<LifecycleCapability>,
    frame: usize,
}

impl Driver {
    /// Spread `handlers` jobs of `lines` lines each over the producers,
    /// starting at a producer that moves every frame.
    fn offer(
        &self,
        ctx: &mut NativeCtx<'_, Self, aether_substrate::Unchecked>,
        handlers: u64,
        lines: u32,
        direct: u32,
    ) {
        let total = self.producers.len() as u64;
        if handlers == 0 || total == 0 {
            return;
        }
        let used = handlers.min(total);
        let each = handlers / used;
        let extra = handlers % used;
        for slot in 0..used {
            let count = u32::try_from(each + u64::from(slot < extra)).unwrap_or(u32::MAX);
            let producer = self.producers[(self.frame + slot as usize) % self.producers.len()];
            let op = if count == 1 {
                OP_WORK
            } else {
                OP_KICK
            };
            ctx.send_to(producer, &Job { op, count, lines, direct });
        }
    }
}

impl aether_actor::Addressable for Driver {
    const NAMESPACE: &'static str = "spike.log.driver";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for Driver {}
impl aether_actor::HandlesKind<Tick> for Driver {}
impl aether_actor::Contracts for Driver {
    type Rows = (Row<Tick, Silent>, (Row<LifecycleSubscribeResult, Silent>, ()));
    const CONTRACTS: &'static [(KindId, ReplyContract)] =
        &[(Tick::ID, ReplyContract::None), (LifecycleSubscribeResult::ID, ReplyContract::None)];
}
impl aether_actor::Contract<Tick> for Driver {
    type Reply = Silent;
    type Index = Here;
}
impl aether_actor::Contract<LifecycleSubscribeResult> for Driver {
    type Reply = Silent;
    type Index = There<Here>;
}
impl aether_actor::Lifecycle<Self> for Driver {
    type Config = (Vec<ActorRef<Producer>>, ActorRef<LifecycleCapability>);
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init(config: Self::Config, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let (producers, lifecycle) = config;
        Ok(Self { producers, lifecycle, frame: 0 })
    }

    fn wire(state: &mut Self, ctx: &mut NativeCtx<'_, Self>) -> Result<(), BootError> {
        ctx.send_to(state.lifecycle, &LifecycleCapability::subscribe_request::<Tick>());
        Ok(())
    }
}
impl aether_actor::Declared for Driver {
    type Depends = ();
    type Spawns = ();
    type Parents = ();
}
impl NativeActor for Driver {
    type State = Self;
}
impl Dispatch<Self> for Driver {
    fn capabilities() -> ComponentCapabilities {
        let silent = |id: KindId, name: &str| HandlerCapability {
            id,
            name: name.to_owned(),
            doc: None,
            reply: ReplyContract::None,
            reason: None,
        };
        ComponentCapabilities {
            handlers: vec![
                silent(Tick::ID, <Tick as Kind>::NAME),
                silent(LifecycleSubscribeResult::ID, <LifecycleSubscribeResult as Kind>::NAME),
            ],
            ..ComponentCapabilities::default()
        }
    }

    fn dispatch(
        state: &mut Self,
        ctx: &mut NativeCtx<'_, Self, aether_substrate::Unchecked>,
        kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        if kind.0 == LifecycleSubscribeResult::ID.0 {
            return Some(());
        }
        if kind.0 != Tick::ID.0 {
            return None;
        }
        let direct = u32::try_from(LOAD_DIRECT.load(Ordering::Relaxed)).unwrap_or(1);
        let lines = u32::try_from(LOAD_LINES.load(Ordering::Relaxed)).unwrap_or(1);
        state.offer(ctx, LOAD_HANDLERS.load(Ordering::Relaxed), lines, direct);
        state.offer(ctx, LOAD_QUIET.load(Ordering::Relaxed), 0, direct);
        let loopers = usize::try_from(LOOP_START.swap(0, Ordering::Relaxed)).unwrap_or(0);
        for producer in state.producers.iter().take(loopers) {
            ctx.send_to(*producer, &Job { op: OP_LOOP, count: 0, lines, direct });
        }
        state.frame = state.frame.wrapping_add(1);
        Some(())
    }
}
