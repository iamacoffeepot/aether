//! The `artifacts` native capability (ADR-0149 §The boundary).
//!
//! The **artifacts** port: "digest-addressed bytes; canonical record, never
//! evicted." An eviction-free consumer of the extracted content-address core
//! (`aether_substrate::content_store`) on mailbox `aether.artifacts`, exposing
//! `put` (digest-address bytes + their derivation-DAG parents) and `get` (by
//! digest). Being a second *consumer* of the one addressing core the hub also
//! uses — not a rival store — is the ADR-0116 reuse-not-rival outcome ADR-0149
//! §The boundary requires; the eviction-free policy sidesteps both hazards
//! ADR-0149 flags in the hub store (pin-not-persisted, unnamed-silent-evict)
//! by not evicting at all.
//!
//! Identity/runtime split (ADR-0122): the [`ArtifactsCapability`] ZST + the
//! `aether.artifacts.*` kind family are always-on; the `ContentStore`-backed
//! runtime lives in `runtime.rs` behind the `runtime` feature.

pub mod kinds;
pub use kinds::*;

#[cfg(feature = "runtime")]
mod config;
#[cfg(feature = "runtime")]
pub use config::{ArtifactsConfig, ArtifactsOverlay};

use aether_actor::actor;

/// Addressing identity for the `aether.artifacts` capability.
#[actor(singleton, root)]
pub struct ArtifactsCapability;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::{ArtifactEntry, ArtifactMeta, ArtifactsCapabilityState, resolve_root};

#[cfg(all(test, feature = "runtime"))]
mod tests;
