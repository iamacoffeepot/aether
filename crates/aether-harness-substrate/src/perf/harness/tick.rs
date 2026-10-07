//! The tick source — the lifecycle bridge that turns the substrate's own
//! `Tick` fan-out into the sweep's offered load.

use aether_actor::{ActorRef, Here, OutboundReply, Publisher, Row, Silent, There};
use aether_data::{Kind, KindId, ReplyContract};
use aether_kinds::{ComponentCapabilities, HandlerCapability, Tick};
use aether_lifecycle::{LifecycleCapability, LifecycleSubscribeResult};
use aether_substrate::{BootError, Dispatch, NativeActor, NativeCtx, NativeInitCtx};

use super::{CountQuery, CountReport, Ping, Relay};

/// Lifecycle bridge for the sweep: it subscribes itself to the `Tick`
/// input stream in its `wire` hook, then emits `pings_per_tick` `Ping`s into the entry relay per
/// frame, each inheriting the tick's trace lineage so the whole
/// per-frame fan-out is one causal forest. The honest stand-in for a
/// real tick-reactive component — the substrate's own `Tick` fan-out
/// drives the work, no synthetic injector, no per-root settlement block.
///
/// `pings_per_tick == 1` is the latency regime (one root per tick, settles within
/// its frame). A larger `pings_per_tick` is the saturation regime
/// (iamacoffeepot/aether#1202): the whole backlog lands on relay 0's inbox
/// in one tick, so a single `advance(1)` drains a deep ready queue — the
/// contention the per-frame `advance` quiescence otherwise prevents.
pub struct TickSource {
    entry: ActorRef<Relay>,
    pings_per_tick: u32,
    seq: u32,
    /// `Ping` mails emitted into the entry, for the run-end keep-up harvest
    /// (iamacoffeepot/aether#1233) — the offered load. `seq` wraps at `u32`
    /// for trace legibility; this is the honest cumulative count.
    sent: u64,
    /// The lifecycle cap's proof, which `wire` subscribes this source to
    /// `Tick` through.
    lifecycle: ActorRef<LifecycleCapability>,
}

impl aether_actor::Addressable for TickSource {
    const NAMESPACE: &'static str = "mlat.ticksrc";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for TickSource {}
impl aether_actor::HandlesKind<Tick> for TickSource {
    type Sender = aether_actor::Anyone;
}
impl aether_actor::HandlesKind<CountQuery> for TickSource {
    type Sender = aether_actor::Anyone;
}
impl aether_actor::HandlesKind<LifecycleSubscribeResult> for TickSource {
    type Sender = aether_actor::Anyone;
}
/// The type-level mirror of [`Dispatch::capabilities`], row for row and in the
/// same order, so a [`TickSource`] reference narrows to the protocols its
/// dispatch answers, such as [`PerfParticipant`](super::PerfParticipant).
impl aether_actor::Contracts for TickSource {
    type Rows = (Row<Tick, Silent>, (Row<CountQuery, CountReport>, (Row<LifecycleSubscribeResult, Silent>, ())));
    const CONTRACTS: &'static [(KindId, ReplyContract)] = &[
        (Tick::ID, ReplyContract::None),
        (CountQuery::ID, ReplyContract::One(CountReport::ID)),
        (LifecycleSubscribeResult::ID, ReplyContract::None),
    ];
}
impl aether_actor::Contract<Tick> for TickSource {
    type Reply = Silent;
    type Sender = aether_actor::Anyone;
    type Index = Here;
}
impl aether_actor::Contract<CountQuery> for TickSource {
    type Reply = CountReport;
    type Sender = aether_actor::Anyone;
    type Index = There<Here>;
}
impl aether_actor::Contract<LifecycleSubscribeResult> for TickSource {
    type Reply = Silent;
    type Sender = aether_actor::Anyone;
    type Index = There<There<Here>>;
}
impl aether_actor::Lifecycle<Self> for TickSource {
    /// `(entry, pings_per_tick, lifecycle)`: relay 0's proof — the first of those
    /// [`spawn_relays`](super::spawn_relays) returns — the number of `Ping`s
    /// to emit per `Tick` (`1` in `Latency`, `backlog` in `Saturate`), and the
    /// lifecycle cap's proof.
    type Config = (ActorRef<Relay>, u32, ActorRef<LifecycleCapability>);
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init(config: Self::Config, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let (entry, pings_per_tick, lifecycle) = config;
        Ok(Self { entry, pings_per_tick, seq: 0, sent: 0, lifecycle })
    }

    /// Subscribe this source to the `Tick` stage as itself (ADR-0082 §7): the
    /// cap reads the subscriber off the host-stamped sender, so no position
    /// crosses the wire.
    fn wire(state: &mut Self, ctx: &mut NativeCtx<'_, Self>) -> Result<(), BootError> {
        ctx.send_to(state.lifecycle, &LifecycleCapability::subscribe_request::<Tick>());
        Ok(())
    }
}
impl aether_actor::Declared for TickSource {
    type Depends = ();
    type Spawns = ();
    type Parents = ();
}
impl NativeActor for TickSource {
    type State = Self;
}
impl Dispatch<Self> for TickSource {
    /// See `Relay::capabilities` (iamacoffeepot/aether#4236) — a hand-written
    /// `Dispatch` gets no generated handler declaration, and the spawn path
    /// seeds the cost table from this list.
    fn capabilities() -> ComponentCapabilities {
        ComponentCapabilities {
            handlers: vec![
                HandlerCapability {
                    id: Tick::ID,
                    name: <Tick as Kind>::NAME.to_owned(),
                    doc: None,
                    reply: ReplyContract::None,
                    reason: None,
                },
                HandlerCapability {
                    id: CountQuery::ID,
                    name: <CountQuery as Kind>::NAME.to_owned(),
                    doc: None,
                    reply: ReplyContract::One(CountReport::ID),
                    reason: None,
                },
                HandlerCapability {
                    id: LifecycleSubscribeResult::ID,
                    name: <LifecycleSubscribeResult as Kind>::NAME.to_owned(),
                    doc: None,
                    reply: ReplyContract::None,
                    reason: None,
                },
            ],
            ..ComponentCapabilities::default()
        }
    }

    fn dispatch(
        state: &mut Self,
        ctx: &mut NativeCtx<'_, Self, aether_substrate::Unchecked>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        // Run-end keep-up harvest (iamacoffeepot/aether#1233): the source
        // never receives a `Ping`, so its `received` is 0.
        if kind.0 == CountQuery::ID.0 {
            ctx.reply(&CountReport { sent: state.sent, received: 0 });
            return Some(());
        }
        // The lifecycle cap's answer to `wire`'s self-subscription. A refusal
        // means the harness's lifecycle graph lacks `Tick`, so the cell runs
        // with no offered load; say so rather than report a silent zero.
        if kind.0 == LifecycleSubscribeResult::ID.0 {
            if let Some(LifecycleSubscribeResult::Err(error)) = LifecycleSubscribeResult::decode_from_bytes(payload) {
                tracing::warn!(target: "aether_perf", ?error, "tick source's Tick subscribe was refused");
            }
            return Some(());
        }
        if kind.0 != Tick::ID.0 {
            return None;
        }
        for _ in 0..state.pings_per_tick {
            ctx.send_to(state.entry, &Ping { seq: state.seq });
            state.seq = state.seq.wrapping_add(1);
            state.sent += 1;
        }
        Some(())
    }
}

pub(super) const TICKSRC_NS: &str = "mlat.ticksrc";
