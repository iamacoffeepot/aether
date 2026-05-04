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
//! ADR-0033 phase 3: each input kind has its own `#[handler]`
//! method. `#[handlers]` auto-subscribes every `K::IS_INPUT` handler
//! kind at init, so the test harness just loads the component and
//! starts driving input — no manual `subscribe_input` needed.

use aether_component::{Component, Ctx, InitCtx, Mailbox, handlers};
use aether_data::{Kind, Schema};
use aether_kinds::{Key, MouseButton, MouseMove, Tick};
use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, Kind, Schema)]
#[kind(name = "demo.input_observed")]
pub struct InputObserved {
    pub stream: u32,
    pub code: u32,
}

pub struct InputLogger {
    observe: Mailbox<InputObserved>,
}

#[handlers]
impl Component for InputLogger {
    const NAMESPACE: &'static str = "input_logger";

    fn init(ctx: &mut InitCtx<'_>) -> Self {
        InputLogger {
            observe: ctx.resolve_mailbox::<InputObserved>("hub.claude.broadcast"),
        }
    }

    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        ctx.send(&self.observe, &InputObserved { stream: 0, code: 0 });
    }

    #[handler]
    fn on_key(&mut self, ctx: &mut Ctx<'_>, key: Key) {
        ctx.send(
            &self.observe,
            &InputObserved {
                stream: 1,
                code: key.code,
            },
        );
    }

    #[handler]
    fn on_mouse_button(&mut self, ctx: &mut Ctx<'_>, _mb: MouseButton) {
        ctx.send(&self.observe, &InputObserved { stream: 2, code: 0 });
    }

    #[handler]
    fn on_mouse_move(&mut self, ctx: &mut Ctx<'_>, m: MouseMove) {
        ctx.send(
            &self.observe,
            &InputObserved {
                stream: 3,
                code: m.x as u32,
            },
        );
    }
}

aether_component::export!(InputLogger);
