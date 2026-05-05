//! Smoke-test component for ADR-0017 (component-origin sender
//! handles). On every tick, sends `demo.request { seq }` to the
//! component registered as `"echoer"`. When the matching
//! `demo.response { seq }` arrives via the Component-variant sender
//! handle the substrate allocated, broadcasts
//! `demo.observation { seq }` to `hub.claude.broadcast` so the round
//! trip is visible to the driving Claude session.
//!
//! ADR-0033 phase 3: each kind gets its own `#[handler]` method on
//! the `#[actor]`-decorated impl; `Mailbox<K>` still carries the
//! send-side mailbox name (data, not type).

use aether_actor::{BootError, Mailbox, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::{Kind, Schema};
use aether_kinds::Tick;
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

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, Kind, Schema)]
#[kind(name = "demo.observation")]
pub struct Observation {
    pub seq: u32,
}

pub struct Caller {
    request: Mailbox<Request>,
    observe: Mailbox<Observation>,
    next_seq: u32,
}

#[actor]
impl WasmActor for Caller {
    const NAMESPACE: &'static str = "caller";

    fn init(ctx: &mut WasmInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Caller {
            request: ctx.resolve_mailbox::<Request>("echoer"),
            observe: ctx.resolve_mailbox::<Observation>("hub.claude.broadcast"),
            next_seq: 0,
        })
    }

    #[handler]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        ctx.send(&self.request, &Request { seq });
    }

    #[handler]
    fn on_response(&mut self, ctx: &mut WasmCtx<'_>, resp: Response) {
        ctx.send(&self.observe, &Observation { seq: resp.seq });
    }
}

aether_actor::export!(Caller);
