use aether_actor::{ActorRef, ProtocolRef, Silent};
use spike_sender_requirement::{TakeKeyFocus, Window};

#[aether_actor::protocol]
trait Taker {
    fn take(mail: TakeKeyFocus);
}

fn launder(window: ActorRef<Window>) -> ProtocolRef<Taker> {
    let _ = core::marker::PhantomData::<Silent>;
    window.narrow::<Taker>()
}

fn main() {}
