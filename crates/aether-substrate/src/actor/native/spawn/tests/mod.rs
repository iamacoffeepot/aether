//! Tests for the native spawn path, one sibling per concern.
//!
//! [`terminals`] pins the asymmetry between the two builders — the staged
//! one has no eager terminal, the boot/embedder one still does — and the
//! bootstrap-mail preparation beside it. [`identity`] covers lineage
//! resolution before construction; [`activation`] drives whole births
//! through the registry owner and its activation barrier. `guest` stages
//! published guests from a booted host and hands a held reply to one. [`support`] holds
//! the probe actor and the prepared-birth fixtures they share. The teardown
//! gate's own tripwire lives beside the walk it exercises, in
//! `spawner::teardown`.

#![allow(clippy::unwrap_used, reason = "activation lifecycle tests use bounded channels and fixture-only setup")]

mod activation;
#[cfg(feature = "wasm")]
mod guest;
mod identity;
mod support;
mod terminals;
