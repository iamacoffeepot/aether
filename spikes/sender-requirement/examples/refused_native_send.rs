use aether_substrate::NativeCtx;
use spike_sender_requirement::{NativeBystander, TakeKeyFocus, Window};

fn take(ctx: &mut NativeCtx<'_, NativeBystander>) {
    ctx.send::<Window>(&TakeKeyFocus { subtree: false });
}

fn main() {}
