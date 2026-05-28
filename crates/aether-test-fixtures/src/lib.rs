//! Shared `Kind` definitions for the workspace's wasm test fixtures
//! (ADR-0090 c1). Each `examples/<name>.rs` actor is its own cdylib
//! that pulls the shared kinds from this rlib — that way integration
//! tests can import `aether_test_fixtures::{TickObserved, …}` for
//! decode + assertions without re-declaring the schemas.
//!
//! Pre-#1256 this crate was `aether-test-fixture-probe` and the actor
//! lived alongside the kinds in `src/lib.rs`. The rename and example
//! split exists so a sibling fixture (`probe_with_config`, exercising
//! ADR-0090's typed `FfiActor::Config`) can live next to the original
//! probe without taking on its own member crate.

#![no_std]

extern crate alloc;

use alloc::string::String;
use bytemuck::{Pod, Zeroable};

/// Mirror of `aether_substrate_bundle::test_bench::TEST_BENCH_OBSERVER_MAILBOX_NAME`.
/// Inlined here so wasm guests don't pull the bundle (`std`-bound)
/// into the FFI build.
pub const TEST_BENCH_OBSERVER_MAILBOX_NAME: &str = "aether.test_bench.observer";

/// Broadcast payload emitted on each tick. Postcard-shaped — schema
/// rides in the wasm's `aether.kinds` custom section, so the bench's
/// loopback decoder can record the kind name without the test
/// pre-registering anything.
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "aether.test_fixture.tick_observed")]
pub struct TickObserved {
    pub count: u64,
}

/// Driver kind: scenarios send this to flip a probe fixture's render
/// state. `visible == 0` halts the per-tick draw; any other value
/// enables it. Cast-shape so encoding is just a memcpy of four
/// bytes — keeps the test-side `MailEnvelope.payload` construction
/// trivial.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.test_fixture.set_render")]
pub struct SetRender {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub visible: u8,
}

/// ADR-0090 c1 typed-config fixture payload. Threaded into the guest
/// at instantiate-time as `<ProbeWithConfig as FfiActor>::Config`;
/// the actor stamps `seed` and `label` into its state and exposes
/// them on demand via `ConfigEcho`.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
)]
#[kind(name = "aether.test_fixtures.probe_config")]
pub struct ProbeConfig {
    pub seed: u32,
    pub label: String,
}

/// Reply kind for `ConfigQuery`: surfaces the `(seed, label)` the
/// fixture cached from its `Config` at init-time. Lets a test
/// assert the typed-config path round-tripped end-to-end.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
)]
#[kind(name = "aether.test_fixtures.config_echo")]
pub struct ConfigEcho {
    pub seed: u32,
    pub label: String,
}

/// Driver kind for the typed-config fixture: request a `ConfigEcho`
/// describing the cached config. Postcard-shaped (unit struct) so the
/// fixture exercises the full schema-driven dispatch path even on the
/// no-payload query side.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Default,
)]
#[kind(name = "aether.test_fixtures.config_query")]
pub struct ConfigQuery;
