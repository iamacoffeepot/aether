//! ADR-0231 §11 happy path, both transports: a tell and a request, direct and
//! deferred, each name a protocol `P` as their ctx's sender, the marker each
//! emits names `P` as its sender requirement, and an actor that covers `P`
//! sends every one of them through the flat verb, through an `ActorRef`, and
//! up through its parent door, with nothing extra at the call site. A handler
//! whose ctx names no sender requires `Anyone`, so the erased ctx still sends
//! it through a reference.
//!
//! In a stating handler `ctx.sender()` is a `ProtocolRef<P>`, bound here by
//! type with no `Option` to unwrap and no cast, and the typed ctx passes to a
//! helper generic over the sender, which a plain handler calls too. A typed
//! ctx that still handed out an `Option`, or that no helper could take, fails
//! this file.
//!
//! The native receiver compiles only its always-on half, as in
//! `accepts_handler_intents_native.rs`: `runtime_feature` cfgs the
//! substrate-typed runtime impls out, since this crate has no
//! `aether-substrate` dev-dependency.

use aether_actor::{
    ActorInitError, ActorRef, Anyone, Erased, ErasedActorRef, HandlesKind, PathRefused, Pending, ProtocolRef,
    WasmActor, WasmCtx, WasmInitCtx, actor, protocol,
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
    fn on_take_focus(&mut self, ctx: &mut WasmCtx<'_, Self, FocusHolder>, _mail: TakeFocus) {
        let holder: ProtocolRef<FocusHolder> = ctx.sender();
        note(ctx);
        ctx.send_to(holder, &FocusLost);
    }

    #[handler::request]
    fn on_dial_self(&mut self, ctx: &mut WasmCtx<'_, Self, FocusHolder>, _mail: DialSelf) -> Dialed {
        ctx.send_to(ctx.sender(), &FocusLost);
        Dialed::Ok
    }

    #[handler::request]
    fn on_bind_self(&mut self, ctx: &mut WasmCtx<'_, Self, FocusHolder>, _mail: BindSelf) -> Pending<Dialed> {
        let _holder: ProtocolRef<FocusHolder> = ctx.sender();
        unimplemented!("a pass fixture is compiled, never dispatched")
    }

    #[handler::tell]
    fn on_plain(&mut self, ctx: &mut WasmCtx<'_, Self, Anyone>, _mail: Plain) {
        let _sender: Option<ErasedActorRef> = ctx.sender();
        note(ctx);
    }
}

/// A helper a stating handler and a plain one both call: it takes a ctx of
/// any sender.
fn note<A, S>(_ctx: &mut WasmCtx<'_, A, S>) {}

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
        ctx: &mut aether_substrate::actor::native::NativeCtx<'_, Self, FocusHolder>,
        _mail: TakeFocus,
    ) {
        let holder: ProtocolRef<FocusHolder> = ctx.sender();
        ctx.send_to(holder, &FocusLost);
    }

    #[handler::request]
    fn on_dial_self(
        _state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_, Self, FocusHolder>,
        _mail: DialSelf,
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
