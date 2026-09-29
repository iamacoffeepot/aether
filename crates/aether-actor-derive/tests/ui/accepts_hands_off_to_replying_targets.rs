//! ADR-0243 §9: a held `R` hands off to a target whose row for the payload's
//! kind replies exactly `R`: an `ActorRef<T>` whose handler for the kind
//! returns `R`, or a `ProtocolRef<P>` whose row for the kind is `Row<K, R>`,
//! each by value and by borrow. `Held` stands in for the substrate's ticket,
//! whose `hand_off` takes this exact target bound.

use std::marker::PhantomData;

use aether_actor::{ActorInitError, ActorRef, HandsOff, ProtocolRef, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};
use aether_data::ActorMail;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hands_off.ask")]
struct Ask {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hands_off.answered")]
struct Answered {
    seq: u32,
}

#[protocol]
trait Control {
    fn ask(mail: Ask) -> Answered;
}

struct Asked;

#[actor]
impl WasmActor for Asked {
    const NAMESPACE: &'static str = "test.hands_off.asked";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ask(&mut self, _ctx: &mut WasmCtx<'_>, mail: Ask) -> Answered {
        Answered { seq: mail.seq }
    }
}

struct Held<R>(PhantomData<fn() -> R>);

impl<R: ActorMail> Held<R> {
    fn hand_off<K: ActorMail, I>(self, _target: impl HandsOff<K, R, I>, _payload: &K) {}
}

fn held() -> Held<Answered> {
    Held(PhantomData)
}

fn hand(asked: ActorRef<Asked>, control: ProtocolRef<Control>) {
    held().hand_off(asked, &Ask { seq: 1 });
    held().hand_off(&asked, &Ask { seq: 2 });
    held().hand_off(control, &Ask { seq: 3 });
    held().hand_off(&control, &Ask { seq: 4 });
}

fn main() {
    let _ = hand;
}
