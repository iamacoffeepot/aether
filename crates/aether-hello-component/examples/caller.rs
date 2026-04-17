// Smoke-test component for ADR-0017 (component-origin sender
// handles). On every `aether.tick`, sends `demo.request { seq }` to
// the component registered as `"echoer"`. When the reply
// `demo.response { seq }` arrives — via the Component-variant
// sender handle the substrate allocated — broadcasts
// `demo.observation { seq }` to `hub.claude.broadcast` so the round
// trip is visible to the driving Claude session.

use aether_component::{Component, Ctx, InitCtx, KindId, Mail, Sink};
use aether_mail::Kind;
use aether_substrate_mail::Tick;
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

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Observation {
    pub seq: u32,
}
impl Kind for Observation {
    const NAME: &'static str = "demo.observation";
}

pub struct Caller {
    tick: KindId<Tick>,
    response: KindId<Response>,
    request: Sink<Request>,
    observe: Sink<Observation>,
    next_seq: u32,
}

impl Component for Caller {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Caller {
            tick: ctx.resolve::<Tick>(),
            response: ctx.resolve::<Response>(),
            request: ctx.resolve_sink::<Request>("echoer"),
            observe: ctx.resolve_sink::<Observation>("hub.claude.broadcast"),
            next_seq: 0,
        }
    }

    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>) {
        if self.tick.matches(mail.kind()) {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            ctx.send(&self.request, &Request { seq });
        } else if let Some(resp) = mail.decode(self.response) {
            ctx.send(&self.observe, &Observation { seq: resp.seq });
        }
    }
}

aether_component::export!(Caller);
