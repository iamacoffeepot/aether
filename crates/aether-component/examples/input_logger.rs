// Smoke component for ADR-0021 input subscriptions. Resolves the
// four substrate-published input kinds (Tick / Key / MouseMove /
// MouseButton) and the `hub.claude.broadcast` sink, then on every
// received input event emits a `demo.input_observed { stream, code }`
// observation so the driving Claude session can see the dispatch
// land via `receive_mail`.
//
// The component itself doesn't subscribe to anything — that's the
// agent's job. From the harness:
//
//   1. `load_component` with this `.wasm` (or compile and use the
//      built artifact at `target/wasm32-unknown-unknown/release/
//      examples/input_logger.wasm`).
//   2. `send_mail` `aether.control.subscribe_input` with
//      `{ stream: "Key", mailbox: <id from load_result> }`.
//   3. Press keys in the substrate window (or otherwise drive the
//      platform layer).
//   4. `receive_mail` — each press lands as a
//      `demo.input_observed { stream: 1, code: <keycode> }`.
//
// `stream` is encoded as: 0=Tick, 1=Key, 2=MouseButton, 3=MouseMove.
// `code` carries the keycode for Key, the rounded cursor `x` for
// MouseMove, and `0` for the empty-payload kinds (Tick and
// MouseButton).
//
// ADR-0027 shape: the four input kinds live in `type Kinds`;
// dispatch reads from the per-component `KindTable` via
// `mail.is::<K>()` for signal kinds and `mail.decode_typed::<K>()`
// for payload-bearing ones.

use aether_component::{Component, Ctx, InitCtx, Mail, Sink};
use aether_kinds::{Key, MouseButton, MouseMove, Tick};
use aether_mail::Kind;
use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct InputObserved {
    pub stream: u32,
    pub code: u32,
}
impl Kind for InputObserved {
    const NAME: &'static str = "demo.input_observed";
}

pub struct InputLogger {
    observe: Sink<InputObserved>,
}

impl Component for InputLogger {
    type Kinds = (Tick, Key, MouseButton, MouseMove);

    fn init(ctx: &mut InitCtx<'_>) -> Self {
        InputLogger {
            observe: ctx.resolve_sink::<InputObserved>("hub.claude.broadcast"),
        }
    }

    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>) {
        let observation = if mail.is::<Tick>() {
            Some(InputObserved { stream: 0, code: 0 })
        } else if mail.is::<MouseButton>() {
            Some(InputObserved { stream: 2, code: 0 })
        } else if let Some(k) = mail.decode_typed::<Key>() {
            Some(InputObserved {
                stream: 1,
                code: k.code,
            })
        } else {
            mail.decode_typed::<MouseMove>().map(|m| InputObserved {
                stream: 3,
                code: m.x as u32,
            })
        };
        if let Some(obs) = observation {
            ctx.send(&self.observe, &obs);
        }
    }
}

aether_component::export!(InputLogger);
