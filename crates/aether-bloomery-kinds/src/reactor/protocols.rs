//! The protocol a reactor bundle root is typed by (ADR-0231 §4).

use aether_actor::Undeclared;

use crate::reactor::mail::{Event, Status, StatusQuery, Warm};

/// A bundle root that runs reactors.
///
/// The driver holds one per loaded bundle that declares the reactor role,
/// cast from the root's spawn reply, and sends it each `Warm`, `Event`, and
/// `StatusQuery`. A generated root publishes these rows only when its bundle
/// declares reactors. Its `Warm` and `Event` handlers read the reply target
/// so a cast does not fold state, so their rows are unchecked.
#[aether_actor::protocol]
pub trait ReactorRoot {
    /// Fold a journal prefix without reacting.
    fn warm(mail: Warm) -> Undeclared;
    /// Evaluate one live journal entry.
    fn event(mail: Event) -> Undeclared;
    /// Report the root's cursor and poison flag.
    fn status(mail: StatusQuery) -> Status;
}
