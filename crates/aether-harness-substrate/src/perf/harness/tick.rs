//! The tick source — the lifecycle bridge that turns the substrate's own
//! `Tick` fan-out into the sweep's offered load.

use aether_actor::{ActorRef, OutboundReply, Publisher};
use aether_data::{Kind, KindId, ReplyContract};
use aether_kinds::{ComponentCapabilities, HandlerCapability, LifecycleSubscribeResult, Tick};
use aether_lifecycle::LifecycleCapability;
use aether_substrate::{BootError, Dispatch, NativeActor, NativeCtx, NativeInitCtx};

use super::{CountQuery, CountReport, Ping, Relay};

/// Lifecycle bridge for the sweep: it subscribes itself to the `Tick`
/// input stream in its `wire` hook, then emits a burst of `burst` `Ping`s into the entry relay per
/// frame, each inheriting the tick's trace lineage so the whole
/// per-frame fan-out is one causal forest. The honest stand-in for a
/// real tick-reactive component — the substrate's own `Tick` fan-out
/// drives the work, no synthetic injector, no per-root settlement block.
///
/// `burst == 1` is the latency regime (one root per tick, settles within
/// its frame). A larger `burst` is the saturation regime
/// (iamacoffeepot/aether#1202): the whole burst lands on relay 0's inbox
/// in one tick, so a single `advance(1)` drains a deep ready queue — the
/// contention the per-frame `advance` quiescence otherwise prevents.
pub struct TickSource {
    entry: ActorRef<Relay>,
    burst: u32,
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
impl aether_actor::HandlesKind<Tick> for TickSource {}
impl aether_actor::Lifecycle<Self> for TickSource {
    /// `(entry, burst, lifecycle)`: relay 0's proof — the first of those
    /// [`spawn_relays`](super::spawn_relays) returns — the number of `Ping`s
    /// to emit per `Tick` (`1` in `Latency`, `backlog` in `Saturate`), and the
    /// lifecycle cap's proof.
    type Config = (ActorRef<Relay>, u32, ActorRef<LifecycleCapability>);
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init(config: Self::Config, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let (entry, burst, lifecycle) = config;
        Ok(Self { entry, burst, seq: 0, sent: 0, lifecycle })
    }

    /// Subscribe this source to the `Tick` stage as itself (ADR-0082 §7): the
    /// cap reads the subscriber off the host-stamped sender, so no position
    /// crosses the wire.
    fn wire(state: &mut Self, ctx: &mut NativeCtx<'_, Self>) {
        ctx.send_to(state.lifecycle, &LifecycleCapability::subscribe_request::<Tick>());
    }
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
                },
                HandlerCapability {
                    id: CountQuery::ID,
                    name: <CountQuery as Kind>::NAME.to_owned(),
                    doc: None,
                    reply: ReplyContract::One(CountReport::ID),
                },
                HandlerCapability {
                    id: LifecycleSubscribeResult::ID,
                    name: <LifecycleSubscribeResult as Kind>::NAME.to_owned(),
                    doc: None,
                    reply: ReplyContract::None,
                },
            ],
            ..ComponentCapabilities::default()
        }
    }

    fn dispatch(
        state: &mut Self,
        ctx: &mut NativeCtx<'_, Self, aether_substrate::Manual>,
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
            if let Some(LifecycleSubscribeResult::Err { stage, error }) =
                LifecycleSubscribeResult::decode_from_bytes(payload)
            {
                tracing::warn!(target: "aether_perf", stage, %error, "tick source's Tick subscribe was refused");
            }
            return Some(());
        }
        if kind.0 != Tick::ID.0 {
            return None;
        }
        for _ in 0..state.burst {
            ctx.send_to(state.entry, &Ping { seq: state.seq });
            state.seq = state.seq.wrapping_add(1);
            state.sent += 1;
        }
        Some(())
    }
}

pub(super) const TICKSRC_NS: &str = "mlat.ticksrc";
