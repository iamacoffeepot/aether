use aether_actor::{ActorRef, Erased, WasmCtx};
use spike_sender_requirement::{TakeKeyFocus, Window};

fn take(ctx: &mut WasmCtx<'_, Erased>, window: ActorRef<Window>) {
    ctx.send_to(window, &TakeKeyFocus { subtree: false });
}

fn main() {}
