//! ADR-0243 §9: a held `R` hands off only to a row that replies exactly `R`.
//! An actor row replying another kind, a protocol row replying another kind,
//! and a silent row are each an `E0277`. `Held` stands in for the
//! substrate's ticket, whose `hand_off` takes this exact target bound.

use std::marker::PhantomData;

use aether_actor::{ActorInitError, ActorRef, HandsOff, ProtocolRef, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};
use aether_data::ActorMail;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hands_off_reply.ask")]
struct Ask {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hands_off_reply.note")]
struct Note {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hands_off_reply.answered")]
struct Answered {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hands_off_reply.other")]
struct Other {
    seq: u32,
}

#[protocol]
trait Control {
    fn ask(mail: Ask) -> Answered;
}

struct Asked;

#[actor]
impl WasmActor for Asked {
    const NAMESPACE: &'static str = "test.hands_off_reply.asked";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ask(&mut self, _ctx: &mut WasmCtx<'_>, mail: Ask) -> Answered {
        Answered { seq: mail.seq }
    }

    #[handler::single]
    fn on_note(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Note) {}
}

struct Held<R>(PhantomData<fn() -> R>);

impl<R: ActorMail> Held<R> {
    fn hand_off<K: ActorMail, I>(self, _target: impl HandsOff<K, R, I>, _payload: &K) {}
}

fn hand(asked: ActorRef<Asked>, control: ProtocolRef<Control>) {
    Held::<Other>(PhantomData).hand_off(asked, &Ask { seq: 1 });
    Held::<Other>(PhantomData).hand_off(control, &Ask { seq: 2 });
    Held::<Answered>(PhantomData).hand_off(asked, &Note { seq: 3 });
}

fn main() {}
