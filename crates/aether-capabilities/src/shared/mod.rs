//! Shared infrastructure consumed by multiple capabilities but not itself a capability.
#[cfg(not(target_family = "wasm"))]
pub mod contentgen;
