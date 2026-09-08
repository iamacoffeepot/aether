//! Chassis-owned construction parameters for the `control` capability.
//!
//! The coordinator's backend-neutral scalars are resolved before actor mounting
//! and handed in as params, the way each outbox reactor receives its own cadence
//! — no config crosses the actor's `NativeActor::Config` boundary.

use aether_bloomery::StoreClass;

/// Construction parameters for [`ControlCore`](super::ControlCore).
pub struct ControlSetup {
    /// How often the control core observes the repository's mainline head, in
    /// seconds — the same backend-neutral coordinator cadence the outbox
    /// reactors poll on (`AETHER_GITHUB_POLL_INTERVAL_SECS`).
    pub poll_interval_secs: u64,
    /// Shared artifacts-store root the calibration and spend reads resolve
    /// study records from. `None` uses the same default
    /// [`resolve_root`](crate::artifacts::resolve_root) the artifacts
    /// capability does, so a coordinator that never set a root still
    /// reads the store it writes.
    pub artifacts_root: Option<String>,
    /// Which world the journal this core folds records (ADR-0184). Carried
    /// onto every rendered capability ledger, so a benchmark run's cells are
    /// never read as measurements of the estate's own operation. The same fact
    /// the store capability proved against the journal's own stamp when it
    /// opened it, resolved once at boot from the GitHub backend.
    pub store_class: StoreClass,
}
