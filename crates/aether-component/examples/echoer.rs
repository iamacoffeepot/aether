//! Smoke-test component for ADR-0017 (component-origin sender
//! handles). Receives `demo.request { seq }` and replies with
//! `demo.response { seq }` to whatever component sent it.
//!
//! ADR-0033 phase 3: uses `#[handlers]` as the only receive path.
//! The synthesized dispatcher reads `ctx.reply_to()` (threaded from the
//! inbound mail by `#[handlers]`) so the handler body never touches
//! `Mail<'_>` directly.

use aether_component::{Component, Ctx, InitCtx, KindId, handlers};
use aether_data::{Kind, Schema};
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

#[handlers]
impl Component for Echoer {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Echoer {
            response: ctx.resolve::<Response>(),
        }
    }

    #[handler]
    fn on_request(&mut self, ctx: &mut Ctx<'_>, req: Request) {
        if let Some(sender) = ctx.reply_to() {
            ctx.reply(sender, self.response, &Response { seq: req.seq });
        }
    }
}

aether_component::export!(Echoer);
