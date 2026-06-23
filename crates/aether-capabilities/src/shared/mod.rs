//! Shared infrastructure consumed by multiple capabilities but not itself a capability.
#[cfg(not(target_arch = "wasm32"))]
pub mod contentgen;
