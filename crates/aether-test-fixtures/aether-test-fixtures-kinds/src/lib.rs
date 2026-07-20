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

use alloc::string::String;
use alloc::vec::Vec;
use bytemuck::{Pod, Zeroable};

/// Mirror of `aether_harness_substrate::SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME`.
/// Inlined here so wasm guests don't pull the bundle (`std`-bound)
/// into the FFI build.
pub const SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME: &str = "aether.substrate_harness.observer";

/// Broadcast payload emitted on each tick. Structured-shaped — schema
/// rides in the wasm's `aether.kinds` custom section, so the harness's
/// loopback decoder can record the kind name without the test
/// pre-registering anything.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[kind(name = "aether.test_fixture.tick_observed")]
pub struct TickObserved {
    pub count: u64,
}

/// ADR-0147 boot fixture: broadcast the module's `boot` actor emits from
/// its `wire` hook, once per boot instance. A `SubstrateHarness` scenario counts
/// it via `count_observed` to prove the module-boot singleton is
/// instantiated exactly once no matter how many selector loads of the
/// module happened (cardinality). Structured-shaped like [`TickObserved`]
/// so the harness's loopback decoder records the kind name without the test
/// pre-registering anything.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[kind(name = "aether.test_fixture.boot_observed")]
pub struct BootObserved {
    pub marker: u64,
}

/// ADR-0147 boot fixture: broadcast the module's `boot` actor emits from
/// its `unwire` teardown hook, once when the host tears the boot singleton
/// down (its refcount reached zero as the last non-boot actor from the
/// module unloaded). The scenario asserts it stays at zero across a partial
/// unload (boot survives) and reaches one after the last unload (teardown).
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[kind(name = "aether.test_fixture.boot_torn_down")]
pub struct BootTornDown {
    pub marker: u64,
}

/// Broadcast payload the probe emits on each `Key` input dispatch,
/// carrying the pressed key `code`. Lets the ADR-0021 input round-trip
/// scenarios count `aether.input` fan-out deliveries the same way
/// [`TickObserved`] counts lifecycle ticks — `Key` is a genuine input
/// interrupt, so it exercises the `aether.input` subscribe / unsubscribe
/// / drop-clears path that `Tick` no longer does (issue 1490).
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[kind(name = "aether.test_fixture.key_observed")]
pub struct KeyObserved {
    pub code: u32,
}

/// Broadcast payload the probe emits on each `TextInput` dispatch,
/// echoing the committed `text`. Lets the ADR-0021 input round-trip
/// scenario assert the `aether.input` cap fanned a `TextInput` out to a
/// subscriber — the guard for the new text-stream fan-out handler being
/// wired up, mirroring how [`KeyObserved`] guards the `Key` fan-out.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[kind(name = "aether.test_fixture.text_input_observed")]
pub struct TextInputObserved {
    pub text: String,
}

/// Driver kind: scenarios send this to flip a probe fixture's render
/// state. `visible == 0` halts the per-tick draw; any other value
/// enables it. Cast-shape so encoding is just a memcpy of four
/// bytes — keeps the test-side `NamedMail.payload` construction
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
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq,
)]
#[kind(name = "aether.test_fixtures.probe_config")]
pub struct ProbeConfig {
    pub seed: u32,
    pub label: String,
}

/// Reply kind for `ConfigQuery`: surfaces the `(seed, label)` the
/// fixture cached from its `Config` at init-time. Lets a test
/// assert the typed-config path round-tripped end-to-end.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.test_fixtures.config_echo")]
pub struct ConfigEcho {
    pub seed: u32,
    pub label: String,
}

/// Driver kind for the typed-config fixture: request a `ConfigEcho`
/// describing the cached config. Structured-shaped (unit struct) so the
/// fixture exercises the full schema-driven dispatch path even on the
/// no-payload query side.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.config_query")]
pub struct ConfigQuery;

/// Trigger for the `mat4_source` fixture (issue 1472). A DAG `Source`
/// dispatches this no-payload trigger to the loaded `mat4_source`
/// component, whose reply (`Mat4Apply`) feeds the `mat4_apply` transform
/// downstream. Structured-shaped unit struct — the trigger carries no
/// fields, so its `encode_into_bytes` is the descriptor `Source.payload`.
/// `Default` lets the descriptor build that payload from one instance.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.mat4_source_trigger")]
pub struct Mat4SourceTrigger;

/// Driver kind for the stateful multi-actor replace fixture (ADR-0101):
/// each `Bump` increments the fixture's in-memory counter by one.
/// Structured-shaped unit struct.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.bump")]
pub struct Bump;

/// Query kind for the stateful replace fixture: request the live counter.
/// The fixture replies with a `CountReport`. Structured-shaped unit struct.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.count_query")]
pub struct CountQuery;

/// Reply to `CountQuery`, and the wire shape of the state bundle the
/// fixture saves in `on_dehydrate` / restores in `on_rehydrate`. A test
/// asserts this value survives a `replace_component` swap via the
/// ADR-0101 hooks (now `WasmActor` defaults, no opt-in).
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
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
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq,
)]
#[kind(name = "aether.test_fixtures.ui_widget_config")]
pub struct UiWidgetConfig {
    pub redraw_each_tick: bool,
    pub quad_count: u32,
}

/// ADR-0114 inline-child fixture driver. A unit query sent to either the
/// parent's own address or its inline child's first-class lineage
/// address; the recipient replies an [`InlineEcho`] tagged with `who`
/// handled it, so the `FleetHarness` scenario proves the membrane demuxed
/// the mail to the child (not the parent) and a control to the parent's
/// own address is unaffected. Structured-shaped unit struct.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.inline_probe")]
pub struct InlineProbe;

/// Reply to [`InlineProbe`] — `who` names the actor that handled the
/// query so the test can assert the demux landed on the child vs the
/// parent. Structured-shaped.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
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
/// down. Structured-shaped unit struct.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.despawn_child")]
pub struct DespawnChild;

/// Issue 2690 typed config for the config-carrying inline-child reload
/// fixture: the durable counter's starting value. Distinct from the
/// `()`-config `InlineStatefulChild` — this is the config-bytes case the
/// composite reload bundle dropped before the fix (`reconstruct_one_child`
/// re-inited every child from empty config bytes, so a typed (non-`()`)
/// `Config` decoded `None` and the child was skipped, not just reset).
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq,
)]
#[kind(name = "aether.test_fixtures.inline_configured_child_config")]
pub struct InlineConfiguredChildConfig {
    pub initial: u32,
}

/// The non-default `initial` value the `inline_child` bundle's
/// `InlineConfiguredParent` spawns its child with — distinct from
/// `InlineConfiguredChildConfig::default()`'s `0`, so a reload that
/// silently re-inited from a default/empty config is distinguishable
/// from one that decoded the real config bytes. Shared here (not just
/// hardcoded in the fixture) so the `FleetHarness` reload scenario asserts
/// against the same constant rather than a magic number.
pub const CONFIGURED_CHILD_INITIAL: u32 = 100;

/// Issue 2692 by-tag inline-spawn fixture driver. Sent to the tag-parent's
/// own address; the parent replies a [`TagSpawnReport`] carrying the outcome
/// of the deliberately-unknown-tag spawn it attempted in `wire`, so a
/// scenario can assert the generated resolver returns `UnknownActorTag`
/// rather than spawning or panicking. Structured-shaped unit struct.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.tag_spawn_query")]
pub struct TagSpawnQuery;

/// Reply to [`TagSpawnQuery`]: `unknown_tag_rejected` is `true` when the
/// tag-parent's `wire`-time `spawn_inline_child_by_tag` with a bogus
/// `ActorTypeTag` returned `SpawnError::UnknownActorTag` (the only correct
/// outcome). Structured-shaped.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.test_fixtures.tag_spawn_report")]
pub struct TagSpawnReport {
    pub unknown_tag_rejected: bool,
}

/// Issue 1958: trigger sent to a `source_observer` fixture to request that
/// it forward a `SourceQuery` to the named target mailbox. The fixture then
/// sends `SourceQuery` to `MailboxId(to)`, making itself the component
/// origin so the reader's `ctx.source_mailbox()` sees the sender's mailbox.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Copy, Default)]
#[kind(name = "aether.test_fixtures.send_source_query")]
pub struct SendSourceQuery {
    /// Raw `MailboxId` of the component to forward `SourceQuery` to.
    pub to: u64,
}

/// Issue 1958: unit query sent to a `source_observer` fixture. Its
/// `Manual`-class handler reads `ctx.source_mailbox()` and broadcasts a
/// `SourceReport` to the substrate-harness observer mailbox.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.source_query")]
pub struct SourceQuery;

/// Issue 1958: broadcast emitted by the `source_observer` fixture after
/// reading `ctx.source_mailbox()`. `mailbox_id` is the raw `MailboxId`
/// of the sender (`0` when the source was a Session / `EngineMailbox` /
/// `None`, i.e. when `source_mailbox()` returned `None`).
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.test_fixtures.source_report")]
pub struct SourceReport {
    pub mailbox_id: u64,
}

/// Issue 2791: trigger for the request-correlation fixture. The fixture
/// sends two `aether.fs.read` requests for this same namespace/path and
/// demuxes the indistinguishable replies by `ctx.in_reply_to()`.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.run_fs_demux")]
pub struct RunFsDemux {
    pub namespace: String,
    pub path: String,
}

/// Issue 2791: report emitted once both same-path fs replies were matched by
/// request id rather than by echoed payload fields.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.test_fixtures.fs_demux_report")]
pub struct FsDemuxReport {
    pub first_matched: bool,
    pub second_matched: bool,
}

/// Configure the listener lineage used by the TCP load probe when it echoes
/// frames received from accepted sessions.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[kind(name = "aether.test_fixtures.configure_tcp_load_probe")]
pub struct ConfigureTcpLoadProbe {
    pub listener_name: String,
}

/// Ask the TCP load probe to start a bounded batch of outbound connections.
/// Every session gets a deterministic, unique name below `aether.tcp`.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[kind(name = "aether.test_fixtures.start_tcp_connect_load")]
pub struct StartTcpConnectLoad {
    pub addr: String,
    pub connection_count: u32,
    pub session_name_prefix: String,
}

/// Query the TCP load probe's exact consumer-side accounting.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.collect_tcp_load_snapshot")]
pub struct CollectTcpLoadSnapshot;

/// The two distinct TCP session lineages exercised by the load scenario.
#[derive(aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpLoadTopology {
    /// A cap -> listener -> session accepted by a bound listener.
    Accepted,
    /// A cap -> session created by an outbound connect.
    Outbound,
}

/// Exact consumer-side state for one accepted or outbound TCP session.
#[derive(aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TcpLoadSessionSnapshot {
    pub topology: TcpLoadTopology,
    pub session_name: String,
    pub established: bool,
    pub received_frame_count: u64,
    pub received_payload_bytes: u64,
    pub closed: bool,
}

/// Reply to [`CollectTcpLoadSnapshot`]. Connect failures are explicit data so
/// the host never has to infer them from missing sessions or scrape logs.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.test_fixtures.tcp_load_snapshot")]
pub struct TcpLoadSnapshot {
    pub sessions: Vec<TcpLoadSessionSnapshot>,
    pub connect_failures: Vec<String>,
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
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Copy, Default)]
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
/// sibling, child → self) and the cross-cluster send. Structured-shaped.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Copy, Default)]
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
/// Structured-shaped unit struct.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.collect_matrix")]
pub struct CollectMatrix;

/// Issue 1977 structured matrix report — the `matrix_sweep` fixture's reply
/// to [`CollectMatrix`]. Each `*_arrived` flag is `1` when that cell's mail
/// was delivered (the recipient's handler ran), and each `*_source` is the
/// raw `MailboxId` the recipient read from `ctx.source_mailbox()` for that
/// cell (`0` for none). The cross-cluster cell is observed out-of-band by the
/// separate observer component (read via `log_tail`), so it carries no field
/// here. Structured-shaped.
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq, Default,
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

/// Typed config for an editor-region probe. The probe intentionally does not
/// subscribe to input itself: an editor shell must address each observation
/// directly to the probe's mailbox.
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq,
)]
#[kind(name = "aether.test_fixtures.editor_region_probe.config")]
pub struct EditorRegionProbeConfig {
    pub name: String,
}

/// One raw input observed by an editor-region probe.
#[derive(aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub enum ObservedEditorInput {
    PointerPress { button: u32, x_pixels: f32, y_pixels: f32 },
    PointerRelease { button: u32, x_pixels: f32, y_pixels: f32 },
    PointerMotion { x_pixels: f32, y_pixels: f32 },
    Wheel { delta_x_pixels: f32, delta_y_pixels: f32, x_pixels: f32, y_pixels: f32 },
    KeyPress { code: u32 },
    KeyRelease { code: u32 },
    TextInput { text: String },
    ImePreedit { text: String, cursor_begin: Option<u32>, cursor_end: Option<u32> },
    Modifiers { shift: bool, ctrl: bool, alt: bool, meta: bool },
}

/// Query that drains an editor-region probe's observations.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.test_fixtures.drain_editor_inputs")]
pub struct DrainEditorInputs;

/// Reply containing every editor input observed since the previous drain.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.test_fixtures.drain_editor_inputs_result")]
pub struct DrainEditorInputsResult {
    pub region_name: String,
    pub inputs: Vec<ObservedEditorInput>,
}
