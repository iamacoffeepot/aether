//! The `source` capability's boot configuration (ADR-0090 derive-`Config`).
//!
//! The connection knobs — token, owner/name, API base, and the CAS-land
//! enable flag — ride the same [`GithubMirrorConfig`](crate::bloomery::GithubMirrorConfig)
//! the mirror shell uses (`bloomery/mirror.rs`): that config already carries
//! `cas_land_enabled` for exactly this port, so one GitHub-connection config
//! serves both caps rather than duplicating the knobs.

pub use crate::bloomery::GithubMirrorConfig as SourceConfig;
pub use crate::bloomery::GithubMirrorOverlay as SourceOverlay;
