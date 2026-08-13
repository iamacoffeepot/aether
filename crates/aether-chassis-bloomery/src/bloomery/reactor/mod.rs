//! The Bloomery outbox reactors: the host-side actors that react to the
//! reducer's outbox topics and carry each recorded decision out as an effect.
//!
//! The reducer ([`mod@aether_bloomery::reduce`]) is a pure decision core — it never
//! touches the outside world; it records [`Decision`](aether_bloomery::Decision)s
//! and enqueues opaque payloads under a [`Topic`](aether_bloomery::Topic). A
//! reactor is the other half of that producer/reactor edge: a poll-driven
//! [`NativeActor`](aether_substrate::actor::native::NativeActor) that wakes on its
//! own timer tick, drains the topic it owns, performs the effect, and admits the
//! outcome back through the `aether.bloomery.control` core. Each topic is drained
//! by exactly one reactor (the pairing is a compiled tripwire — see the crate's
//! `topic_pairing` test).
//!
//! "Reactor" names what these do without colliding with the substrate
//! [`DriverCapability`](aether_substrate::chassis::builder::DriverCapability) — the
//! chassis run-loop that [`BloomeryDriverCapability`](crate::bloomery::BloomeryDriverCapability)
//! implements — or with the closed [`StageId`](aether_bloomery::StageId) line
//! vocabulary a member walks. Each reactor follows the identity/runtime split
//! (ADR-0122): the `#[actor(singleton)]` ZST identity lives in the submodule's
//! `mod.rs`, the state-bearing `#[runtime] impl NativeActor` in its `runtime.rs`.
//!
//! - [`executor`] — drains the dispatch / redispatch / aggregate-review topics,
//!   submits attempts through the [`ExecutorShell`](crate::bloomery::ExecutorShell),
//!   and admits matched results. The redispatch drain replays the attempt an
//!   answered parked question held (ADR-0151, #3664).
//! - [`integrate`] — folds each resolved member's claimed candidate tree onto the
//!   bloom's integration branch (ADR-0152).
//! - [`land`] — issues the source-port compare-and-swap that lands a resolved
//!   bloom on the mainline (ADR-0149 §The boundary).
//! - [`mirror`] — routes landing receipts and view documents out to the GitHub
//!   projection (ADR-0149 migration step 1).
//! - [`claim_release`] — runs the expected-holder compare-and-swap that retires
//!   an authorized orphan claim ref (ADR-0179). Mounted only on the GitHub
//!   branch: there is no ref namespace to release without one.
//! - [`janitor`] — reclaims leftover worktrees, consumed evidence, idle
//!   over-budget lane targets, and a terminal bloom's ephemeral repository
//!   refs. Journal-driven, not topic-driven: a kill or crash is a reclaimable
//!   fact the happy-path release misses.

mod claim_release;
mod executor;
mod integrate;
mod janitor;
mod land;
mod mirror;

pub use claim_release::{
    ClaimReleaseReactorCapability, ClaimReleaseReactorSetup, ClaimReleaseReactorState, ClaimReleaseTick,
};
pub use janitor::{JanitorReactorCapability, JanitorReactorSetup, JanitorReactorState, JanitorTick};
// `pub` because `reactor` is a private module — see the note in `executor`.
pub use executor::default_candidate_push;
pub use executor::{
    CandidatePush, DispatchTick, ExecutorReactorCapability, ExecutorReactorSetup, ExecutorReactorState,
};
pub use integrate::{IntegrateReactorCapability, IntegrateReactorSetup, IntegrateReactorState, IntegrateTick};
pub use land::{LandReactorCapability, LandReactorSetup, LandReactorState, LandTick};
pub use mirror::{DrainTick, MirrorReactorCapability, MirrorReactorSetup, MirrorReactorState};
