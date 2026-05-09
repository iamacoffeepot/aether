//! Smoke-test component for ADR-0017 (component-origin sender
//! handles). Receives `demo.request { seq }` and replies with
//! `demo.response { seq }` to whatever component sent it.
//!
//! ADR-0033 phase 3: uses `#[actor]` as the only receive path.
//! The synthesized dispatcher reads `ctx.reply_target()` (threaded from the
//! inbound mail by `#[actor]`) so the handler body never touches
//! `Mail<'_>` directly.

use aether_actor::{BootError, FfiActor, FfiCtx, KindId, Resolver, actor};
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

#[actor]
impl FfiActor for Echoer {
    const NAMESPACE: &'static str = "echoer";

    fn init<C>(ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(Echoer {
            response: ctx.resolve::<Response>(),
        })
    }

    #[handler]
    fn on_request(&mut self, ctx: &mut FfiCtx<'_>, req: Request) {
        if let Some(sender) = ctx.reply_target() {
            ctx.reply_kind(sender, self.response, &Response { seq: req.seq });
        }
    }
}

aether_actor::export!(Echoer);
