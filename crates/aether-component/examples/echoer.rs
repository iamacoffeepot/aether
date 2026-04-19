// Smoke-test component for ADR-0017 (component-origin sender
// handles). Receives `demo.request { seq: u32 }` and replies with
// `demo.pong { seq: u32 }` to whatever component sent it, using
// `ctx.reply` and the Component-variant handle the substrate
// allocates for intra-substrate mail.

use aether_component::{Component, Ctx, InitCtx, KindId, Mail};
use aether_mail::Kind;
use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Request {
    pub seq: u32,
}
impl Kind for Request {
    const NAME: &'static str = "demo.request";
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Response {
    pub seq: u32,
}
impl Kind for Response {
    const NAME: &'static str = "demo.response";
}

pub struct Echoer {
    request: KindId<Request>,
    response: KindId<Response>,
}

impl Component for Echoer {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Echoer {
            request: ctx.resolve::<Request>(),
            response: ctx.resolve::<Response>(),
        }
    }

    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>) {
        if let Some(req) = mail.decode(self.request)
            && let Some(sender) = mail.sender()
        {
            ctx.reply(sender, self.response, &Response { seq: req.seq });
        }
    }
}

aether_component::export!(Echoer);
