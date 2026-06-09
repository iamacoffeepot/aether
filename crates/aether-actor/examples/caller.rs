//! Smoke-test component for ADR-0017 (component-origin sender
//! handles). On every tick, sends `demo.request { seq }` to the
//! component registered as `"echoer"`. When the matching
//! `demo.response { seq }` arrives via the Component-variant sender
//! handle the substrate allocated, logs the round trip.
//!
//! Pre-issue-775 the example also broadcast `demo.observation { seq }`
//! to `hub.claude.broadcast` so the round trip was visible to the
//! driving Claude session. With `BroadcastCapability` retired the
//! observation send goes away; the component is now a pure
//! request/reply smoke without an observation channel.
//!
//! ADR-0033 phase 3: each kind gets its own `#[handler]` method on
//! the `#[actor]`-decorated impl. The peer-component send to
//! `"echoer"` rides the `Sender::send_to_named` string-keyed escape
//! hatch — the echoer's actor type lives in a sibling cdylib this
//! crate can't import without colliding FFI exports.

// Handlers take `&mut self` per the ADR-0033 / ADR-0038 dispatch
// contract; a stub handler that ignores both `self` and the mail
// payload still has to match the trampoline ABI.
#![allow(clippy::unused_self)]

use aether_actor::{BootError, FfiActor, FfiCtx, MailSender, Resolver, actor};
use aether_capabilities::LifecycleCapability;
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_data::{Kind, MailboxId, Schema};
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

pub struct Caller {
    next_seq: u32,
}

#[actor]
impl FfiActor for Caller {
    const NAMESPACE: &'static str = "caller";

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(Caller { next_seq: 0 })
    }

    //noinspection DuplicatedCode
    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        ctx.actor::<LifecycleCapability>()
            .subscribe(Tick::ID, MailboxId(ctx.mailbox_id()));
    }

    #[handler]
    fn on_tick(&mut self, ctx: &mut FfiCtx<'_>, _tick: Tick) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        ctx.send_to_named("echoer", &Request { seq });
    }

    #[handler]
    fn on_response(&mut self, _ctx: &mut FfiCtx<'_>, _resp: Response) {
        // Pre-#775 this broadcast `Observation { seq: resp.seq }` to
        // the hub fan-out mailbox. Broadcast retired with the cap.
    }
}

aether_actor::export!(Caller);
