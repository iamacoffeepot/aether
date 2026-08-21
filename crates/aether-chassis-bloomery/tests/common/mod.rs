//! Shared fixtures for the cross-process tests that fork the `bloomery`
//! coordinator bin: a free localhost port, and a guard that owns a forked
//! coordinator for the life of a binding.
//!
//! Scenario harnesses live in [`aether_harness_bloomery`]. This module re-exports
//! the pieces remaining binaries still need after the promotion (issue 5332).

#![allow(dead_code, reason = "each test binary compiles the whole module and uses only the fixtures it needs")]
#![allow(clippy::unwrap_used, reason = "a fixture that cannot set up its process reports it by panicking")]

pub use aether_harness_bloomery::client;
pub use aether_harness_bloomery::{Coordinator, free_port};

fn _every_binary_names_the_fixtures() {
    let _: fn() -> u16 = free_port;
    let _: fn(u16, &[(&str, &str)]) -> Coordinator = Coordinator::spawn;
    let _ = client::connect_and_handshake;
}
