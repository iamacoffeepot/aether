//! The actor clock: the one clock every ctx's `now()` reads, guest and
//! native alike.
//!
//! There is one [`ActorClock`] per engine, held by the engine's
//! [`Mailer`](crate::Mailer) and fixed when that `Mailer` is built. Within one
//! engine a later reading is never less than an earlier one, and only the
//! difference between two readings means anything.
//!
//! Every engine runs [`ActorClock::Running`], which reads
//! `std::time::Instant`. It does not read the
//! [`TraceClock`](super::clock::TraceClock): that clock's TSC path carries a
//! scale error of a few parts per million and a read on one core can trail a
//! read on another, where std's clock promises exactly the ordering above.
//!
//! A test that measures time needs a clock it can move by hand, so a harness
//! is built with [`ActorClock::Stepped`] over a [`SteppedClock`] the test
//! keeps a clone of. The trace clock is left alone, so a stepped harness
//! still stamps real trace and cost timestamps.
//!
//! [`SteppedClock`] holds the one atomic here. The actor model's
//! one-mail-at-a-time does not cover it: its writer is the test thread, which
//! is no actor, and its readers are the pool workers running handlers. No
//! actor could own it either, since a handler needs the reading before it
//! returns and so cannot ask for it by mail. It exists only inside the
//! `Stepped` case, so an engine's read touches no atomic and no lock.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The clock an engine's actors read through `ctx.now()`. See the module
/// docs.
#[derive(Clone, Debug)]
pub enum ActorClock {
    /// Real elapsed time. Every engine uses this.
    Running {
        /// The moment this engine's clock started.
        anchor: Instant,
    },
    /// The clock a test moves by hand.
    Stepped(SteppedClock),
}

impl ActorClock {
    /// A clock anchored now that reads real elapsed time.
    #[must_use]
    pub fn running() -> Self {
        Self::Running { anchor: Instant::now() }
    }

    /// A clock that reads `stepped`, which moves only when the holder of
    /// another clone of it steps it.
    #[must_use]
    pub const fn stepped(stepped: SteppedClock) -> Self {
        Self::Stepped(stepped)
    }

    /// Nanoseconds since this clock's anchor.
    pub(crate) fn now_nanos(&self) -> u64 {
        match self {
            Self::Running { anchor } => saturating_nanos(anchor.elapsed()),
            Self::Stepped(stepped) => stepped.now_nanos(),
        }
    }
}

/// A clock that stands still until it is stepped. Every clone shares one
/// reading: a test keeps one clone and hands another to the harness it
/// builds, then steps its own.
#[derive(Clone, Debug, Default)]
pub struct SteppedClock {
    /// The clock's current reading in nanoseconds, shared between the test's
    /// handle and the engine's.
    nanos: Arc<AtomicU64>,
}

impl SteppedClock {
    /// A stepped clock reading zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Move the clock forward by `by`. Every clone reads the new value.
    pub fn step(&self, by: Duration) {
        // Relaxed: the reading is one independent counter, and the mail a
        // test sends after stepping is what orders its step before the
        // handler that reads it.
        self.nanos.fetch_add(saturating_nanos(by), Ordering::Relaxed);
    }

    fn now_nanos(&self) -> u64 {
        self.nanos.load(Ordering::Relaxed)
    }
}

/// `duration` in whole nanoseconds, pinned at `u64::MAX` past about 584
/// years.
fn saturating_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches a `SteppedClock` clone that copies the counter instead of
    // sharing it: the engine's handle would then never see a test's steps,
    // and every stepped scenario would read zero.
    #[test]
    fn a_stepped_clock_reads_the_sum_of_its_steps_through_another_clone() {
        let stepped = SteppedClock::new();
        let clock = ActorClock::stepped(stepped.clone());
        assert_eq!(clock.now_nanos(), 0);

        stepped.step(Duration::from_millis(7));
        stepped.step(Duration::from_nanos(5));
        assert_eq!(clock.now_nanos(), 7_000_005);
    }
}
