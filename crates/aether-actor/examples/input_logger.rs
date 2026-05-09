//! Smoke component for ADR-0021 input subscriptions. Observes the
//! four substrate-published input kinds (Tick / Key / MouseMove /
//! MouseButton) and emits a `demo.input_observed { stream, code }`
//! observation to `hub.claude.broadcast` so the driving Claude
//! session sees each dispatch land via `receive_mail`.
//!
//! `stream` encoding: 0=Tick, 1=Key, 2=MouseButton, 3=MouseMove.
//! `code` carries the keycode for Key, the rounded cursor `x` for
//! MouseMove, and `0` for the empty-payload kinds (Tick and
//! MouseButton).
//!
//! Each input kind has its own `#[handler]` method. Issue 640 retired
//! the cap-side manifest auto-subscribe walker (and the macro-side
//! walker retired earlier in #403); components subscribe explicitly
//! via `ctx.subscribe_input::<K>()` in `init`.

use aether_actor::{BootError, FfiActor, FfiCtx, Resolver, actor};
use aether_capabilities::{BroadcastCapability, InputCapability};
use aether_data::{Kind, MailboxId, Schema};
use aether_kinds::{Key, MouseButton, MouseMove, SubscribeInput, Tick};
use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, Kind, Schema)]
#[kind(name = "demo.input_observed")]
pub struct InputObserved {
    pub stream: u32,
    pub code: u32,
}

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
        for kind in [Tick::ID, Key::ID, MouseMove::ID, MouseButton::ID] {
            input.send(&SubscribeInput { kind, mailbox: me });
        }
    }

    #[handler]
    fn on_tick(&mut self, ctx: &mut FfiCtx<'_>, _tick: Tick) {
        ctx.actor::<BroadcastCapability>()
            .send(&InputObserved { stream: 0, code: 0 });
    }

    #[handler]
    fn on_key(&mut self, ctx: &mut FfiCtx<'_>, key: Key) {
        ctx.actor::<BroadcastCapability>().send(&InputObserved {
            stream: 1,
            code: key.code,
        });
    }

    #[handler]
    fn on_mouse_button(&mut self, ctx: &mut FfiCtx<'_>, _mb: MouseButton) {
        ctx.actor::<BroadcastCapability>()
            .send(&InputObserved { stream: 2, code: 0 });
    }

    #[handler]
    fn on_mouse_move(&mut self, ctx: &mut FfiCtx<'_>, m: MouseMove) {
        ctx.actor::<BroadcastCapability>().send(&InputObserved {
            stream: 3,
            code: m.x as u32,
        });
    }
}

aether_actor::export!(InputLogger);
