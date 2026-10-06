use aether_actor::{ActorRef, WasmCtx};
use spike_sender_requirement::{HalfConsole, TakeKeyFocus, Window};

fn take(ctx: &mut WasmCtx<'_, HalfConsole>, window: ActorRef<Window>) {
    ctx.send_to(window, &TakeKeyFocus { subtree: false });
}

fn main() {}
