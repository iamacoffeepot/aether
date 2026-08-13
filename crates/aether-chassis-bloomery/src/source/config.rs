//! Chassis-owned construction parameters for the `source` capability.
//!
//! The chassis resolves adapter configuration and constructs the shared
//! [`SourceShell`] before actor mounting. The actor receives that shell plus the
//! claim-registry enable decision as params; no adapter config crosses its
//! `NativeActor::Config` boundary.

use aether_bloomery_github::MainlineRef;

use crate::bloomery::SourceShell;

pub struct SourceSetup {
    pub shell: SourceShell,
    pub claims_enabled: bool,
    /// The ref the shell was pointed at (ADR-0186), so the capability's boot log
    /// names the branch this coordinator is operating on. Read for the log only —
    /// the shell holds the ref that is actually addressed.
    pub mainline: MainlineRef,
}
