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
#![forbid(unsafe_code)]

extern crate alloc;

pub mod wire_corpus;

use aether_bloomery_reactor::kinds::{Head, OpaqueBytes, ProgramName, Ref, Utf8Text};
use alloc::string::String;
use alloc::vec::Vec;

/// Typed root marker for the substrate harness's observer mailbox, the
/// sink the fixtures report to through
/// `ctx.send::<SubstrateHarnessObserver>(&report)`. Its `NAMESPACE`
/// mirrors `aether_harness_substrate::SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME`;
/// it lives here so wasm guests don't pull the harness (`std`-bound) into the
/// FFI build.
///
/// The blanket `HandlesKind<K>` is honest: the harness registers the observer
/// as an inline closure that records every kind it receives. A fixture that
/// reports here declares `depends(SubstrateHarnessObserver)`, so it loads only
/// where the observer is registered: the substrate harness. Headless and
/// `FleetHarness` register none, so a load of such a fixture there is refused.
pub struct SubstrateHarnessObserver;

impl aether_actor::Addressable for SubstrateHarnessObserver {
    const NAMESPACE: &'static str = "aether.substrate_harness.observer";
    type Resolver = aether_actor::One;
}

impl<K: aether_data::Kind> aether_actor::HandlesKind<K> for SubstrateHarnessObserver {}

/// Broadcast payload emitted on each tick. Structured-shaped — schema
/// rides in the wasm's `aether.kinds` custom section, so the harness's
/// loopback decoder can record the kind name without the test
/// pre-registering anything.
#[aether_data::kind(name = "aether.test_fixture.tick_observed")]
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
#[aether_data::kind(name = "aether.test_fixture.boot_observed")]
pub struct BootObserved {
    pub marker: u64,
}

/// Report the stateful-replace `Counter` fixture mails the harness observer
/// from its `wire` hook, once per run of the hook. A scenario counts it to
/// prove a guest reinstated after a failed replace runs `wire` again
/// (ADR-0241 §7).
#[aether_data::kind(name = "aether.test_fixture.wire_observed", default)]
pub struct WireObserved;

/// ADR-0147 boot fixture: broadcast the module's `boot` actor emits from
/// its `unwire` hook, once when the boot singleton closes on a drop
/// addressed at it. The scenario asserts it stays at zero while every widget
/// unloads (the boot outlives them) and reaches one after the boot's own
/// drop.
#[aether_data::kind(name = "aether.test_fixture.boot_torn_down")]
pub struct BootTornDown {
    pub marker: u64,
}

/// Broadcast payload the probe emits on each `Key` input dispatch,
/// carrying the pressed key `code`. Lets the ADR-0021 input round-trip
/// scenarios count `aether.window` fan-out deliveries the same way
/// [`TickObserved`] counts lifecycle ticks — `Key` is a genuine input
/// interrupt, so it exercises the `aether.window` subscribe /
/// unsubscribe / drop-clears path that `Tick` does not.
#[aether_data::kind(name = "aether.test_fixture.key_observed")]
pub struct KeyObserved {
    pub code: u32,
}

/// Broadcast payload the probe emits on each `TextInput` dispatch,
/// echoing the committed `text`. Lets the ADR-0021 input round-trip
/// scenario assert the `aether.window` cap fanned a `TextInput` out to a
/// subscriber — the guard for the new text-stream fan-out handler being
/// wired up, mirroring how [`KeyObserved`] guards the `Key` fan-out.
#[aether_data::kind(name = "aether.test_fixture.text_input_observed")]
pub struct TextInputObserved {
    pub text: String,
}

/// Driver kind: scenarios send this to flip a probe fixture's render
/// state. `visible == 0` halts the per-tick draw; any other value
/// enables it. Cast-shape so encoding is just a memcpy of four
/// bytes — keeps the test-side `NamedMail.payload` construction
/// trivial.
#[repr(C)]
#[aether_data::kind(name = "aether.test_fixture.set_render", pod, default)]
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
#[aether_data::kind(name = "aether.test_fixtures.probe_config", default, eq)]
pub struct ProbeConfig {
    pub seed: u32,
    pub label: String,
}

/// Reply kind for `ConfigQuery`: surfaces the `(seed, label)` the
/// fixture cached from its `Config` at init-time. Lets a test
/// assert the typed-config path round-tripped end-to-end.
#[aether_data::kind(name = "aether.test_fixtures.config_echo", eq)]
pub struct ConfigEcho {
    pub seed: u32,
    pub label: String,
}

/// Driver kind for the typed-config fixture: request a `ConfigEcho`
/// describing the cached config. Structured-shaped (unit struct) so the
/// fixture exercises the full schema-driven dispatch path even on the
/// no-payload query side.
#[aether_data::kind(name = "aether.test_fixtures.config_query", default)]
pub struct ConfigQuery;

/// ADR-0163 §3 (#3984) driver kind: ask the `QuietProbe` fixture to report
/// what it pulled from its asset load window during `wire`. No-payload
/// query; the reply is an [`AssetProbeResult`]. Structured unit struct so
/// it exercises the schema-driven dispatch path like [`ConfigQuery`].
#[aether_data::kind(name = "aether.test_fixtures.asset_probe", default)]
pub struct AssetProbe;

/// Issue 7107: driver kind for the `QuietProbe` fixture's log-ring proof. The
/// `QuietProbe` fixture writes one `typed_send_alive` info line per delivery,
/// so the log-ring tests send this instead of waiting on a tick to fire it.
/// No-payload trigger, structured unit struct like [`AssetProbe`].
#[aether_data::kind(name = "aether.test_fixtures.log_marker", default)]
pub struct LogMarker;

/// Reply kind for [`AssetProbe`]: the length and a wrapping-sum checksum of
/// the bytes the fixture pulled through `AssetWindow::asset` in `wire`,
/// stashed in state and surfaced from a post-`wire` handler. Lets a test
/// assert the guest-side asset pull round-tripped the exact bytes across
/// the FFI (the checksum is content-sensitive) and that the value survived
/// the window closing (the read happens after `wire`). `pulled` is `false`
/// when the window returned no such asset — a loud negative rather than a
/// silent zero.
#[aether_data::kind(name = "aether.test_fixtures.asset_probe_result", default, eq)]
pub struct AssetProbeResult {
    pub pulled: bool,
    pub len: u64,
    pub checksum: u64,
}

/// Trigger for the `mat4_source` fixture (issue 1472). A DAG `Source`
/// dispatches this no-payload trigger to the loaded `mat4_source`
/// component, whose reply (`Mat4Apply`) feeds the `mat4_apply` transform
/// downstream. Structured-shaped unit struct — the trigger carries no
/// fields, so its `encode_into_bytes` is the descriptor `Source.payload`.
/// `Default` lets the descriptor build that payload from one instance.
#[aether_data::kind(name = "aether.test_fixtures.mat4_source_trigger", default)]
pub struct Mat4SourceTrigger;

/// Driver kind for the stateful multi-actor replace fixture (ADR-0101):
/// each `Bump` increments the fixture's in-memory counter by one.
/// Structured-shaped unit struct.
#[aether_data::kind(name = "aether.test_fixtures.bump", default)]
pub struct Bump;

/// Query kind for the stateful replace fixture: request the live counter.
/// The fixture replies with a `CountReport`. Structured-shaped unit struct.
#[aether_data::kind(name = "aether.test_fixtures.count_query", default)]
pub struct CountQuery;

/// Reply to `CountQuery`, and the wire shape of the state bundle the
/// fixture saves in `on_dehydrate` / restores in `on_rehydrate`. A test
/// asserts this value survives a `replace_component` swap via the
/// ADR-0101 hooks (now `WasmActor` defaults, no opt-in).
#[aether_data::kind(name = "aether.test_fixtures.count_report", eq)]
pub struct CountReport {
    pub count: u32,
}

/// Typed config for the `ui_widget` fixture (issue 1793 widget-actor
/// cost spike). `redraw_each_tick` selects the per-frame cost profile:
/// `true` re-emits the full `DrawShapes` batch across the wasm
/// boundary every tick (the naive actor-backed widget), `false`
/// early-returns on tick (the stable-frame floor a host-cached-replay
/// widget pays before the host replays its retained batch — the guest is
/// still dispatched, it just emits nothing). `quad_count` is the draw
/// weight: how many flat `Shape`s the batch carries when it does emit, so
/// the measurement can scale the per-frame re-emit cost with widget
/// visual complexity.
#[aether_data::kind(name = "aether.test_fixtures.ui_widget_config", default, eq)]
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
#[aether_data::kind(name = "aether.test_fixtures.inline_probe", default)]
pub struct InlineProbe;

/// Reply to [`InlineProbe`] — `who` names the actor that handled the
/// query so the test can assert the demux landed on the child vs the
/// parent. Structured-shaped.
#[aether_data::kind(name = "aether.test_fixtures.inline_echo", eq)]
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
/// or to the nested-lineage fixture's inline child (which tears down its
/// leaf). Carries no payload — the recipient address selects which actor
/// tears its child down. Structured-shaped unit struct.
#[aether_data::kind(name = "aether.test_fixtures.despawn_child", default)]
pub struct DespawnChild;

/// ADR-0241 §8 inline-child re-spawn trigger. Sent to the despawn fixture's
/// parent, which spawns a child at its `respawn` key and replies a
/// [`RespawnResult`]. Structured-shaped unit struct.
#[aether_data::kind(name = "aether.test_fixtures.respawn_child", default)]
pub struct RespawnChild;

/// Reply to [`RespawnChild`]: whether the spawn failed because the host
/// allocated no alias for the child's key, which is how a guest learns a
/// despawned child's name is spent.
#[aether_data::kind(name = "aether.test_fixtures.respawn_result", copy, default, eq)]
pub struct RespawnResult {
    pub alias_refused: bool,
}

/// Issue 2690 typed config for the config-carrying inline-child reload
/// fixture: the durable counter's starting value. Distinct from the
/// `()`-config `InlineStatefulChild` — this is the config-bytes case the
/// composite reload bundle dropped before the fix (`reconstruct_one_child`
/// re-inited every child from empty config bytes, so a typed (non-`()`)
/// `Config` decoded `None` and the child was skipped, not just reset).
#[aether_data::kind(name = "aether.test_fixtures.inline_configured_child_config", default, eq)]
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
/// own address; the parent replies a [`TagSpawnReport`] covering the accepted
/// composable spawn plus wrong-parent, non-instanced, and unknown-tag
/// rejections. Structured-shaped unit struct.
#[aether_data::kind(name = "aether.test_fixtures.tag_spawn_query", default)]
pub struct TagSpawnQuery;

/// Reply to [`TagSpawnQuery`], exposing all export-generated by-tag placement
/// outcomes attempted by the fixture during `wire`.
#[aether_data::kind(name = "aether.test_fixtures.tag_spawn_report", eq)]
#[allow(clippy::struct_excessive_bools)] // one observable result per independent placement guard
pub struct TagSpawnReport {
    pub composable_spawned: bool,
    pub wrong_parent_rejected: bool,
    pub non_instanced_rejected: bool,
    pub unknown_tag_rejected: bool,
}

/// Issue 1958: fieldless trigger sent to a `source_forwarder` fixture to make
/// it forward a `SourceQuery`. The forwarder names its target by type — it
/// declares `SourceObserver` as a dependency and mints the reference from that
/// declaration (ADR-0230) — so the trigger carries no address; the forward
/// makes the forwarder the component origin the reader's
/// `ctx.sender()` reads back.
#[aether_data::kind(name = "aether.test_fixtures.send_source_query", default)]
pub struct SendSourceQuery;

/// Issue 1958: unit query sent to a `source_observer` fixture. Its
/// `Manual`-class handler reads `ctx.sender()` and replies a
/// [`SourceReport`].
#[aether_data::kind(name = "aether.test_fixtures.source_query", default)]
pub struct SourceQuery;

/// Issue 1958: the `source_observer` fixture's reply to a [`SourceQuery`].
/// The reply lands on the origin the host stamped on the query, the same
/// origin `ctx.sender()` reads, so where it lands is half the observation.
/// The other half rides in the report as a verdict, never a position.
#[aether_data::kind(name = "aether.test_fixtures.source_report", copy, default, eq)]
pub struct SourceReport {
    /// Whether the observer's `ctx.sender()` returned a proof: `true` for a
    /// component origin, `false` for a Session / `EngineMailbox` origin.
    pub had_sender: bool,
}

/// Issue 2791: trigger for the request-correlation fixture. The fixture
/// sends two `aether.fs.read` requests for this same namespace/path and
/// demuxes the indistinguishable replies by `ctx.in_reply_to()`.
#[aether_data::kind(name = "aether.test_fixtures.run_fs_demux", default)]
pub struct RunFsDemux {
    pub namespace: String,
    pub path: String,
}

/// Issue 2791: report emitted once both same-path fs replies were matched by
/// request id rather than by echoed payload fields.
#[aether_data::kind(name = "aether.test_fixtures.fs_demux_report", eq)]
pub struct FsDemuxReport {
    pub first_matched: bool,
    pub second_matched: bool,
}

/// Issue 5508: trigger for the typed request-context fixture that recovers
/// contexts by trying each context type in turn. The fixture sends two
/// `aether.fs.read` requests carrying distinct context kinds and recovers them
/// from the shared `ReadResult` handler.
#[aether_data::kind(name = "aether.test_fixtures.run_fs_context_demux", default)]
pub struct RunFsContextDemux {
    pub namespace: String,
    pub path: String,
}

/// Issue 5508: report emitted once both distinct typed request contexts were
/// recovered by trying each context type in turn on the shared `ReadResult`
/// handler. Payloads are the values actually decoded from each context, not
/// synthetic flags.
#[aether_data::kind(name = "aether.test_fixtures.fs_context_demux_report", eq)]
pub struct FsContextDemuxReport {
    pub first_payload: u32,
    pub second_payload: u32,
}

/// Ask the probe fixture to unsubscribe itself from `Key` on every window, so
/// a scenario drives a subscriber's own unsubscribe without naming its
/// position. Fieldless: the window cap reads the subscriber off the sender.
#[aether_data::kind(name = "aether.test_fixtures.unsubscribe_keys", default)]
pub struct UnsubscribeKeys;

/// Configure the listener lineage used by the TCP load probe when it echoes
/// frames received from accepted sessions. The probe binds that listener on
/// `127.0.0.1:0` with itself as the consumer (`aether.tcp.bind_listener_self`)
/// and reports the bound port in its snapshot.
#[aether_data::kind(name = "aether.test_fixtures.configure_tcp_load_probe")]
pub struct ConfigureTcpLoadProbe {
    pub listener_name: String,
}

/// Ask the TCP load probe to start a bounded batch of outbound connections.
/// Every session gets a deterministic, unique name below `aether.tcp`.
#[aether_data::kind(name = "aether.test_fixtures.start_tcp_connect_load")]
pub struct StartTcpConnectLoad {
    pub addr: String,
    pub connection_count: u32,
    pub session_name_prefix: String,
}

/// Query the TCP load probe's exact consumer-side accounting.
#[aether_data::kind(name = "aether.test_fixtures.collect_tcp_load_snapshot", default)]
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
/// `local_port` is the port the probe's own listener bound, `None` until the
/// bind reply arrives.
#[aether_data::kind(name = "aether.test_fixtures.tcp_load_snapshot", eq)]
pub struct TcpLoadSnapshot {
    pub sessions: Vec<TcpLoadSessionSnapshot>,
    pub connect_failures: Vec<String>,
    pub local_port: Option<u16>,
}

/// Issue 1977 (ADR-0114 amendment) cluster-addressing matrix driver. Sent to
/// the `matrix_sweep` fixture's parent over the wire to kick off the sweep:
/// the parent drives every in-cluster addressing direction (parent → child,
/// child → parent, child → sibling, child → self) and one cross-cluster send,
/// and each participant records the cell it observed (did the mail arrive,
/// and was `ctx.sender()` the proof of the actor that sent it). The
/// cross-cluster recipient is a declared dependency of the cluster's parent
/// rather than an address on this kind: the parent mints its reference from
/// that declaration (ADR-0230) and records it for the fanning-out child, so
/// the driver is fieldless.
#[aether_data::kind(name = "aether.test_fixtures.run_matrix", default)]
pub struct RunMatrix;

/// Issue 1977 in-cluster ping for the `matrix_sweep` fixture. `cell` selects
/// which matrix cell the recipient records (one of the `MATRIX_CELL_*`
/// markers); `fan_out` (set only on the parent → child a ping) instructs the
/// receiving child to drive the child-origin cells (child → parent, child →
/// sibling, child → self) and the cross-cluster send. The cross-cluster
/// recipient is not threaded here: the parent recorded its proven reference in
/// the cluster-shared log and the child reads it back, because a reference has
/// no codec and so cannot ride a kind (ADR-0230). Structured-shaped.
#[aether_data::kind(name = "aether.test_fixtures.matrix_ping", copy, default)]
pub struct MatrixPing {
    /// Which matrix cell the recipient records (a `MATRIX_CELL_*` marker).
    pub cell: u32,
    /// Set on the parent → child a ping: the receiving child fans out the
    /// child-origin cells and the cross-cluster send. `0` on every other ping.
    pub fan_out: u32,
}

/// Issue 1977 report query for the `matrix_sweep` fixture. Sent to the
/// parent over the wire *after* `RunMatrix` settles; the parent reads the
/// cluster's shared observation log and replies a [`MatrixReport`].
/// Structured-shaped unit struct.
#[aether_data::kind(name = "aether.test_fixtures.collect_matrix", default)]
pub struct CollectMatrix;

/// Issue 1977 structured matrix report — the `matrix_sweep` fixture's reply
/// to [`CollectMatrix`]. Each `*_arrived` flag is `1` when that cell's mail
/// was delivered (the recipient's handler ran). Each `*_sender_matched` flag
/// is `1` when the recipient's `ctx.sender()` equalled the proof it holds of
/// the actor expected to send that cell, compared inside the guest so no
/// position leaves the cluster. The two `observer_reports_to_*` counts are the
/// cross-cluster cells: the `source_observer` replies each report to the
/// origin the host stamped on the query, and only a report whose
/// `had_sender` is set is counted. Structured-shaped.
#[aether_data::kind(name = "aether.test_fixtures.matrix_report", default, eq)]
pub struct MatrixReport {
    /// parent → child a (in place): did child a receive the ping.
    pub parent_to_child_arrived: u32,
    /// parent → child a: was child a's sender the proof of its parent.
    pub parent_to_child_sender_matched: u32,
    /// child a → parent (in place): did the parent receive the ping.
    pub child_to_parent_arrived: u32,
    /// child a → parent: was the parent's sender the proof of child a.
    pub child_to_parent_sender_matched: u32,
    /// child a → sibling child b (in place): did child b receive the ping.
    pub child_to_sibling_arrived: u32,
    /// child a → sibling: was child b's sender the proof of child a.
    pub child_to_sibling_sender_matched: u32,
    /// child a → self (in place): did child a receive its own ping.
    pub child_to_self_arrived: u32,
    /// child a → self: was child a's sender the proof of child a.
    pub child_to_self_sender_matched: u32,
    /// Observer reports that landed on the parent. The parent's own query
    /// before the fan-out brings back exactly one.
    pub observer_reports_to_parent: u32,
    /// Observer reports that landed on a child. Child a's query during the
    /// in-place drain brings back exactly one when the drain stamps child a,
    /// not the parent, as that send's origin.
    pub observer_reports_to_child: u32,
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
#[aether_data::kind(name = "aether.test_fixtures.editor_region_probe.config", default, eq)]
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
#[aether_data::kind(name = "aether.test_fixtures.drain_editor_inputs", default)]
pub struct DrainEditorInputs;

/// Reply containing every editor input observed since the previous drain.
#[aether_data::kind(name = "aether.test_fixtures.drain_editor_inputs_result", partial_eq)]
pub struct DrainEditorInputsResult {
    pub region_name: String,
    pub inputs: Vec<ObservedEditorInput>,
}

/// Stored kind id that makes the reactor fixture's shared fold refuse.
pub const REACTOR_FOLD_FAIL_KIND: aether_data::KindId =
    aether_data::storage_kind_id_from_name("test.bloomery.reactor.fold_fail");

/// Mirrors `aether-test-fixtures-program`'s private summarize input: the same
/// name and shape, so the same digest.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.input")]
pub struct SummarizeInput {
    pub text: Ref<Utf8Text>,
}

/// Program-bundle head the summarize-caller reactor rule names.
pub const SUMMARIZE_BUNDLE: Head<OpaqueBytes> = Head::new("test.bloomery.summarize.bundle");

/// Program the summarize-caller reactor rule invokes.
pub const SUMMARIZE_PROGRAM: &str = "test.program.summarize";

/// Bundle head the mixed fixture's reactor rule names for its own program.
pub const MIXED_BUNDLE: Head<OpaqueBytes> = Head::new("test.bloomery.mixed.bundle");

const _: () = assert!(ProgramName::is_valid(SUMMARIZE_PROGRAM));

/// Issue 6400: trigger that makes the correlation-carry requester send one
/// [`CarriedRequest`] carrying `tag`, with the same `tag` bound as the
/// request's context.
#[aether_data::kind(name = "aether.test_fixtures.run_carried_request", copy, default)]
pub struct RunCarriedRequest {
    pub tag: u32,
}

/// Issue 6400: request the correlation-carry requester sends to the reply
/// holder, which parks its reply handle until [`ReleaseCarried`].
#[aether_data::kind(name = "aether.test_fixtures.carried_request", copy)]
pub struct CarriedRequest {
    pub tag: u32,
}

/// Issue 6400: tells the reply holder to answer every parked request, in
/// arrival order.
#[aether_data::kind(name = "aether.test_fixtures.release_carried", default)]
pub struct ReleaseCarried;

/// Issue 6400: the reply holder's answer to a [`CarriedRequest`], echoing its
/// `tag`.
#[aether_data::kind(name = "aether.test_fixtures.carried_request_result", copy)]
pub struct CarriedRequestResult {
    pub tag: u32,
}

/// Issue 6400: report the requester emits when a [`CarriedRequestResult`] recovered
/// the context of the request it answers.
#[aether_data::kind(name = "aether.test_fixtures.carried_reply_matched", default)]
pub struct CarriedReplyMatched;

/// Issue 6983: [`RunHeldRequest::target`] naming the held relay, which holds
/// its reply in a request context.
pub const HELD_TARGET_RELAY: u32 = 0;

/// Issue 6983: [`RunHeldRequest::target`] naming the held keeper, which holds
/// its reply in state and saves it on dehydrate.
pub const HELD_TARGET_KEEPER: u32 = 1;

/// Issue 6983: [`RunHeldRequest::target`] naming the held forgetter, which
/// holds its reply in state and saves nothing on dehydrate.
pub const HELD_TARGET_FORGETTER: u32 = 2;

/// Issue 6983: trigger that makes the held requester send one [`HeldRequest`]
/// carrying `tag` to the held actor `target` names (`HELD_TARGET_*`).
#[aether_data::kind(name = "aether.test_fixtures.run_held_request", copy, default)]
pub struct RunHeldRequest {
    pub tag: u32,
    pub target: u32,
}

/// Issue 6983: request a held actor answers later, through a held reply.
#[aether_data::kind(name = "aether.test_fixtures.held_request", copy)]
pub struct HeldRequest {
    pub tag: u32,
}

/// Issue 6983: a held actor's answer to a [`HeldRequest`], echoing its `tag`.
#[aether_data::kind(name = "aether.test_fixtures.held_request_result", copy)]
pub struct HeldRequestResult {
    pub tag: u32,
}

/// Issue 7015: the [`HeldRequestResult::tag`] a held actor that closed before
/// answering sends in its place (ADR-0243 §6).
pub const HELD_UNANSWERED_TAG: u32 = u32::MAX;

impl aether_actor::HeldReply for HeldRequestResult {
    fn unanswered() -> Self {
        Self { tag: HELD_UNANSWERED_TAG }
    }
}

/// Issue 6983: report the held requester emits when a [`HeldRequestResult`] echoes
/// the tag of the request it sent.
#[aether_data::kind(name = "aether.test_fixtures.held_reply_matched", default)]
pub struct HeldReplyMatched;

/// Issue 7015: report the held requester emits when a [`HeldRequestResult`]
/// carries [`HELD_UNANSWERED_TAG`]: its holder closed before answering.
#[aether_data::kind(name = "aether.test_fixtures.held_reply_unanswered", default)]
pub struct HeldReplyUnanswered;

/// Issue 6983: tells a held keeper or forgetter to answer the reply it holds.
#[aether_data::kind(name = "aether.test_fixtures.release_held", default)]
pub struct ReleaseHeld;

/// Issue 7109: a probe the second version of the republish gate records in
/// arrival order. The first version has no row for it, so only the guest a
/// republish installs ever handles one.
#[aether_data::kind(name = "aether.test_fixtures.gate_probe", copy)]
pub struct GateProbe {
    pub seq: u32,
}

/// Issue 7109: asks the republish gate for the probes it has recorded. It
/// replies a [`GateQueryResult`].
#[aether_data::kind(name = "aether.test_fixtures.gate_query", default)]
pub struct GateQuery;

/// Issue 7109: the republish gate's answer to a [`GateQuery`], each recorded
/// [`GateProbe::seq`] in arrival order.
#[aether_data::kind(name = "aether.test_fixtures.gate_query_result", default, eq)]
pub struct GateQueryResult {
    pub seqs: Vec<u32>,
}

/// Issue 7109: the config every instance of the republish gate is loaded
/// with.
#[aether_data::kind(name = "aether.test_fixtures.gate.config", default, eq)]
pub struct GateConfig;

/// Issue 7109: the config of the republish peer. With `trap_on_rehydrate`
/// set, the second version traps in `on_rehydrate`, so a republish that
/// carries the peer's state fails there.
#[aether_data::kind(name = "aether.test_fixtures.peer.config", copy, default, eq)]
pub struct PeerConfig {
    pub trap_on_rehydrate: bool,
}

/// Issue 7109: the state the republish peer carries across a replace.
#[aether_data::kind(name = "aether.test_fixtures.peer_state", copy, default, eq)]
pub struct PeerState {
    pub count: u32,
}

/// Issue 7086: the config every instance of the third version of the
/// republish gate is built with, a kind the first two versions' gate does
/// not declare, so republishing to the third changes the gate's config kind.
/// The third version's gate answers `GateQuery` with `[label]`.
#[aether_data::kind(name = "aether.test_fixtures.gate.labelled_config", copy, default, eq)]
pub struct GateLabelledConfig {
    pub label: u32,
}

/// Issue 7086: asks the republish loader to load `wasm` as a component under
/// `name`, exporting `export`, through the component host, the way any
/// guest loads one. It replies the host's `LoadResult`.
#[aether_data::kind(name = "aether.test_fixtures.guest_load")]
pub struct GuestLoad {
    pub wasm: Vec<u8>,
    pub name: Option<String>,
    pub export: Option<String>,
}

/// Issue 7086: asks the republish gate how many times its `wire` hook has
/// run on this instance. It replies a [`CountReport`].
#[aether_data::kind(name = "aether.test_fixtures.wire_count_query", default)]
pub struct WireCountQuery;
