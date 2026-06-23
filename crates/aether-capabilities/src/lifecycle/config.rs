use aether_data::{KindId, MailboxId as DataMailboxId};

use super::LifecycleGraphData;

/// Construction-time configuration for `LifecycleCapability`.
/// Carries the compiled data graph + the initial subscriber wiring.
/// Built per-chassis at builder time and consumed by `init`.
pub struct LifecycleConfig {
    /// The compiled lifecycle graph. Built via
    /// [`LifecycleGraphData::builder`](super::LifecycleGraphData::builder)
    /// on the chassis side.
    pub graph: LifecycleGraphData,
    /// Initial `(stage_kind, mailbox)` pairs to populate the
    /// subscriber table at boot — a chassis builder can pre-subscribe
    /// a mailbox to a stage this way without round-tripping a
    /// `LifecycleSubscribe` mail. Each pair must
    /// reference a stage kind declared by `graph` — the boot path
    /// verifies this and returns `BootError` otherwise, so
    /// misconfiguration fails fast at chassis-build.
    pub initial_subscribers: Vec<(KindId, DataMailboxId)>,
    /// Force-complete deadline for a pending advance's `Settled`
    /// (iamacoffeepot/aether#1048), in milliseconds. Resolved
    /// chassis-side (env override over [`Self::ADVANCE_TIMEOUT_MS_DEFAULT`])
    /// rather than read from the environment in `init`, so the cap
    /// configures through this struct rather than a naked env read.
    pub advance_timeout_millis: u64,
}

impl LifecycleConfig {
    /// Default force-complete deadline (ms) for a pending advance.
    /// Chassis builders that don't override use this.
    pub const ADVANCE_TIMEOUT_MS_DEFAULT: u64 = super::settlement::ADVANCE_TIMEOUT_MS_DEFAULT;
}
