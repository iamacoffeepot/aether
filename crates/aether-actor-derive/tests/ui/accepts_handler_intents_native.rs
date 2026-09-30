//! #7201, native half: the four intent words pass their signature checks on a
//! native actor, including a `response` context parameter in both forms and an
//! `event` over a batched `&[K]` slice, and each handler keeps its
//! `HandlesKind<K>` marker.
//!
//! This crate has no `aether-substrate` dev-dependency, so, as in
//! `accepts_cfg_gated_handler_native.rs`, the split shape plus
//! `runtime_feature` cfgs the substrate-typed runtime impls out and only the
//! always-on half compiles: the `Addressable` impl and one `HandlesKind<K>` per
//! handler. The native dispatch arms, the context take among them, are driven
//! by the `response` tests in `aether-substrate`'s native ctx suite.

use aether_actor::actor;

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.native_intents.ask")]
pub struct Ask {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.native_intents.later")]
pub struct Later {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.native_intents.answer")]
pub struct Answer {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.native_intents.poke")]
pub struct Poke {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.native_intents.tick")]
pub struct Tick {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.native_intents.bare_reply")]
pub struct BareReply {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.native_intents.required_reply")]
pub struct RequiredReply {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.native_intents.optional_reply")]
pub struct OptionalReply {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.native_intents.context")]
pub struct AskContext {
    seq: u32,
}

pub struct NativeIntents;

struct NativeIntentsState {
    seen: u32,
}

#[actor(singleton, runtime_feature = "native-intents")]
impl aether_substrate::actor::native::NativeActor for NativeIntents {
    type State = NativeIntentsState;
    type Config = ();

    const NAMESPACE: &'static str = "test.native_intents";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<NativeIntentsState, aether_substrate::chassis::error::BootError> {
        Ok(NativeIntentsState { seen: 0 })
    }

    #[handler::request]
    fn on_ask(state: &mut Self::State, _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>, ask: Ask) -> Answer {
        Answer { seq: ask.seq + state.seen }
    }

    #[handler::request]
    fn on_later(
        _state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _later: Later,
    ) -> aether_substrate::actor::native::Pending<Answer> {
        unimplemented!("a pass fixture is compiled, never dispatched")
    }

    #[handler::tell]
    fn on_poke(state: &mut Self::State, _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>, poke: Poke) {
        state.seen = poke.seq;
    }

    #[handler::event]
    fn on_tick(state: &mut Self::State, _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>, tick: &[Tick]) {
        state.seen += tick.len() as u32;
    }

    #[handler::response]
    fn on_bare_reply(
        state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        reply: BareReply,
    ) {
        state.seen += reply.seq;
    }

    #[handler::response]
    fn on_required_reply(
        state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        reply: RequiredReply,
        context: AskContext,
    ) {
        state.seen += reply.seq + context.seq;
    }

    #[handler::response]
    fn on_optional_reply(
        state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        reply: OptionalReply,
        context: Option<AskContext>,
    ) {
        state.seen += reply.seq + context.map_or(0, |c| c.seq);
    }
}

fn main() {
    fn handles<K: aether_data::Kind, A: aether_actor::HandlesKind<K>>() {}
    handles::<Ask, NativeIntents>();
    handles::<Later, NativeIntents>();
    handles::<Poke, NativeIntents>();
    handles::<Tick, NativeIntents>();
    handles::<BareReply, NativeIntents>();
    handles::<RequiredReply, NativeIntents>();
    handles::<OptionalReply, NativeIntents>();

    // The runtime impls that would construct the state are gated out by the
    // absent `native-intents` feature, so name it here rather than suppressing
    // the dead-code lint the fixture would otherwise trip.
    let state = NativeIntentsState { seen: 0 };
    assert_eq!(state.seen, 0);
}
