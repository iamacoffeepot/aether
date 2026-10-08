//! ADR-0231 §11, native receiver: a native actor's handler states its sender
//! requirement the same way, and the native expansion writes it onto the
//! `HandlesKind<K>` marker a sender is checked against. `Dialer` requires
//! `Consumer` of whoever sends it `DialSelf`. `Half` lacks the `Closed`
//! handler, so its send is an `E0277` naming that handler; an expansion that
//! dropped the requirement from the marker would let it dial and warn-drop
//! every close notice.
//!
//! This crate has no `aether-substrate` dev-dependency, so, as in
//! `accepts_handler_intents_native.rs`, `runtime_feature` cfgs the
//! substrate-typed runtime impls out and only the always-on half compiles:
//! the markers a send is checked against. The native verbs carry the same
//! bound, shown by the `compile_fail` doctest on `NativeCtx::send`, and the
//! native arm's cast is driven by `aether-component`'s
//! `tests/harness_sender_requirement.rs`.

use aether_actor::{ActorInitError, PathRefused, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};

#[aether_data::kind(name = "test.native_sender_rows.dial_self", copy)]
pub struct DialSelf;

#[aether_data::kind(name = "test.native_sender_rows.dialed")]
pub enum Dialed {
    Ok,
    Err(PathRefused),
}

impl From<PathRefused> for Dialed {
    fn from(refused: PathRefused) -> Self {
        Self::Err(refused)
    }
}

#[aether_data::kind(name = "test.native_sender_rows.data", copy)]
pub struct Data;

#[aether_data::kind(name = "test.native_sender_rows.closed", copy)]
pub struct Closed;

#[aether_data::kind(name = "test.native_sender_rows.trigger", copy)]
pub struct Trigger;

#[protocol]
pub trait Consumer {
    fn data(mail: Data);
    fn closed(mail: Closed);
}

pub struct Dialer;

pub struct DialerState;

#[actor(singleton, root, runtime_feature = "native-sender-rows")]
impl aether_substrate::actor::native::NativeActor for Dialer {
    type State = DialerState;
    type Config = ();

    const NAMESPACE: &'static str = "test.native_sender_rows.dialer";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<DialerState, aether_substrate::chassis::error::BootError> {
        Ok(DialerState)
    }

    #[handler::request]
    fn on_dial_self(
        _state: &mut Self::State,
        ctx: &mut aether_substrate::actor::native::NativeCtx<'_, Self, Consumer>,
        _mail: DialSelf,
    ) -> Dialed {
        ctx.send_to(ctx.sender(), &Data);
        Dialed::Ok
    }
}

struct Half;

#[actor(root, depends(Dialer))]
impl WasmActor for Half {
    const NAMESPACE: &'static str = "test.native_sender_rows.half";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_trigger(&mut self, ctx: &mut WasmCtx<'_>, _mail: Trigger) {
        ctx.send::<Dialer>(&DialSelf);
    }

    #[handler::tell]
    fn on_data(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Data) {}

    #[handler::response]
    fn on_dialed(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Dialed) {}
}

fn main() {
    // The runtime impls that would construct the state are gated out by the
    // absent `native-sender-rows` feature, so name it here rather than
    // suppressing the dead-code lint the fixture would otherwise trip.
    let _state = DialerState;
}
