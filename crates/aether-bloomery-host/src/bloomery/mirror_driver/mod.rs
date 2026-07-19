//! The `aether.bloomery.mirror` outbox-consumer / mirror-driver capability
//! (ADR-0149 migration step 1, third slice — issue #3499).
//!
//! Identity/runtime split (ADR-0122), mirroring the store: the
//! [`MirrorDriverCapability`] ZST identity lives here; the state-bearing
//! `#[runtime] impl NativeActor` — the poll timer, the drain → route → ack
//! state machine, and the config-gating — lives in `runtime.rs`.

use aether_actor::actor;
use aether_bloomery::Topic;

// The handler kinds the `#[actor]` macro references when it emits this cap's
// `HandlesKind` markers must be in scope here (the store's `pub use kinds::*`
// does the same): `DrainTick` from the runtime module, the two store reply
// kinds from `crate::store`.
use crate::store::{AckOutboxResult, DrainOutboxResult};
pub use runtime::{DrainTick, MirrorDriverState, TOPIC_VIEW_DOCUMENT};

/// Addressing identity for the outbox-consumer / mirror-driver capability.
#[actor(singleton)]
pub struct MirrorDriverCapability;

impl MirrorDriverCapability {
    /// The reducer-minted outbox topics this driver drains — its half of the
    /// producer/consumer pairing the topic tripwire checks against
    /// [`Topic::ALL`]. Only [`Topic::LANDING_RECEIPT`]: the host-local
    /// `TOPIC_VIEW_DOCUMENT` this driver also drains is not a reducer [`Topic`]
    /// and is deliberately outside `ALL`.
    pub const DRAINED_TOPICS: &'static [Topic] = &[Topic::LANDING_RECEIPT];
}

mod runtime;
