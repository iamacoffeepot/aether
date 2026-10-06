//! Issue 7532 compile spike. The receiver `Window` is written by hand so its
//! row for `TakeKeyFocus` can name `KeyFocusHolder` as `Contract::Sender`,
//! which is what `#[actor]` would emit from a handler that takes
//! `sender: ProtocolRef<KeyFocusHolder>`. Every sender is a real `#[actor]`
//! expansion, so the coverage is checked against emitted rows.

use aether_actor::__macro_internals::{KindId, ReplyContract};
use aether_actor::{
    ActorInitError, ActorRef, Addressable, Anyone, Contract, Contracts, HandlesKind, Here, One, ProtocolRef, Root, Row,
    Sends, Silent, There, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeInitCtx};

#[aether_data::kind(name = "spike.take_key_focus")]
pub struct TakeKeyFocus {
    pub subtree: bool,
}

#[aether_data::kind(name = "spike.raise")]
pub struct Raise {
    pub front: bool,
}

#[aether_data::kind(name = "spike.key_focus_gained")]
pub struct KeyFocusGained {
    pub window: u32,
}

#[aether_data::kind(name = "spike.key_focus_lost")]
pub struct KeyFocusLost {
    pub window: u32,
}

#[aether_actor::protocol]
pub trait KeyFocusHolder {
    fn gained(mail: KeyFocusGained);
    fn lost(mail: KeyFocusLost);
}

/// The receiver. `TakeKeyFocus` requires a `KeyFocusHolder` sender; `Raise`
/// requires nothing.
pub struct Window;

impl Addressable for Window {
    const NAMESPACE: &'static str = "spike.window";
    type Resolver = One;
}

impl Root for Window {}

impl HandlesKind<TakeKeyFocus> for Window {
    type Sender = KeyFocusHolder;
}

impl HandlesKind<Raise> for Window {
    type Sender = Anyone;
}

impl Contracts for Window {
    type Rows = (Row<TakeKeyFocus, Silent>, (Row<Raise, Silent>, ()));
    const CONTRACTS: &'static [(KindId, ReplyContract)] = &[];
}

impl Contract<TakeKeyFocus> for Window {
    type Reply = Silent;
    type Index = Here;
}

impl Contract<Raise> for Window {
    type Reply = Silent;
    type Index = There<Here>;
}

/// A wasm component that handles both notices.
pub struct Console;

#[actor(root, depends(Window))]
impl WasmActor for Console {
    type Config = ();
    const NAMESPACE: &'static str = "spike.console";

    fn init((): (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[aether_actor::handler::tell]
    fn on_gained(&mut self, _ctx: &mut WasmCtx<'_>, _mail: KeyFocusGained) {}

    #[aether_actor::handler::tell]
    fn on_lost(&mut self, _ctx: &mut WasmCtx<'_>, _mail: KeyFocusLost) {}
}

/// A wasm component that handles only one of the two notices.
pub struct HalfConsole;

#[actor(root, depends(Window))]
impl WasmActor for HalfConsole {
    type Config = ();
    const NAMESPACE: &'static str = "spike.half_console";

    fn init((): (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[aether_actor::handler::tell]
    fn on_gained(&mut self, _ctx: &mut WasmCtx<'_>, _mail: KeyFocusGained) {}
}

/// A native actor that handles both notices.
pub struct NativeConsole;

#[actor(root, depends(Window))]
impl NativeActor for NativeConsole {
    type Config = ();
    const NAMESPACE: &'static str = "spike.native_console";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::tell]
    fn on_gained(&mut self, _ctx: &mut NativeCtx<'_>, _mail: KeyFocusGained) {}

    #[aether_actor::handler::tell]
    fn on_lost(&mut self, _ctx: &mut NativeCtx<'_>, _mail: KeyFocusLost) {}
}

/// A native actor that handles neither notice.
pub struct NativeBystander;

#[actor(root, depends(Window))]
impl NativeActor for NativeBystander {
    type Config = ();
    const NAMESPACE: &'static str = "spike.native_bystander";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::tell]
    fn on_raise(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Raise) {}
}

/// Every wasm typed send verb, from a sender that covers the protocol.
pub fn wasm_holder_sends(ctx: &mut WasmCtx<'_, Console>, window: ActorRef<Window>) {
    ctx.send::<Window>(&TakeKeyFocus { subtree: false });
    ctx.send_detached::<Window>(&TakeKeyFocus { subtree: false });
    let _ = ctx.send_tracked::<Window>(&TakeKeyFocus { subtree: false });
    let _ = ctx.send_with_context::<Window>(&TakeKeyFocus { subtree: false }, Raise { front: true });
    ctx.send_to(window, &TakeKeyFocus { subtree: true });
    ctx.send_to(&window, &TakeKeyFocus { subtree: true });
    wasm_helper(&mut ctx.sends(), window);
}

fn wasm_helper(sends: &mut Sends<'_, Console>, window: ActorRef<Window>) {
    sends.send_to(window, &TakeKeyFocus { subtree: true });
}

/// A sender that does not cover the protocol still sends a kind whose
/// handler requires nothing, through every door.
pub fn wasm_half_sends_unrestricted(ctx: &mut WasmCtx<'_, HalfConsole>, window: ActorRef<Window>) {
    ctx.send::<Window>(&Raise { front: true });
    ctx.send_to(window, &Raise { front: true });
}

/// The erased ctx sends a kind whose handler requires nothing.
pub fn wasm_erased_sends_unrestricted(ctx: &mut WasmCtx<'_, aether_actor::Erased>, window: ActorRef<Window>) {
    ctx.send_to(window, &Raise { front: true });
}

/// Every native typed send verb, from a sender that covers the protocol.
pub fn native_holder_sends(ctx: &mut NativeCtx<'_, NativeConsole>, window: ActorRef<Window>) {
    ctx.send::<Window>(&TakeKeyFocus { subtree: false });
    ctx.send_detached::<Window>(&TakeKeyFocus { subtree: false });
    let _ = ctx.send_with_context::<Window>(&TakeKeyFocus { subtree: false }, Raise { front: true });
    ctx.send_to(window, &TakeKeyFocus { subtree: true });
    let _ = ctx.send_detached_to(window, &TakeKeyFocus { subtree: true });
    let _ = ctx.send_to_with_context(window, &TakeKeyFocus { subtree: true }, Raise { front: true });
    ctx.fanout([window], &TakeKeyFocus { subtree: true });
}

pub fn native_bystander_sends_unrestricted(ctx: &mut NativeCtx<'_, NativeBystander>, window: ActorRef<Window>) {
    ctx.send::<Window>(&Raise { front: true });
    ctx.send_to(window, &Raise { front: true });
}

/// A protocol reference to a holder still sends the notices: a protocol row
/// asks nothing of its sender.
pub fn window_notifies<A>(ctx: &mut NativeCtx<'_, A>, holder: ProtocolRef<KeyFocusHolder>) {
    ctx.send_to(holder, &KeyFocusGained { window: 1 });
    ctx.send_to(holder, &KeyFocusLost { window: 1 });
}
