//! Chassis-owned construction parameters for the `source` capability.
//!
//! The chassis resolves adapter configuration and constructs the shared
//! [`SourceShell`] before actor mounting. The actor receives that shell plus the
//! claim-registry enable decision as params; no adapter config crosses its
//! `NativeActor::Config` boundary.

use crate::bloomery::SourceShell;

pub struct SourceSetup {
    pub shell: SourceShell,
    pub claims_enabled: bool,
}
