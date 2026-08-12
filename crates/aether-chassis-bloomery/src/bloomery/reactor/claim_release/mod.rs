//! The claim-release reactor capability (ADR-0179).
//!
//! The executing half of an authorized orphan-claim release: it drains the
//! reducer's `topic:orphan_claim_release` outbox topic and runs the source
//! port's expected-holder compare-and-swap, admitting the terminal result back
//! through the control core. The reducer decides *that* a release was
//! authorized; nothing but this reactor makes it happen.
//!
//! Mounted only in the GitHub branch of the chassis: the whole point of the
//! release is a shared repository's ref namespace, and a runtime-only build has
//! none to reach. The identity/runtime split follows ADR-0122 — this ZST is the
//! addressing identity; the state-bearing logic is [`runtime`].

use aether_actor::actor;
use aether_bloomery::Topic;

use crate::bloomery::SourceShell;

pub use runtime::{ClaimReleaseReactorState, ClaimReleaseTick};

/// Composer-supplied parts for the claim-release reactor.
pub struct ClaimReleaseReactorSetup {
    /// The connected source shell, or `None` for an unconfigured bin (which
    /// mounts the reactor disabled).
    pub source: Option<SourceShell>,
    /// The store the outbox topic is drained from.
    pub store_path: String,
    /// How often to wake and drain.
    pub poll_interval_secs: u64,
}

/// Addressing identity for the claim-release reactor capability.
#[actor(singleton, root)]
pub struct ClaimReleaseReactorCapability;

impl ClaimReleaseReactorCapability {
    /// The outbox topics this reactor drains — its half of the producer/reactor
    /// pairing the topic tripwire checks against [`Topic::ALL`]. It is the sole
    /// drainer of [`Topic::OrphanClaimRelease`].
    pub const DRAINED_TOPICS: &'static [Topic] = &[Topic::OrphanClaimRelease];
}

mod runtime;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
