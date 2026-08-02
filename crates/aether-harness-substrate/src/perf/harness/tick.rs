//! The tick source — the lifecycle bridge that turns the substrate's own
//! `Tick` fan-out into the sweep's offered load.

use aether_actor::OutboundReply;
use aether_data::{Kind, KindId, MailboxId, ReplyContract, mailbox_id_from_name};
use aether_kinds::{ComponentCapabilities, HandlerCapability, Tick};
use aether_substrate::{BootError, Dispatch, NativeActor, NativeCtx, NativeInitCtx};

use super::{CountQuery, CountReport, Ping};

/// Lifecycle bridge for the sweep: subscribed to the `Tick` input
/// stream, it emits a burst of `burst` `Ping`s into the entry relay per
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
    entry: MailboxId,
    burst: u32,
    seq: u32,
    /// `Ping` mails emitted into the entry, for the run-end keep-up harvest
    /// (iamacoffeepot/aether#1233) — the offered load. `seq` wraps at `u32`
    /// for trace legibility; this is the honest cumulative count.
    sent: u64,
}

impl aether_actor::Addressable for TickSource {
    const NAMESPACE: &'static str = "mlat.ticksrc";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for TickSource {}
impl aether_actor::HandlesKind<Tick> for TickSource {}
impl aether_actor::Lifecycle<Self> for TickSource {
    /// `(entry, burst)`: the relay-0 mailbox and the number of `Ping`s to
    /// emit per `Tick` (`1` in `Latency`, `backlog` in `Saturate`).
    type Config = (MailboxId, u32);
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a>;
    fn init(config: Self::Config, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let (entry, burst) = config;
        Ok(Self { entry, burst, seq: 0, sent: 0 })
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
            ],
            ..ComponentCapabilities::default()
        }
    }

    fn dispatch(
        state: &mut Self,
        ctx: &mut NativeCtx<'_, aether_substrate::Manual, Self>,
        kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        // Run-end keep-up harvest (iamacoffeepot/aether#1233): the source
        // never receives a `Ping`, so its `received` is 0.
        if kind.0 == CountQuery::ID.0 {
            ctx.reply(&CountReport { sent: state.sent, received: 0 });
            return Some(());
        }
        if kind.0 != Tick::ID.0 {
            return None;
        }
        for _ in 0..state.burst {
            let bytes = Ping { seq: state.seq }.encode_into_bytes();
            state.seq = state.seq.wrapping_add(1);
            let _ = ctx.send_envelope_tracked(state.entry, Ping::ID, &bytes);
            state.sent += 1;
        }
        Some(())
    }
}

pub(super) const TICKSRC_NS: &str = "mlat.ticksrc";

/// Deterministic id for the single tick source (subname `"src"`).
// Harness derives the single tick-source id from its name to wire the topology
// — id derivation, not sibling-cap addressing.
#[must_use]
#[allow(clippy::disallowed_methods)]
pub fn ticksrc_id() -> MailboxId {
    MailboxId(mailbox_id_from_name(&format!("{TICKSRC_NS}:src")).0)
}
