// Smoke-test component for ADR-0017 (component-origin sender
// handles). On every `aether.tick`, sends `demo.request { seq }` to
// the component registered as `"echoer"`. When the reply
// `demo.response { seq }` arrives — via the Component-variant
// sender handle the substrate allocated — broadcasts
// `demo.observation { seq }` to `hub.claude.broadcast` so the round
// trip is visible to the driving Claude session.
//
// ADR-0027 shape: receive-side kinds (`Tick`, `Response`) live in
// `type Kinds` and dispatch via `mail.is::<Tick>()` /
// `mail.decode_typed::<Response>()`; send-side `Sink<K>` keeps its
// explicit field because mailbox names are data, not type.

use aether_component::{Component, Ctx, InitCtx, Mail, Sink};
use aether_kinds::Tick;
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

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Observation {
    pub seq: u32,
}
impl Kind for Observation {
    const NAME: &'static str = "demo.observation";
}

pub struct Caller {
    request: Sink<Request>,
    observe: Sink<Observation>,
    next_seq: u32,
}

impl Component for Caller {
    type Kinds = (Tick, Response);

    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Caller {
            request: ctx.resolve_sink::<Request>("echoer"),
            observe: ctx.resolve_sink::<Observation>("hub.claude.broadcast"),
            next_seq: 0,
        }
    }

    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>) {
        if mail.is::<Tick>() {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            ctx.send(&self.request, &Request { seq });
        } else if let Some(resp) = mail.decode_typed::<Response>() {
            ctx.send(&self.observe, &Observation { seq: resp.seq });
        }
    }
}

aether_component::export!(Caller);
