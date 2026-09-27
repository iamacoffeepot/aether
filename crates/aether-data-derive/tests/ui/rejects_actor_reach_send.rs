//! A kind's reach is its narrowest field's (ADR-0242). A `Source` is a raw
//! sender route of actor reach, so a kind holding one, however deep and in
//! whichever variant, is not `ActorMail`, and a typed send of it fails at the
//! call site. The plain kind beside them still sends.

use aether_data::Source;

#[aether_data::kind(name = "test.reach.plain")]
struct Plain {
    label: Option<String>,
}

#[aether_data::kind(name = "test.reach.context")]
struct Context {
    label: String,
    reply_to: Option<Source>,
}

#[aether_data::kind(name = "test.reach.pending")]
enum Pending {
    Idle,
    Waiting { count: u32, reply_to: Source },
}

fn send<K: aether_data::ActorMail>(_: &K) {}

fn main() {
    send(&Plain { label: None });
    send(&Context { label: String::new(), reply_to: None });
    send(&Pending::Idle);
}
