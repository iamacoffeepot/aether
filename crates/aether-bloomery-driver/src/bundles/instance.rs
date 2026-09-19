//! One reactor bundle's lifecycle state and driver-tracked cursor (ADR-0226 decision 2).
//!
//! A digest serves one role: once claimed as a reactor, it never becomes a
//! program. The cursor is the last seq the root has folded or evaluated, so
//! a live instance expects `Event(N)` exactly when its cursor is `N-1`, and
//! activation warms from `cursor + 1` through `live_from - 1`.

use aether_bloomery_kinds::Detail;
use aether_data::MailboxId;

/// Lifecycle state of one reactor digest.
#[derive(Debug)]
pub enum InstanceState {
    /// A bundle artifact read is in flight.
    Reading,
    /// A load is in flight.
    Loading,
    /// Loaded; the root stays cached for the engine's life.
    Ready {
        /// Mailbox of the digest-named reactor root.
        root: MailboxId,
    },
    /// A fold failed; every later activation for it is rejected, and it is never reloaded.
    Poisoned {
        /// The recorded failure.
        reason: Detail,
    },
    /// Read, decode, or load failed, or the cursor can no longer be trusted; never retried.
    Unavailable {
        /// The recorded failure.
        reason: Detail,
    },
}

/// One reactor digest's state and driver-tracked cursor.
#[derive(Debug)]
pub struct Instance {
    /// Lifecycle state.
    pub state: InstanceState,
    /// Last seq the root has folded or evaluated.
    pub cursor: u64,
}

impl Instance {
    /// A claimed instance awaiting its artifact read, having folded nothing.
    pub fn new() -> Self {
        Self { state: InstanceState::Reading, cursor: 0 }
    }
}

impl Default for Instance {
    fn default() -> Self {
        Self::new()
    }
}
