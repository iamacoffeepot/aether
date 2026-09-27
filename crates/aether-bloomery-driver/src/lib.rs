//! The native bundle driver (ADR-0226): journal folds in, driver commands
//! out, performed as mail by the `aether.bloomery.driver` actor.
//!
//! The identity half of the ADR-0122 split is always on: the
//! [`BundleDriver`] marker. The runtime half, behind the `runtime` feature, is
//! the sans-io program core, the construction params, and the actor's state;
//! its module documentation describes the core.
//!
//! `no_std` without the `runtime` feature, so a wasm guest can name the driver
//! and send it kind-checked mail.

#![cfg_attr(not(feature = "runtime"), no_std)]
#![forbid(unsafe_code)]

#[cfg(feature = "runtime")]
pub use runtime::{
    AppendTicket, ArtifactTicket, CallerId, ClosureTicket, Command, DriverParams, EVENTS_PAGE, EvaluateTicket,
    EventsTicket, InvokeTicket, LoadOutcome, LoadTicket, ProgramCore, StatusTicket, WarmTicket, WatchTicket,
};

/// `aether.bloomery.driver` actor **identity** (ADR-0122 split): the native
/// bundle driver over the sans-io program core. A ZST carrying only the
/// addressing, the per-handler `HandlesKind` markers, the contract rows, and
/// the name-inventory row `#[actor]` emits always-on; the state-bearing
/// runtime lives behind `feature = "runtime"`.
///
/// Answers `aether.bloomery.driver.call` with exactly one `CallOutcome` per
/// call, once the outcome is recorded, and `aether.bloomery.driver.await_processed`
/// with `Processed` once its bound is quiescent. It performs the core's commands
/// as mail to the journal owner (including the watch), the component host,
/// and bundle roots. One driver per unit; the type does not enforce it.
#[actor(instanced, root, depends(ComponentHostCapability))]
pub struct BundleDriver;

use aether_actor::actor;
use aether_component::ComponentHostCapability;

#[cfg(feature = "runtime")]
mod runtime;
