//! ADR-0243 §9, #6895: an `ErasedActorRef` proves no row, so a held reply
//! never hands off to one, though it is a `Target` for every kind. `Held`
//! stands in for the substrate's ticket, whose `hand_off` takes this exact
//! target bound.

use std::marker::PhantomData;

use aether_actor::{ErasedActorRef, HandsOff};
use aether_data::ActorMail;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hands_off_erased.ask")]
struct Ask {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hands_off_erased.answered")]
struct Answered {
    seq: u32,
}

struct Held<R>(PhantomData<fn() -> R>);

impl<R: ActorMail> Held<R> {
    fn hand_off<K: ActorMail, I>(self, _target: impl HandsOff<K, R, I>, _payload: &K) {}
}

fn hand(target: ErasedActorRef) {
    Held::<Answered>(PhantomData).hand_off(target, &Ask { seq: 1 });
}

fn main() {}
