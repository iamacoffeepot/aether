//! The driver's clock: one reserved, driver-native program that waits for a
//! journal time (ADR-0245).
//!
//! `clock.until` is named through the reserved [`CLOCK`] head, which the
//! driver resolves itself before it consults any binding, and is recorded
//! under the reserved [`CLOCK_BUNDLE`] identity. The driver never loads,
//! queues, or invokes anything for it: it arms the request on its own timer
//! heap and, once the due time has passed, records the request's
//! `Transition` with a [`Fired`] result. The journal is the timer table: a
//! `Requested` naming `clock.until` is a timer set, and its `Transition` is
//! the timer firing.
//!
//! The contract: the `Transition` is never recorded before `due_millis` in
//! journal time, and it is recorded within about one driver tick after it
//! while the engine is healthy, with no upper bound under load. A due time
//! more than [`MAX_DUE_AHEAD_MILLIS`] after the request's own recorded time
//! is refused with a `Fault`. There is no cancel: a timeout is a race
//! between the work and the timer, and the loser's outcome is ignored by
//! the rule that reads it.

use crate::{Digest, Head, OpaqueBytes};

/// The reserved program head a rule or a native caller names the clock by.
///
/// The driver resolves it to [`CLOCK_BUNDLE`] before it reads the journal's
/// head bindings, so a binding recorded under this name is never consulted.
pub const CLOCK: Head<OpaqueBytes> = Head::new("bloomery.clock");

/// The reserved bundle identity every clock request is recorded under.
///
/// It is not a content hash: no bundle is stored or loaded under it, and its
/// bytes spell its purpose so no hash can collide with it by accident.
pub const CLOCK_BUNDLE: Digest = Digest::from_bytes(*b"aether.bloomery.clock.native.v1\0");

/// The furthest a due time may lie after its request's recorded time: 7 days.
pub const MAX_DUE_AHEAD_MILLIS: u64 = 7 * 24 * 60 * 60 * 1000;

/// `clock.until`'s input: wait until journal time reaches `due_millis`.
///
/// `due_millis` is absolute journal time in unix milliseconds, computed from
/// recorded time (an entry's `recorded_at_millis`), never from a live clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.clock.until")]
pub struct Until {
    /// The journal time the timer fires at.
    pub due_millis: u64,
}

/// `clock.until`'s result: the timer set for `due_millis` fired.
///
/// The `Transition` that records it is stamped at or after `due_millis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.clock.fired")]
pub struct Fired {
    /// The due time the timer was set for.
    pub due_millis: u64,
}
