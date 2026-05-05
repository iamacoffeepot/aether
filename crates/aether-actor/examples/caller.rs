//! Smoke-test component for ADR-0017 (component-origin sender
//! handles). On every tick, sends `demo.request { seq }` to the
//! component registered as `"echoer"`. When the matching
//! `demo.response { seq }` arrives via the Component-variant sender
//! handle the substrate allocated, broadcasts
//! `demo.observation { seq }` to `hub.claude.broadcast` so the round
//! trip is visible to the driving Claude session.
//!
//! ADR-0033 phase 3: each kind gets its own `#[handler]` method on
//! the `#[actor]`-decorated impl. ADR-0075 actor-typed sender API:
//! the broadcast send goes through `ctx.actor::<BroadcastCapability>()`
//! (typed receiver, gates `HandlesKind<K>` at the call site), and
//! the peer-component send to `"echoer"` rides the `Sender::send_to_named`
//! string-keyed escape hatch — the echoer's actor type lives in a
//! sibling cdylib this crate can't import without colliding FFI
//! exports.

use aether_actor::{BootError, Sender, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::BroadcastCapability;
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
    next_seq: u32,
}

#[actor]
impl WasmActor for Caller {
    const NAMESPACE: &'static str = "caller";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Caller { next_seq: 0 })
    }

    #[handler]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        ctx.send_to_named("echoer", &Request { seq });
    }

    #[handler]
    fn on_response(&mut self, ctx: &mut WasmCtx<'_>, resp: Response) {
        ctx.actor::<BroadcastCapability>()
            .send(&Observation { seq: resp.seq });
    }
}

aether_actor::export!(Caller);
