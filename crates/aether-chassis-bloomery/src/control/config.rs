//! Chassis-owned construction parameters for the `control` capability.
//!
//! The coordinator's backend-neutral scalars are resolved before actor mounting
//! and handed in as params, the way each outbox reactor receives its own cadence
//! — no config crosses the actor's `NativeActor::Config` boundary.

/// Construction parameters for [`ControlCore`](super::ControlCore).
pub struct ControlSetup {
    /// How often the control core observes the repository's mainline head, in
    /// seconds — the same backend-neutral coordinator cadence the outbox
    /// reactors poll on (`AETHER_GITHUB_POLL_INTERVAL_SECS`).
    pub poll_interval_secs: u64,
}
