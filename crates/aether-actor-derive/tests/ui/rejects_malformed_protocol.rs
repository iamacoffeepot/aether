//! ADR-0231 §2: `#[protocol]` takes no arguments and a trait of row
//! signatures `fn name(mail: K) -> O;` / `fn name(mail: K);`. Every violation is
//! reported at its own span: an attribute argument, a receiver, a default body,
//! a method generic, `-> Pending<O>` (the row is spelled `-> O`), a kind listed
//! twice, and an empty trait. The expansion refuses before any row type is
//! resolved, so the names below need not exist.

use aether_actor::protocol;

#[protocol(includes(Pingable))]
trait WithArgument {
    fn ping(mail: Ping) -> Pong;
}

#[protocol]
trait Malformed {
    fn receiver(&self, mail: Ping) -> Pong;
    fn body(mail: Note) {}
    fn generic<T>(mail: Tick);
    fn deferred(mail: Load) -> Pending<Loaded>;
    fn duplicate(mail: Ping);
}

#[protocol]
trait Empty {}

fn main() {}
