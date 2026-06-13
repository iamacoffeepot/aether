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

use aether_data::Ref;
use aether_math::Vec4;
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

/// Broadcast payload the probe emits on each `Key` input dispatch,
/// carrying the pressed key `code`. Lets the ADR-0021 input round-trip
/// scenarios count `aether.input` fan-out deliveries the same way
/// [`TickObserved`] counts lifecycle ticks — `Key` is a genuine input
/// interrupt, so it exercises the `aether.input` subscribe / unsubscribe
/// / drop-clears path that `Tick` no longer does (issue 1490).
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "aether.test_fixture.key_observed")]
pub struct KeyObserved {
    pub code: u32,
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

/// Trigger for the `mat4_source` fixture (issue 1472). A DAG `Source`
/// dispatches this no-payload trigger to the loaded `mat4_source`
/// component, whose reply (`Mat4Apply`) feeds the `mat4_apply` transform
/// downstream. Postcard-shaped unit struct — the trigger carries no
/// fields, so its `encode_into_bytes` is the descriptor `Source.payload`.
/// `Default` lets the descriptor build that payload from one instance.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Default,
)]
#[kind(name = "aether.test_fixtures.mat4_source_trigger")]
pub struct Mat4SourceTrigger;

/// Observer request for the `vec4_observer` fixture (issue 1472). The
/// substrate's handle-resolution walk splices the transform's resolved
/// `Vec4` output into the `input` slot as `Ref::Inline` before dispatch,
/// so the observer reads the value directly. The `Ref<Vec4>` field's
/// inner kind id is `Vec4::ID`, which the Transform→Observer edge
/// type-check matches against the transform's `output_kind_id`.
///
/// Postcard-shaped: the `Ref<Vec4>` field serializes through the
/// hand-written `impl<K: Kind> Serialize/Deserialize for Ref<K>`
/// (ADR-0100), which needs only `Vec4: Kind` — no `Vec4` serde.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    PartialEq,
)]
#[kind(name = "aether.test_fixtures.vec4_observed")]
pub struct Vec4Observed {
    pub input: Ref<Vec4>,
}

/// Driver kind for the stateful multi-actor replace fixture (ADR-0101):
/// each `Bump` increments the fixture's in-memory counter by one.
/// Postcard-shaped unit struct.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Default,
)]
#[kind(name = "aether.test_fixtures.bump")]
pub struct Bump;

/// Query kind for the stateful replace fixture: request the live counter.
/// The fixture replies with a `CountReport`. Postcard-shaped unit struct.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Default,
)]
#[kind(name = "aether.test_fixtures.count_query")]
pub struct CountQuery;

/// Reply to `CountQuery`, and the wire shape of the state bundle the
/// fixture saves in `on_dehydrate` / restores in `on_rehydrate`. A test
/// asserts this value survives a `replace_component` swap via the
/// ADR-0101 hooks (now `FfiActor` defaults, no opt-in).
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
#[kind(name = "aether.test_fixtures.count_report")]
pub struct CountReport {
    pub count: u32,
}

/// Typed config for the `ui_widget` fixture (issue 1793 widget-actor
/// cost spike). `redraw_each_tick` selects the per-frame cost profile:
/// `true` re-emits the full `DrawSolidQuads` batch across the wasm
/// boundary every tick (the naive actor-backed widget), `false`
/// early-returns on tick (the stable-frame floor a host-cached-replay
/// widget pays before the host replays its retained batch — the guest is
/// still dispatched, it just emits nothing). `quad_count` is the draw
/// weight: how many `SolidQuad`s the batch carries when it does emit, so
/// the measurement can scale the per-frame re-emit cost with widget
/// visual complexity.
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
#[kind(name = "aether.test_fixtures.ui_widget_config")]
pub struct UiWidgetConfig {
    pub redraw_each_tick: bool,
    pub quad_count: u32,
}

#[cfg(test)]
mod tests {
    use aether_data::{Kind, Ref};
    use aether_math::Vec4;

    use super::Vec4Observed;

    /// The `Ref<Vec4>` slot survives a postcard round-trip through the
    /// ADR-0100 hand-written `Ref<K>` serde: an inline `Vec4` encodes
    /// then decodes unchanged. Guards the #1475-backed derive the
    /// `vec4_observer` fixture rests on (the observer reads its
    /// `Ref::Inline(Vec4)` slot the same way).
    #[test]
    fn vec4_observed_inline_round_trips() {
        let original = Vec4Observed {
            input: Ref::Inline(Vec4::new(7.0, 9.0, 11.0, 1.0)),
        };
        let bytes = original.encode_into_bytes();
        let decoded = Vec4Observed::decode_from_bytes(&bytes)
            .expect("Vec4Observed decodes from its own encode_into_bytes output");
        assert_eq!(decoded, original);
    }
}
