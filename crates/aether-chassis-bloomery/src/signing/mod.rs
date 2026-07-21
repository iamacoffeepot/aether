//! The `signing` native capability — statement-signature custody (ADR-0149
//! step 3, ADR-0150, ADR-0151).
//!
//! The single host-local custody point for statement verification. ADR-0149
//! bars the wasm guest — and the pure control core — from holding key material:
//! the [`KeyProvider`](aether_bloomery::KeyProvider) trait is the pure
//! verification contract, but the *keys* live here, on the per-developer
//! instance, and never leave the machine (ADR-0150). This capability loads the
//! authorized-signer allowlist host-local via ADR-0090 derive-`Config`,
//! constructs the real [`Ed25519KeyProvider`](aether_bloomery::Ed25519KeyProvider),
//! and serves one `aether.signing.verify` request kind — the live answer gate
//! (`api/runtime.rs`) dials it rather than verifying inline against the fake
//! stub. Who may sign in a person's stead is the allowlist — capability key
//! policy (ADR-0151), never reducer logic.
//!
//! Identity/runtime split (ADR-0122): the [`SigningCapability`] ZST + the
//! `aether.signing.*` kind family are always-on; the `Ed25519KeyProvider`-backed
//! runtime lives in `runtime.rs` behind the `runtime` feature.

pub mod kinds;
pub use kinds::*;

#[cfg(feature = "runtime")]
mod config;
#[cfg(feature = "runtime")]
pub use config::{SigningConfig, SigningOverlay};

use aether_actor::actor;

/// Addressing identity for the `aether.signing` capability.
#[actor(singleton)]
pub struct SigningCapability;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::SigningCapabilityState;

#[cfg(all(test, feature = "runtime"))]
mod tests;
