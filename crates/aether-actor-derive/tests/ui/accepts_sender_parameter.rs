//! ADR-0231 §11 happy path, both transports: a tell and a request, direct and
//! deferred, each take `sender: ProtocolRef<P>`, the marker each emits names
//! `P` as its sender requirement, and an actor that covers `P` sends every one
//! of them through the flat verb, through an `ActorRef`, and up through its
//! parent door, with nothing extra at the call site. A handler with no sender
//! parameter requires `Anyone`, so the erased ctx still sends it through a
//! reference.
//!
//! The native receiver compiles only its always-on half, as in
//! `accepts_handler_intents_native.rs`: `runtime_feature` cfgs the
//! substrate-typed runtime impls out, since this crate has no
//! `aether-substrate` dev-dependency.

use aether_actor::{
    ActorInitError, ActorRef, Anyone, Erased, HandlesKind, PathRefused, Pending, ProtocolRef, WasmActor, WasmCtx,
    WasmInitCtx, actor, protocol,
};

#[aether_data::kind(name = "test.sender_param.take_focus", copy)]
pub struct TakeFocus;

#[aether_data::kind(name = "test.sender_param.dial_self", copy)]
pub struct DialSelf;

#[aether_data::kind(name = "test.sender_param.bind_self", copy)]
pub struct BindSelf;

#[aether_data::kind(name = "test.sender_param.plain", copy)]
pub struct Plain;

#[aether_data::kind(name = "test.sender_param.dialed")]
pub enum Dialed {
    Ok,
    Err(PathRefused),
}

impl From<PathRefused> for Dialed {
    fn from(refused: PathRefused) -> Self {
        Self::Err(refused)
    }
}

#[aether_data::kind(name = "test.sender_param.focus_lost", copy)]
pub struct FocusLost;

#[aether_data::kind(name = "test.sender_param.trigger", copy)]
pub struct Trigger;

#[protocol]
pub trait FocusHolder {
    fn lost(mail: FocusLost);
}

struct Window;

#[actor(root)]
impl WasmActor for Window {
    const NAMESPACE: &'static str = "test.sender_param.window";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_take_focus(&mut self, ctx: &mut WasmCtx<'_>, _mail: TakeFocus, sender: ProtocolRef<FocusHolder>) {
        ctx.send_to(sender, &FocusLost);
    }

    #[handler::request]
    fn on_dial_self(&mut self, ctx: &mut WasmCtx<'_>, _mail: DialSelf, sender: ProtocolRef<FocusHolder>) -> Dialed {
        ctx.send_to(sender, &FocusLost);
        Dialed::Ok
    }

    #[handler::request]
    fn on_bind_self(
        &mut self,
        _ctx: &mut WasmCtx<'_>,
        _mail: BindSelf,
        _sender: ProtocolRef<FocusHolder>,
    ) -> Pending<Dialed> {
        unimplemented!("a pass fixture is compiled, never dispatched")
    }

    #[handler::tell]
    fn on_plain(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Plain) {}
}

pub struct NativeWindow;

pub struct NativeWindowState;

#[actor(singleton, root, runtime_feature = "native-sender-param")]
impl aether_substrate::actor::native::NativeActor for NativeWindow {
    type State = NativeWindowState;
    type Config = ();

    const NAMESPACE: &'static str = "test.sender_param.native_window";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<NativeWindowState, aether_substrate::chassis::error::BootError> {
        Ok(NativeWindowState)
    }

    #[handler::tell]
    fn on_take_focus(
        _state: &mut Self::State,
        ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _mail: TakeFocus,
        sender: ProtocolRef<FocusHolder>,
    ) {
        ctx.send_to(sender, &FocusLost);
    }

    #[handler::request]
    fn on_dial_self(
        _state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _mail: DialSelf,
        _sender: ProtocolRef<FocusHolder>,
    ) -> aether_substrate::actor::native::Pending<Dialed> {
        unimplemented!("a pass fixture is compiled, never dispatched")
    }
}

struct Holder;

#[actor(instanced, root, child_of(Window), depends(Window, NativeWindow))]
impl WasmActor for Holder {
    const NAMESPACE: &'static str = "test.sender_param.holder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_trigger(&mut self, ctx: &mut WasmCtx<'_>, _mail: Trigger) {
        ctx.send::<Window>(&TakeFocus);
        ctx.send::<Window>(&DialSelf);
        ctx.send_detached::<Window>(&BindSelf);
        ctx.send_to(ctx.actor_ref::<Window>(), &TakeFocus);
        ctx.send::<NativeWindow>(&TakeFocus);
        ctx.send::<NativeWindow>(&DialSelf);
        if let Some(parent) = ctx.parent() {
            parent.send(&TakeFocus);
        }
    }

    #[handler::tell]
    fn on_lost(&mut self, _ctx: &mut WasmCtx<'_>, _mail: FocusLost) {}

    #[handler::response]
    fn on_dialed(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Dialed) {}
}

fn send_plain_from_an_erased_ctx(ctx: &mut WasmCtx<'_, Erased>, window: ActorRef<Window>) {
    ctx.send_to(window, &Plain);
}

fn main() {
    fn requires<K: aether_data::Kind, P, A: HandlesKind<K, Sender = P>>() {}
    requires::<TakeFocus, FocusHolder, Window>();
    requires::<DialSelf, FocusHolder, Window>();
    requires::<BindSelf, FocusHolder, Window>();
    requires::<Plain, Anyone, Window>();
    requires::<TakeFocus, FocusHolder, NativeWindow>();
    requires::<DialSelf, FocusHolder, NativeWindow>();

    let _ = send_plain_from_an_erased_ctx;
    // The runtime impls that would construct the state are gated out by the
    // absent `native-sender-param` feature, so name it here rather than
    // suppressing the dead-code lint the fixture would otherwise trip.
    let _state = NativeWindowState;
}
