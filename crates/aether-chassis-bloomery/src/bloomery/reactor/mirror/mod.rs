//! The `aether.bloomery.mirror` outbox reactor capability (ADR-0149 migration
//! step 1, third slice — issue #3499).
//!
//! Identity/runtime split (ADR-0122), mirroring the store: the
//! [`MirrorReactorCapability`] ZST identity lives here; the state-bearing
//! `#[runtime] impl NativeActor` — the poll timer, the drain → route → ack
//! state machine, and the config-gating — lives in `runtime.rs`.

use aether_actor::actor;
use aether_bloomery::Topic;

// The handler kinds the `#[actor]` macro references when it emits this cap's
// `HandlesKind` markers must be in scope here (the store's `pub use kinds::*`
// does the same): `DrainTick` from the runtime module, the two store reply
// kinds from `crate::store`.
use crate::bloomery::{ProjectionShell, SourceReplicaShell, SourceShell};
use crate::store::{AckOutboxResult, DrainOutboxResult};
pub use runtime::{DrainTick, MirrorReactorState};

pub struct MirrorReactorSetup {
    pub projection: Option<ProjectionShell>,
    pub source: Option<SourceShell>,
    pub replica: Option<SourceReplicaShell>,
    pub poll_interval_secs: u64,
    pub repository: Option<(String, String)>,
}

/// Addressing identity for the outbox reactor capability.
#[actor(singleton, root)]
pub struct MirrorReactorCapability;

impl MirrorReactorCapability {
    /// The outbox topics this reactor drains — its half of the producer/reactor
    /// pairing the topic tripwire checks against [`Topic::ALL`]. Two topics: the
    /// reducer-minted [`Topic::LandingReceipt`] and the host-minted
    /// [`Topic::ViewDocument`] (host-produced and host-drained — this reactor is
    /// both sides), both members of the closed set.
    pub const DRAINED_TOPICS: &'static [Topic] = &[Topic::LandingReceipt, Topic::ViewDocument, Topic::SourceReplica];
}

mod runtime;
