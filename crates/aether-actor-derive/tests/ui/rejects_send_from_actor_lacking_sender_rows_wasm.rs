//! ADR-0231 §11: a handler that takes `sender: ProtocolRef<P>` requires `P` of
//! whoever sends it that kind, and the typed sends check the sending actor
//! against it. `Window`'s take of key focus requires `FocusHolder`. `Holder`
//! handles both of its kinds, so its sends compile. `HalfHolder` lacks the
//! `FocusLost` handler, so its flat send, its send through an `ActorRef`, its
//! send to an inline child, and its send up through its parent door are each
//! an `E0277` naming that handler; a bound dropped from any of the four would
//! let it take focus and warn-drop the notice that it lost it.

use aether_actor::{ActorInitError, InlineChild, ProtocolRef, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};

#[aether_data::kind(name = "test.sender_rows.take_focus", copy)]
struct TakeFocus;

#[aether_data::kind(name = "test.sender_rows.focus_gained", copy)]
struct FocusGained;

#[aether_data::kind(name = "test.sender_rows.focus_lost", copy)]
struct FocusLost;

#[aether_data::kind(name = "test.sender_rows.trigger", copy)]
struct Trigger;

#[protocol]
trait FocusHolder {
    fn gained(mail: FocusGained);
    fn lost(mail: FocusLost);
}

struct Window;

#[actor(root)]
impl WasmActor for Window {
    const NAMESPACE: &'static str = "test.sender_rows.window";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_take_focus(&mut self, ctx: &mut WasmCtx<'_>, _mail: TakeFocus, sender: ProtocolRef<FocusHolder>) {
        ctx.send_to(sender, &FocusGained);
    }
}

struct Holder;

#[actor(root, depends(Window))]
impl WasmActor for Holder {
    const NAMESPACE: &'static str = "test.sender_rows.holder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_trigger(&mut self, ctx: &mut WasmCtx<'_>, _mail: Trigger) {
        ctx.send::<Window>(&TakeFocus);
        ctx.send_to(ctx.actor_ref::<Window>(), &TakeFocus);
    }

    #[handler::tell]
    fn on_gained(&mut self, _ctx: &mut WasmCtx<'_>, _mail: FocusGained) {}

    #[handler::tell]
    fn on_lost(&mut self, _ctx: &mut WasmCtx<'_>, _mail: FocusLost) {}
}

struct HalfHolder;

#[actor(instanced, root, child_of(Window), depends(Window))]
impl WasmActor for HalfHolder {
    const NAMESPACE: &'static str = "test.sender_rows.half_holder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_trigger(&mut self, ctx: &mut WasmCtx<'_>, _mail: Trigger) {
        ctx.send::<Window>(&TakeFocus);
        ctx.send_to(ctx.actor_ref::<Window>(), &TakeFocus);
        if let Some(parent) = ctx.parent() {
            parent.send(&TakeFocus);
        }
    }

    #[handler::tell]
    fn on_gained(&mut self, _ctx: &mut WasmCtx<'_>, _mail: FocusGained) {}
}

fn send_to_an_inline_child(ctx: &mut WasmCtx<'_, HalfHolder>, child: InlineChild<Window>) {
    child.send(ctx, &TakeFocus);
}

fn main() {
    let _ = send_to_an_inline_child;
}
