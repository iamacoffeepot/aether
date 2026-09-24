//! An `engine_only` kind carries no `ActorMail`, so a typed send of it fails
//! at the call site; the plain kind beside it still sends.

#[aether_data::kind(name = "test.engine_only", engine_only)]
struct Notice;

#[aether_data::kind(name = "test.actor_mail")]
struct Plain;

fn send<K: aether_data::ActorMail>(_: &K) {}

fn main() {
    send(&Plain);
    send(&Notice);
}
