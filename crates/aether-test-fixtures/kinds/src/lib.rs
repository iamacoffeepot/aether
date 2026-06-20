//! Shared `Kind` definitions for the workspace's wasm test fixtures.
//! The fixture component crates (the `bundle` cdylib and the
//! `stateful-{typed,reshaped}` satellites) pull these schemas from this
//! rlib, and the integration tests import the same types — visible as
//! `aether_test_fixtures_kinds::{TickObserved, …}` — for decode +
//! assertions without re-declaring them.
//!
//! The typed↔reshaped `CounterState` variants are deliberately absent:
//! each satellite crate carries its own so the schemas (and therefore
//! `Kind::ID`s) differ, which is what the ADR-0113 decode-miss test
//! turns on.

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
/// at instantiate-time as `<ProbeWithConfig as WasmActor>::Config`;
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
/// ADR-0101 hooks (now `WasmActor` defaults, no opt-in).
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

/// ADR-0114 inline-child fixture driver. A unit query sent to either the
/// parent's own address or its inline child's first-class lineage
/// address; the recipient replies an [`InlineEcho`] tagged with `who`
/// handled it, so the `FleetBench` scenario proves the membrane demuxed
/// the mail to the child (not the parent) and a control to the parent's
/// own address is unaffected. Postcard-shaped unit struct.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Default,
)]
#[kind(name = "aether.test_fixtures.inline_probe")]
pub struct InlineProbe;

/// Reply to [`InlineProbe`] — `who` names the actor that handled the
/// query so the test can assert the demux landed on the child vs the
/// parent. Postcard-shaped.
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
#[kind(name = "aether.test_fixtures.inline_echo")]
pub struct InlineEcho {
    pub who: u32,
}

/// [`InlineEcho::who`] marker for the parent component (the membrane's
/// own-id path).
pub const INLINE_WHO_PARENT: u32 = 1;

/// [`InlineEcho::who`] marker for the inline child (the membrane's
/// child-alias path).
pub const INLINE_WHO_CHILD: u32 = 2;

/// ADR-0114 inline-child teardown trigger. Sent to the despawn fixture's
/// parent (which tears down its stored child via `ctx.despawn_inline_child`)
/// or to the child's alias (which despawns itself mid-dispatch). Carries no
/// payload — the recipient address selects which actor tears the child
/// down. Postcard-shaped unit struct.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Default,
)]
#[kind(name = "aether.test_fixtures.despawn_child")]
pub struct DespawnChild;

/// Issue 1958: trigger sent to a `source_observer` fixture to request that
/// it forward a `SourceQuery` to the named target mailbox. The fixture then
/// sends `SourceQuery` to `MailboxId(to)`, making itself the component
/// origin so the reader's `ctx.source_mailbox()` sees the sender's mailbox.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Copy,
    Default,
)]
#[kind(name = "aether.test_fixtures.send_source_query")]
pub struct SendSourceQuery {
    /// Raw `MailboxId` of the component to forward `SourceQuery` to.
    pub to: u64,
}

/// Issue 1958: unit query sent to a `source_observer` fixture. Its
/// `Manual`-class handler reads `ctx.source_mailbox()` and broadcasts a
/// `SourceReport` to the test-bench observer mailbox.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Default,
)]
#[kind(name = "aether.test_fixtures.source_query")]
pub struct SourceQuery;

/// Issue 1958: broadcast emitted by the `source_observer` fixture after
/// reading `ctx.source_mailbox()`. `mailbox_id` is the raw `MailboxId`
/// of the sender (`0` when the source was a Session / `EngineMailbox` /
/// `None`, i.e. when `source_mailbox()` returned `None`).
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
#[kind(name = "aether.test_fixtures.source_report")]
pub struct SourceReport {
    pub mailbox_id: u64,
}

/// Issue 1977 (ADR-0114 amendment) cluster-addressing matrix driver. Sent to
/// the `matrix_sweep` fixture's parent over the wire to kick off the sweep:
/// the parent drives every in-cluster addressing direction (parent → child,
/// child → parent, child → sibling, child → self) and one cross-cluster send,
/// and each participant records the cell it observed (did the mail arrive,
/// what `ctx.source_mailbox()` did it read). `observer_mailbox` is the raw
/// `MailboxId` of a separate loaded component the cluster sends cross-cluster
/// to *during the in-place drain* — the recipient's observed source reflects
/// the documented Task 2 boundary (the cluster's inbound identity, not the
/// in-place child's id). `0` skips the cross-cluster cell.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Copy,
    Default,
)]
#[kind(name = "aether.test_fixtures.run_matrix")]
pub struct RunMatrix {
    /// Raw `MailboxId` of the cross-cluster observer component, or `0` to
    /// skip the cross-cluster cell.
    pub observer_mailbox: u64,
}

/// Issue 1977 in-cluster ping for the `matrix_sweep` fixture. `cell` selects
/// which matrix cell the recipient records (one of the `MATRIX_CELL_*`
/// markers); `fan_out` (set only on the parent → child a ping) instructs the
/// receiving child to drive the child-origin cells (child → parent, child →
/// sibling, child → self) and the cross-cluster send. Postcard-shaped.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Copy,
    Default,
)]
#[kind(name = "aether.test_fixtures.matrix_ping")]
pub struct MatrixPing {
    /// Which matrix cell the recipient records (a `MATRIX_CELL_*` marker).
    pub cell: u32,
    /// Set on the parent → child a ping: the receiving child fans out the
    /// child-origin cells and the cross-cluster send. `0` on every other ping.
    pub fan_out: u32,
    /// Raw `MailboxId` of the cross-cluster observer, threaded to the
    /// fanning-out child so it can address the cross-cluster send. `0` when
    /// the cross-cluster cell is skipped.
    pub observer_mailbox: u64,
}

/// Issue 1977 report query for the `matrix_sweep` fixture. Sent to the
/// parent over the wire *after* `RunMatrix` settles; the parent reads the
/// cluster's shared observation log and replies a [`MatrixReport`].
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
#[kind(name = "aether.test_fixtures.collect_matrix")]
pub struct CollectMatrix;

/// Issue 1977 structured matrix report — the `matrix_sweep` fixture's reply
/// to [`CollectMatrix`]. Each `*_arrived` flag is `1` when that cell's mail
/// was delivered (the recipient's handler ran), and each `*_source` is the
/// raw `MailboxId` the recipient read from `ctx.source_mailbox()` for that
/// cell (`0` for none). The cross-cluster cell is observed out-of-band by the
/// separate observer component (read via `log_tail`), so it carries no field
/// here. Postcard-shaped.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    Default,
)]
#[kind(name = "aether.test_fixtures.matrix_report")]
pub struct MatrixReport {
    /// parent → child a (in place): did child a receive the ping.
    pub parent_to_child_arrived: u32,
    /// parent → child a: the source child a read (expected: the parent id).
    pub parent_to_child_source: u64,
    /// child a → parent (in place): did the parent receive the ping.
    pub child_to_parent_arrived: u32,
    /// child a → parent: the source the parent read (expected: child a id).
    pub child_to_parent_source: u64,
    /// child a → sibling child b (in place): did child b receive the ping.
    pub child_to_sibling_arrived: u32,
    /// child a → sibling: the source child b read (expected: child a id).
    pub child_to_sibling_source: u64,
    /// child a → self (in place): did child a receive its own ping.
    pub child_to_self_arrived: u32,
    /// child a → self: the source child a read (expected: child a id).
    pub child_to_self_source: u64,
    /// child a's own folded `MailboxId` raw value, so the test can assert
    /// the child-origin sources equal the actual child id.
    pub child_a_id: u64,
    /// The parent's own folded `MailboxId` raw value, so the test can assert
    /// the parent-origin source (and the cross-cluster observed source).
    pub parent_id: u64,
}

/// [`MatrixPing::cell`] marker — parent to child a (in place).
pub const MATRIX_CELL_PARENT_TO_CHILD: u32 = 1;
/// [`MatrixPing::cell`] marker — child a to parent (in place).
pub const MATRIX_CELL_CHILD_TO_PARENT: u32 = 2;
/// [`MatrixPing::cell`] marker — child a to sibling child b (in place).
pub const MATRIX_CELL_CHILD_TO_SIBLING: u32 = 3;
/// [`MatrixPing::cell`] marker — child a to self (in place).
pub const MATRIX_CELL_CHILD_TO_SELF: u32 = 4;

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
