//! ADR-0231 §11: a protocol row is covered only by a handler that asks nothing
//! of its sender. `Window` handles `TakeFocus` and requires `FocusHolder` of
//! its sender, so it does not cover `Taking`, whose one row is that kind:
//! narrowing a reference or a path to `Taking` is an `E0271`. A send through
//! a `ProtocolRef` checks nothing about the sending actor, so a coverage
//! bound that ignored the requirement would let any actor hold a
//! `ProtocolRef<Taking>` and take focus without the handlers the take needs.
//! A held reply handed off to an `ActorRef<Window>` is sent in the name of
//! whoever holds it, which no bound names, so the hand-off of `DialSelf` is
//! refused the same way.

use aether_actor::{
    ActorInitError, ActorPath, ActorRef, HandsOff, PathRefused, WasmActor, WasmCtx, WasmInitCtx, actor, protocol,
};

#[aether_data::kind(name = "test.narrow_sender.take_focus", copy)]
struct TakeFocus;

#[aether_data::kind(name = "test.narrow_sender.focus_lost", copy)]
struct FocusLost;

#[aether_data::kind(name = "test.narrow_sender.trigger", copy)]
struct Trigger;

#[aether_data::kind(name = "test.narrow_sender.dial_self", copy)]
struct DialSelf;

#[aether_data::kind(name = "test.narrow_sender.dialed")]
enum Dialed {
    Ok,
    Err(PathRefused),
}

impl From<PathRefused> for Dialed {
    fn from(refused: PathRefused) -> Self {
        Self::Err(refused)
    }
}

#[protocol]
trait FocusHolder {
    fn lost(mail: FocusLost);
}

#[protocol]
trait Taking {
    fn take(mail: TakeFocus);
}

struct Window;

#[actor(root)]
impl WasmActor for Window {
    const NAMESPACE: &'static str = "test.narrow_sender.window";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_take_focus(&mut self, ctx: &mut WasmCtx<'_, Self, FocusHolder>, _mail: TakeFocus) {
        ctx.send_to(ctx.sender(), &FocusLost);
    }

    #[handler::request]
    fn on_dial_self(&mut self, ctx: &mut WasmCtx<'_, Self, FocusHolder>, _mail: DialSelf) -> Dialed {
        ctx.send_to(ctx.sender(), &FocusLost);
        Dialed::Ok
    }
}

struct Bystander;

#[actor(root, depends(Window))]
impl WasmActor for Bystander {
    const NAMESPACE: &'static str = "test.narrow_sender.bystander";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_trigger(&mut self, ctx: &mut WasmCtx<'_>, _mail: Trigger) {
        let taking = ctx.actor_ref::<Window>().narrow::<Taking>();
        ctx.send_to(taking, &TakeFocus);
    }
}

fn takes_a_held_reply(_target: impl HandsOff<DialSelf, Dialed>) {}

fn hand_off_to_the_window(window: ActorRef<Window>) {
    takes_a_held_reply(window);
}

fn main() {
    let _path = ActorPath::<Window>::root().narrow::<Taking>();
    let _ = hand_off_to_the_window;
}
