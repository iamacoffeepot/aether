use aether_actor::{ActorRef, MailSender, WasmCtx};
use spike_sender_requirement::{Console, TakeKeyFocus, Window};

fn take(ctx: &mut WasmCtx<'_, Console>, window: ActorRef<Window>) {
    MailSender::send_detached_to(ctx, window, &TakeKeyFocus { subtree: false });
}

fn main() {}
