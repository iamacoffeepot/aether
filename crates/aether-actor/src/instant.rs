//! [`Instant`]: a reading of the engine's actor clock, the value `ctx.now()`
//! returns on a guest ctx and on a native ctx alike.
//!
//! The promise: within one engine process a later reading is never less than
//! an earlier one, and only the difference between two readings means
//! anything. It is not wall-clock time and not the game time a `Tick`
//! carries, and it means nothing in another engine or after a restart.
//!
//! It has no codec by design. It implements none of `Schema`, `Kind`,
//! `CrossesActors` or `CrossesWire`, so it is a field of no kind: it cannot be
//! mailed, saved as actor state across a republish, or put in a request
//! context. A measurement that spans two handlers keeps its start in actor
//! state, and a successor takes a fresh reading in `init` or `on_rehydrate`.
//! It has no public constructor and no accessor for the raw number, so the
//! only way to hold one is to ask a ctx.

use core::time::Duration;

/// A reading of the engine's actor clock. See the module docs for the
/// promise it carries and why it has no codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    /// Nanoseconds since the engine clock's anchor.
    nanos: u64,
}

impl Instant {
    /// A reading of `nanos` nanoseconds since the engine clock's anchor.
    pub(crate) const fn new(nanos: u64) -> Self {
        Self { nanos }
    }

    /// How long after `earlier` this reading was taken. Zero when `earlier`
    /// is the later of the two.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.nanos.saturating_sub(earlier.nanos))
    }
}

/// Mint an [`Instant`] from a reading of the engine clock. The native ctx
/// lives in another crate, so it mints through this door; the guest ctx uses
/// the crate-private constructor. The caller passes a reading of its engine's
/// one actor clock and nothing else.
#[doc(hidden)]
#[must_use]
pub const fn __mint_instant(nanos: u64) -> Instant {
    Instant::new(nanos)
}
