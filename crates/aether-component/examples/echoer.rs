// Smoke-test component for ADR-0017 (component-origin sender
// handles). Receives `demo.request { seq: u32 }` and replies with
// `demo.pong { seq: u32 }` to whatever component sent it, using
// `ctx.reply` and the Component-variant handle the substrate
// allocates for intra-substrate mail.
//
// ADR-0027 shape: `Request` is declared in `type Kinds`, dispatched
// via `mail.decode_typed::<Request>()`, and the reply still uses an
// explicit `KindId<Response>` because `Ctx::reply` takes one. (The
// reply path could be type-driven too in a follow-up; v1 keeps it
// explicit so the receive-side cleanup is the only delta this diff
// demonstrates.)

use aether_component::{Component, Ctx, InitCtx, KindId, Mail};
use aether_mail::{Kind, Schema};
use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, Kind, Schema)]
#[kind(name = "demo.request")]
pub struct Request {
    pub seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, Kind, Schema)]
#[kind(name = "demo.response")]
pub struct Response {
    pub seq: u32,
}

pub struct Echoer {
    response: KindId<Response>,
}

impl Component for Echoer {
    type Kinds = (Request,);

    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Echoer {
            response: ctx.resolve::<Response>(),
        }
    }

    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>) {
        if let Some(req) = mail.decode_typed::<Request>()
            && let Some(sender) = mail.sender()
        {
            ctx.reply(sender, self.response, &Response { seq: req.seq });
        }
    }
}

aether_component::export!(Echoer);
