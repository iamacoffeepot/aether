use aether_actor::WasmCtx;
use spike_sender_requirement::{HalfConsole, TakeKeyFocus, Window};

fn take(ctx: &mut WasmCtx<'_, HalfConsole>) {
    ctx.send::<Window>(&TakeKeyFocus { subtree: false });
}

fn main() {}
