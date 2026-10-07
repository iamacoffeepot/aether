//! ADR-0231 §11: a request whose ctx names a protocol `P` as its sender is
//! answered by the engine when its sender does not cover `P`, with the reply
//! built from a `PathRefused` naming the sender. A reply that is not
//! `From<PathRefused>` fails to compile at the handler's return type; without
//! the bound a refused request would be left unanswered and its caller would
//! wait on a reply that never comes.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};

#[aether_data::kind(name = "test.sender_reply.dial_self", copy)]
struct DialSelf;

#[aether_data::kind(name = "test.sender_reply.dialed", copy)]
struct Dialed;

#[aether_data::kind(name = "test.sender_reply.closed", copy)]
struct Closed;

#[protocol]
trait Consumer {
    fn closed(mail: Closed);
}

struct Dialer;

#[actor(root)]
impl WasmActor for Dialer {
    const NAMESPACE: &'static str = "test.sender_reply.dialer";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::request]
    fn on_dial_self(&mut self, ctx: &mut WasmCtx<'_, Self, Consumer>, _mail: DialSelf) -> Dialed {
        ctx.send_to(ctx.sender(), &Closed);
        Dialed
    }
}

fn main() {}
