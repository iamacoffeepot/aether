//! Smoke component for ADR-0021 input subscriptions. Observes the
//! substrate-published input kinds (Key / `MouseMove` / `MouseButton`)
//! and counts each dispatch. `Tick` is a frame-lifecycle stage
//! (`aether.lifecycle`, ADR-0082), not an input stream, so it is not
//! part of this input-streams demo.
//!
//! Pre-issue-775 the example emitted a `demo.input_observed { stream,
//! code }` to `hub.claude.broadcast` so the driving Claude session
//! saw each dispatch land via `receive_mail`. With
//! `BroadcastCapability` retired the broadcast goes away; handlers
//! still run (and trigger any tracing the substrate captures), but no
//! observation kind is emitted.
//!
//! Each input kind has its own `#[handler]` method. Issue 640 retired
//! the cap-side manifest auto-subscribe walker (and the macro-side
//! walker retired earlier in #403); components subscribe from the
//! `wire` hook by sending a `SubscribeInput { kind, mailbox }` to the
//! `InputCapability`.

// Stateless logger: each `#[handler]` keeps `&mut self` for the
// ADR-0033 / ADR-0038 dispatch ABI but doesn't need any field access.
#![allow(clippy::unused_self)]

use aether_actor::{BootError, FfiActor, FfiCtx, Resolver, actor};
use aether_capabilities::InputCapability;
use aether_data::{Kind, MailboxId};
use aether_kinds::{Key, MouseButton, MouseMove, SubscribeInput};

pub struct InputLogger;

#[actor]
impl FfiActor for InputLogger {
    const NAMESPACE: &'static str = "input_logger";

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(InputLogger)
    }

    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        let me = MailboxId(ctx.mailbox_id());
        let input = ctx.actor::<InputCapability>();
        for kind in [Key::ID, MouseMove::ID, MouseButton::ID] {
            input.send(&SubscribeInput { kind, mailbox: me });
        }
    }

    #[handler]
    fn on_key(&mut self, _ctx: &mut FfiCtx<'_>, _key: Key) {}

    #[handler]
    fn on_mouse_button(&mut self, _ctx: &mut FfiCtx<'_>, _mb: MouseButton) {}

    #[handler]
    fn on_mouse_move(&mut self, _ctx: &mut FfiCtx<'_>, _m: MouseMove) {}
}

aether_actor::export!(InputLogger);
