//! aether-kinds: the substrate's own mail vocabulary. Imported by any
//! actor that wants to send mail to the substrate, receive mail the
//! substrate dispatches (tick, input), or consume the substrate's sink
//! kinds (`draw_triangle`). See ADR-0005 / ADR-0030.
//!
//! Kind ids are `fnv1a_64(KIND_DOMAIN ++ canonical(name, schema))` — a compile-time
//! const on the `Kind` trait (ADR-0030 Phase 2). Substrate boot and
//! guest SDK arrive at the same id independently; no host-fn resolve
//! round-trip. Consumers address kinds via the `NAME` constants and
//! the derived `ID` constants on the impls below.

#![no_std]

extern crate alloc;

pub mod descriptors;
pub mod keycode;
pub mod text_metrics;
pub mod trace;

pub use text_metrics::{CachedFontMetrics, scale_units};

use aether_math::{Mat4, Vec4};
use alloc::string::String;
use bytemuck::{Pod, Zeroable};

// Every kind below derives both `Kind` and `Schema`. Pre-ADR-0032
// `Schema` was gated behind a `descriptors` feature so wasm guests
// stayed free of hub-protocol; that gate retired once hub-protocol
// went no_std + alloc. `Schema` drives both the canonical bytes the
// `aether.kinds` section carries and the `LABEL_NODE` sidecar — so
// it's load-bearing on every build, not an optional enrichment.

// ADR-0082 lifecycle stage kinds. Empty payload — the broadcast is the
// signal. Future revisions may add per-stage fields (frame_no on Tick,
// vp matrix on Render) once stage payload semantics settle; v1 keeps
// the wire shape minimal so the application-declared graph can drive
// stage timing without committing to a fixed payload schema.

/// Per-frame lifecycle stage (ADR-0082 §11). Empty payload —
/// elapsed-time is parked until a subscriber actually needs it. The
/// kind moved from `aether.tick` into the `aether.lifecycle.*` family
/// in PR 4 so the lifecycle stage vocabulary reads as one namespace.
///
/// ADR-0033 handler dispatch (`#[actor]` synthesized
/// `__aether_dispatch`) decodes every typed handler via
/// `Mail::decode_typed::<K>()`, which requires `K: AnyBitPattern`.
/// Zero-sized unit kinds like `Tick` trivially satisfy that through
/// `Pod` + `Zeroable` — no padding, no uninitialized bits.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.tick")]
pub struct Tick;

/// Lifecycle stage broadcast — capability init pass (ADR-0082 §5).
/// Fires once at chassis boot, after every capability's actor-framework
/// `claim → init → wire → spawn` completes and before
/// [`InitComponents`] fires. Capabilities that need to send mail to
/// peers during boot subscribe to this stage.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.init_caps")]
pub struct InitCaps;

/// Lifecycle stage broadcast — component init pass (ADR-0082 §5).
/// Fires once after [`InitCaps`] settles, before the per-frame loop
/// begins. Component-category actors subscribe here when they need to
/// reach already-wired capabilities during their boot logic.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.init_components")]
pub struct InitComponents;

/// Lifecycle stage broadcast — render stage (ADR-0082 §1). Fires every
/// frame after the whole [`Tick`] chain has settled (ADR-0080 §6) on
/// chassis that declare a render state in their lifecycle graph (today:
/// desktop and `test_bench`). Render-producing actors compute their
/// per-frame state on [`Tick`] and submit it to `aether.render` here, on
/// `Render` — so a submission integrates the fully-settled cross-actor
/// state of the frame rather than racing other actors' Tick handlers.
/// Headless / hub chassis omit this state from their graph; subscribing
/// on a chassis that doesn't declare it rejects fail-fast at wire time
/// per ADR-0082 §7.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.render")]
pub struct Render;

/// Lifecycle stage broadcast — frame-present stage (ADR-0082 §1).
/// Fires every frame after [`Render`] on chassis that drive a display.
/// The default desktop graph routes the quit edge through this stage so
/// the current frame finishes drawing before shutdown.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.present")]
pub struct Present;

/// Lifecycle stage broadcast — shutdown stage (ADR-0082 §1). Fires
/// once when the graph reaches a terminal state. Subscribers perform
/// graceful cleanup with the full mail surface still operational
/// (save game state, flush a write, post a metric) before the chassis
/// runs each actor's `unwire` finaliser. Distinct from the actor
/// framework's per-actor `unwire` hook — ADR-0082 §12.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.shutdown")]
pub struct Shutdown;

/// Lifecycle escape signal (ADR-0082 §3). The one hardcoded signal the
/// driver recognises. Setting `quit_pending = true` on receipt; the
/// flag is consumed at the next state whose graph declares a `quit`
/// edge. Chassis bridges OS-level termination signals (ctrlc, winit
/// `WindowEvent::CloseRequested`, future hub-shutdown mail) to this
/// kind so three trigger sources converge on one consumption point.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.quit")]
pub struct Quit;

/// Driver-internal trigger that advances the lifecycle state machine
/// by one step (ADR-0082 §2). The chassis main loop sends this each
/// frame; the driver responds by minting the current state's payload
/// via its factory, broadcasting to subscribers, awaiting settlement,
/// and advancing the internal state pointer along the resolved edge
/// (`next` or `quit`). Not exposed via the `aether.lifecycle.*` stage
/// vocabulary because it carries no semantic meaning to subscribers;
/// it's the cadence input, not a stage broadcast.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.advance")]
pub struct LifecycleAdvance;

/// Reply to [`LifecycleAdvance`] signalling that the stage's broadcast
/// root has settled (ADR-0082 §6). The chassis main loop wait-replies
/// on this so cadence couples to actual work completion — back-pressure
/// flows from subscriber drain time back to the chassis. `completed`
/// is the kind id of the state the driver just finished broadcasting;
/// `next` is the kind id of the state the driver will broadcast on the
/// next [`LifecycleAdvance`], or `0` when the lifecycle reached a
/// terminal state.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
)]
#[kind(name = "aether.lifecycle.advance_complete")]
pub struct LifecycleAdvanceComplete {
    pub completed: u64,
    pub next: u64,
}

/// Subscribe a mailbox to a lifecycle stage broadcast (ADR-0082 §7).
/// `stage` is the [`KindId`](aether_data::KindId) of the stage kind
/// (e.g. `<Tick as Kind>::ID.0`); `mailbox` is the subscriber's mailbox
/// id. Substrate replies with [`LifecycleSubscribeResult`] —
/// `Err { reason: UnsupportedStage }` when the chassis's lifecycle
/// graph doesn't declare a state at that kind, fail-fast at wire time
/// per ADR-0082 §7.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.subscribe")]
pub struct LifecycleSubscribe {
    pub stage: u64,
    pub mailbox: u64,
}

/// Reflexive counterpart of [`LifecycleSubscribe`]: subscribe the
/// *sending* actor to a lifecycle stage broadcast, with no explicit
/// `mailbox` field. The cap resolves the subscriber from the inbound
/// envelope's host-stamped `Source` (ADR-0083) via
/// `ctx.source_mailbox()`, so the subscriber cannot be forged and the
/// op is gated to in-process actors by construction — an external
/// session or another engine has no local mailbox and gets an `Err`
/// reply, pushing it onto the named [`LifecycleSubscribe`] form. This
/// is the common "subscribe me" case; `stage` carries the same
/// [`KindId`](aether_data::KindId) as [`LifecycleSubscribe`]. Substrate
/// replies with [`LifecycleSubscribeResult`].
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.subscribe_self")]
pub struct LifecycleSubscribeSelf {
    pub stage: u64,
}

/// Unsubscribe counterpart of [`LifecycleSubscribe`]. Idempotent on
/// "not currently subscribed."
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.unsubscribe")]
pub struct LifecycleUnsubscribe {
    pub stage: u64,
    pub mailbox: u64,
}

/// Reflexive counterpart of [`LifecycleUnsubscribe`]: unsubscribe the
/// *sending* actor from a lifecycle stage, with no explicit `mailbox`
/// field. The cap resolves the subscriber from the inbound envelope's
/// host-stamped `Source` (ADR-0083), the same gating as
/// [`LifecycleSubscribeSelf`]. Idempotent on "not currently
/// subscribed." Substrate replies with [`LifecycleSubscribeResult`].
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.unsubscribe_self")]
pub struct LifecycleUnsubscribeSelf {
    pub stage: u64,
}

/// `aether.lifecycle.unsubscribe_all` — remove `mailbox` from every
/// lifecycle stage's subscriber set. Issued by
/// `ComponentHostCapability` on `DropComponent` so the lifecycle cap's
/// per-stage broadcast doesn't keep firing at a dropped trampoline —
/// the lifecycle-family counterpart of `UnsubscribeAll` for
/// `aether.input`. Idempotent: a mailbox with no stage subscriptions
/// is still a no-op. Fire-and-forget; no reply. Cast-shape (Pod), one
/// `mailbox` field, matching the sibling lifecycle kinds' raw-`u64`
/// shape.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.lifecycle.unsubscribe_all")]
pub struct LifecycleUnsubscribeAll {
    pub mailbox: u64,
}

/// Reply to [`LifecycleSubscribe`] / [`LifecycleUnsubscribe`].
/// `Err` carries the stage kind id and a human-readable reason —
/// fail-fast subscribe per ADR-0082 §7. Same shape and rationale as
/// `SubscribeInputResult` for input subscriptions.
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "aether.lifecycle.subscribe_result")]
pub enum LifecycleSubscribeResult {
    Ok,
    Err { stage: u64, error: String },
}

/// A single keyboard keypress, identified by the stable codes in
/// `keycode`. Dispatched on press only (no repeat). Released keys
/// arrive as `KeyRelease`. Unmapped winit keys (any `KeyCode` variant
/// the substrate doesn't translate) produce no mail.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.key")]
pub struct Key {
    pub code: u32,
}

/// Release counterpart of `Key`. Dispatched once per key release, with
/// the same `code` value the press carried. Components tracking
/// hold-to-act semantics (e.g. WASD movement) pair subscription to
/// both kinds so they can clear state on release.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.key_release")]
pub struct KeyRelease {
    pub code: u32,
}

/// A mouse-button press. No payload today — which button isn't tracked.
/// Zero-sized but derives `Pod` / `Zeroable` for the same reason as
/// `Tick` — see the note on that type.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.mouse_button")]
pub struct MouseButton;

/// Cursor position in window coordinates, as logical pixels cast to f32.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.mouse_move")]
pub struct MouseMove {
    pub x: f32,
    pub y: f32,
}

/// Current window size in physical pixels. Published by the desktop
/// chassis on startup (once the window exists) and on every
/// `WindowEvent::Resized` that isn't a zero-dimension minimize.
/// Headless and hub chassis never publish — they have no window. A
/// client that needs to map pixel-space input (e.g. `MouseMove`) to
/// clip-space geometry subscribes to this kind and caches the latest
/// value; the initial value arrives right after the component's
/// auto-subscribe fires, without any request/reply dance.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.window_size")]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

// The render cap's drawing/texture kinds — `Vertex` / `DrawTriangle` /
// `DRAW_TRIANGLE_BYTES` / `Camera` and the `aether.render.*` texture +
// quad family — moved to `aether_capabilities::render::kinds` (ADR-0121,
// "capabilities own their kinds"). The capture-request and `FrameCheck`
// verification kinds stay below: `aether-mcp` and the substrate core
// consume them, so moving them would close a dependency cycle.

// `aether.camera.*` control kinds (CameraCreate / CameraDestroy /
// CameraSetActive / CameraSetMode / CameraOrbitSet / CameraTopdownSet)
// live in `mod control_plane` below — they're structured because
// every one carries a `String` name and `Option<...>` per-field
// deltas, so they can't ride the cast-shaped path.

/// Input to the `mat4_apply` native transform (ADR-0048, issue 1464):
/// apply a 4×4 matrix to a 4-vector, `M · v`. Both operands ride in
/// one kind so the transform stays a unary `Kind → Kind` node — a
/// two-operand transform would need multi-input slot wiring.
///
/// `matrix` is the `aether_math::Mat4` operand (column-major, the same
/// layout as the substrate's `view_proj` uniform). `vector` is the
/// homogeneous `aether_math::Vec4` — the apply is a raw left-multiply
/// with the `w` weight carried and no perspective divide, so a point
/// (`w = 1`) picks up the translation column and a direction (`w = 0`)
/// does not.
///
/// Cast-shaped (`#[repr(C)]` + `Pod`, like `Vec4` and `Camera`),
/// composing the math primitives directly rather than flattening them
/// to raw `[f32; N]` arrays. The `Kind` canonical encode/decode keeps
/// the transform boundary consistent: a source encodes its output and
/// the transform decodes its input through the same shape-agnostic
/// `Kind` path, so cast bytes agree on both sides.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.math.mat4_apply")]
pub struct Mat4Apply {
    pub matrix: Mat4,
    pub vector: Vec4,
}

/// Request addressed to a component that supports the ADR-0013
/// reply-to-sender smoke path. The component answers with `Pong`
/// carrying the same `seq`; the round trip proves that a Claude
/// session → component → session reply actually works end-to-end.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.ping")]
pub struct Ping {
    pub seq: u32,
}

/// Reply-to-sender counterpart to `Ping`. The `seq` is the incoming
/// `Ping.seq` echoed back so the caller can match requests against
/// replies when multiple are in flight.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.pong")]
pub struct Pong {
    pub seq: u32,
}

/// Diagnostic the hub emits back to an originating engine when mail
/// that engine bubbled up (ADR-0037) doesn't resolve at the hub
/// either. Lands on the engine's `aether.diagnostics` sink, which
/// re-warns locally so the unresolved address surfaces in that
/// engine's `engine_logs` rather than only in the hub's. Closes the
/// "typo diagnostics" follow-up from ADR-0037 (issue #185).
///
/// `recipient_mailbox_id` is the hashed mailbox id the originator
/// sent to — the id space is cross-process-stable (ADR-0029 /
/// ADR-0030 / issue #186) so agents can map it back to a name in
/// tooling. `kind_id` is the kind the original mail carried.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.mail.unresolved")]
pub struct UnresolvedMail {
    pub recipient_mailbox_id: aether_data::MailboxId,
    pub kind_id: aether_data::KindId,
}

/// Issue 607 Phase 4b (ADR-0079): framework-emitted close
/// notification. Sent to every monitor a closing actor accumulated via
/// `NativeCtx::monitor` — the substrate drains `monitors_of[target]`
/// after the target's `unwire` runs, fires one `MonitorNotice` per
/// watcher, and only then flips the target's slot from `Live` to
/// `Dead`.
///
/// The watcher receives this kind as ordinary mail; its `#[handler]`
/// reads `target` to identify which actor it was monitoring. v1 carries
/// only the target id — no `CloseReason` field — so the wire shape is
/// purely additive if a future revision wants to surface trap vs
/// shutdown vs cooperative close.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.actor.monitor_notice")]
pub struct MonitorNotice {
    pub target: aether_data::MailboxId,
}

// Reserved control-plane vocabulary (ADR-0010). The substrate handles
// these kinds inline rather than dispatching to a component — the
// namespace itself is the routing discriminator. ADR-0019 PR 5 turned
// these from Opaque markers into real schema-described types: their
// fields are structured-encoded on the wire, hub-encodable from agent
// params (no more `payload_bytes` workaround), and the substrate
// decodes them with `wire::from_bytes` against the same types
// that ship as the kind.
//
// Gated behind `descriptors` because the types use `String`/`Vec`/
// `Option` — wasm guests that don't enable descriptors stay free of
// the alloc-heavy payload types (and have no business loading
// components anyway).

pub use control_plane::*;
pub use engine::*;
pub use trajectory::*;

mod engine {
    use alloc::string::String;
    use alloc::vec::Vec;

    use serde::{Deserialize, Serialize};

    /// `aether.engine.list` — ask the engines cap (`aether.engine`) to
    /// enumerate every engine it currently supervises. Fieldless
    /// request; the reply is a [`ListEnginesResult`]. Issue 763 P4.
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.engine.list")]
    pub struct ListEngines {}

    /// One supervised engine, as reported in a [`ListEnginesResult`].
    ///
    /// `engine_id` is the plain UUID string the engines cap minted at
    /// spawn time — `EngineId` itself doesn't implement `Schema`, so
    /// the wire carries the string form (the same convention the
    /// `aether.process.*` kinds use). `rpc_port` is the localhost port
    /// the cap assigned the substrate's `RpcServerCapability`.
    ///
    /// `last_heartbeat_age_millis` is how long ago the cap last saw a
    /// liveness signal from this engine (issue 1339) — `0` right after
    /// spawn, refreshed each time the engine's proxy confirms a `Pong`.
    /// A value climbing past the heartbeat interval means the engine is
    /// going stale; the cap evicts it (drops it from this list) once it
    /// crosses the miss limit.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct EngineDescriptor {
        pub engine_id: String,
        pub rpc_port: u16,
        pub last_heartbeat_age_millis: u64,
    }

    /// Why an engine left the supervised-engine table, as carried in a
    /// [`DeadEngineDescriptor`] (and in the `EngineDied` signal for the
    /// two self-death paths). A tagged enum so an observer can branch on
    /// the cause without parsing free text; the `detail` string on the
    /// non-clean variants carries the specifics.
    ///
    /// - `Terminated` — a deliberate `aether.engine.terminate` shut the
    ///   engine down. The clean-shutdown case; carries no detail.
    /// - `Crashed { detail }` — the substrate closed its RPC connection
    ///   (`Bye` / eof) on its own; `detail` is the close reason the proxy
    ///   observed.
    /// - `Evicted { detail }` — the liveness heartbeat crossed its miss
    ///   limit and the proxy declared the engine dead; `detail` is the
    ///   `heartbeat miss limit N of M` count.
    /// - `SpawnFailed { detail }` — the spawn never connected: the
    ///   substrate failed to come up (fork / materialize error, or the
    ///   proxy connect/boot failed), so it was never registered alive.
    ///   `detail` is the proxy connect / boot error. Distinct from
    ///   `Crashed`, which is a registered substrate that later closed its
    ///   connection.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum DeathReason {
        Terminated,
        Crashed { detail: String },
        Evicted { detail: String },
        SpawnFailed { detail: String },
    }

    /// One recently-departed engine, as reported in a
    /// [`ListEnginesResult`]'s `recently_died` ring (the bounded last-N
    /// deaths the engines cap retains). Distinct from [`EngineDescriptor`]:
    /// a dead engine carries a [`DeathReason`] and an age-since-death
    /// rather than a live heartbeat age. `engine_id` / `rpc_port` are the
    /// same identifiers it carried while alive; `died_age_millis` is how
    /// long ago the cap removed it from the supervised table.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct DeadEngineDescriptor {
        pub engine_id: String,
        pub rpc_port: u16,
        pub reason: DeathReason,
        pub died_age_millis: u64,
    }

    /// `aether.engine.list_result` — reply to [`ListEngines`]: every
    /// engine the cap supervises right now, plus a bounded sidecar of the
    /// engines that recently left and why. Issue 763 P4.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.list_result")]
    pub struct ListEnginesResult {
        pub engines: Vec<EngineDescriptor>,
        /// The recently-died ring: the last few engines that left the
        /// supervised table, each with why it left ([`DeathReason`]) and
        /// how long ago. Lets an observer tell a clean `terminate` from a
        /// crash or a heartbeat eviction without grepping host logs.
        pub recently_died: Vec<DeadEngineDescriptor>,
    }

    /// How a [`SpawnEngine`] names the binary to fork — a registry
    /// selector resolved against the hub's content-addressed binary store
    /// (ADR-0115), not a host filesystem path. The engines cap resolves it
    /// in this order:
    ///
    /// - `query` is the exact selector token: a sha256 content `hash`, a
    ///   `name@version` (where `version` is the binary's self-reported
    ///   build id — its manifest `git_sha`), or a `name` an upload pointed
    ///   at a hash. `None` means `default` — the configured fallback, the
    ///   `headless` chassis (no window, runs on any host), so a bare
    ///   `SpawnEngine` with an empty selector returns a working engine.
    /// - `chassis` / `caps` / `target` are an attribute query over the
    ///   stored manifests, consulted when `query` is `None`: keep only
    ///   binaries whose `chassis` matches, whose linked `caps` are a
    ///   superset of every listed cap, and whose `target` triple matches.
    ///   They mirror [`ListEngineBinaries`]' filter shape.
    ///
    /// An exact `query` wins first; absent one, the attribute query
    /// resolves, then `default`. A selector that resolves to no stored
    /// binary fails the spawn.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct BinarySelector {
        pub query: Option<String>,
        pub chassis: Option<String>,
        pub caps: Vec<String>,
        pub target: Option<String>,
    }

    /// `aether.engine.spawn` — ask the engines cap to fork+exec a
    /// substrate binary and connect a per-engine proxy to it. Issue
    /// 763 P4.
    ///
    /// The cap resolves `selector` against its content-addressed binary
    /// store (ADR-0115) to the stored content bytes, materializes them to
    /// an executable temp file, picks a free localhost port for the
    /// substrate's `RpcServerCapability`, injects it as `AETHER_RPC_PORT`,
    /// forks the realized binary with `args` forwarded verbatim, then
    /// boots an `aether.engine.proxy:<id>` actor that dials it. Reply:
    /// [`SpawnEngineResult`] — `Err` if the selector resolves to no stored
    /// binary. The host filesystem path is gone from the spawn surface;
    /// the only path input is the one-time [`UploadBinary`].
    ///
    /// `boot_manifest` (when `Some`) is the absolute path to a
    /// `BundleManifest` JSON of components to auto-load at boot; the cap
    /// injects it as `AETHER_BOOT_MANIFEST` alongside `AETHER_RPC_PORT`,
    /// and the spawned chassis reads the listed wasm itself (spawn is
    /// single-host) so the engine comes up with those components already
    /// loading — no follow-up `load_component` round-trips. `None` boots
    /// a bare engine, the pre-existing behaviour.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.spawn")]
    pub struct SpawnEngine {
        pub selector: BinarySelector,
        pub args: Vec<String>,
        pub boot_manifest: Option<String>,
    }

    /// Reply to [`SpawnEngine`]. Issue 763 P4.
    ///
    /// `Ok` carries the freshly minted `engine_id` (plain UUID string —
    /// pass it back to [`TerminateEngine`]) and the `rpc_port` the cap
    /// assigned. `Err` carries a free-form reason — fork failure, or
    /// the proxy failing to connect within the substrate's startup
    /// window — plus `engine_id`, the allocated id when the failure came
    /// after the cap minted one (`None` for a pre-allocation failure
    /// like a selector miss or a port-allocation error). A failed spawn
    /// with `engine_id = Some(_)` also leaves a matching `SpawnFailed`
    /// entry in [`ListEnginesResult`]'s `recently_died` ring, so a caller
    /// can correlate and reap. On `Err` no child process is left running.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.spawn_result")]
    pub enum SpawnEngineResult {
        Ok {
            engine_id: String,
            rpc_port: u16,
        },
        Err {
            engine_id: Option<String>,
            error: String,
        },
    }

    /// `aether.engine.terminate` — ask the engines cap to shut down a
    /// supervised engine. Issue 763 P4.
    ///
    /// The cap forwards this kind to the engine's
    /// `aether.engine.proxy:<id>` actor, which SIGKILLs the child
    /// substrate it forked and self-shuts-down. `engine_id` is the
    /// plain UUID string from [`SpawnEngineResult`] /
    /// [`ListEnginesResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.terminate")]
    pub struct TerminateEngine {
        pub engine_id: String,
    }

    /// Reply to [`TerminateEngine`]. Issue 763 P4. `Err` is for an
    /// `engine_id` that doesn't parse or names no supervised engine.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.terminate_result")]
    pub enum TerminateEngineResult {
        Ok,
        Err { error: String },
    }

    /// What a stored binary *is*, captured once at upload time by forking
    /// the binary with `--describe` (ADR-0115, issue 1953). The
    /// content-addressed store sidecars one of these next to each entry's
    /// bytes, and [`ListEngineBinariesResult`] returns it per entry so an
    /// observer (and the spawn cutover, #1954) can tell a `headless` from
    /// a `desktop` binary, see which capabilities it links, and read its
    /// build provenance — all without re-running the binary.
    ///
    /// - `chassis` — the chassis profile (`Chassis::PROFILE`):
    ///   `"headless"` / `"desktop"` / `"hub"`.
    /// - `caps` — the mailbox namespaces the chassis registers (its
    ///   linked capabilities, e.g. `aether.fs`, `aether.render`).
    /// - `git_sha` / `profile` / `target` — build provenance from the
    ///   bundle's `build.rs` (`git rev-parse --short HEAD`, the cargo
    ///   build profile, the target triple); `git_sha` is `"unknown"` when
    ///   the binary was built outside a git checkout.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct BinaryManifest {
        pub chassis: String,
        pub caps: Vec<String>,
        pub git_sha: String,
        pub profile: String,
        pub target: String,
    }

    /// One stored binary in a [`ListEngineBinariesResult`] (ADR-0115, issue
    /// 1953). `hash` is the sha256 hex over the binary's raw bytes — the
    /// content-address key. `name` is the optional human-readable name an
    /// upload pointed at this hash (the latest upload that named it wins).
    /// `manifest` is what the binary reported via `--describe`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct BinaryEntry {
        pub hash: String,
        pub name: Option<String>,
        pub manifest: BinaryManifest,
    }

    /// `aether.engine.upload_binary` — ingest a binary into the hub's
    /// content-addressed store (ADR-0115, issue 1953). `staged_path` is an
    /// absolute host path the hub reads itself (aether-mcp never reads the
    /// bytes — a binary is too large to ride the tool channel); the cap
    /// sha256-hashes the bytes, dedups against the existing store, forks
    /// `staged_path --describe` to capture its [`BinaryManifest`], and
    /// stores both. `name`, when set, points that human-readable name at
    /// the resulting hash. Reply: [`UploadBinaryResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.upload_binary")]
    pub struct UploadBinary {
        pub staged_path: String,
        pub name: Option<String>,
    }

    /// Reply to [`UploadBinary`] (ADR-0115, issue 1953). `Ok` carries the
    /// content-address `hash` the bytes stored under (a re-upload of
    /// identical bytes returns the same hash) and the `name` now pointing
    /// at it, if any. `Err` carries a free-form reason — an unreadable
    /// `staged_path`, or a `--describe` that failed or didn't yield a
    /// parseable manifest.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.upload_binary_result")]
    pub enum UploadBinaryResult {
        Ok { hash: String, name: Option<String> },
        Err { error: String },
    }

    /// `aether.engine.list_binaries` — enumerate the hub's stored binaries
    /// (ADR-0115, issue 1953). The filter fields are AND-combined and each
    /// defaults to "no constraint": `chassis` keeps only entries whose
    /// `manifest.chassis` matches, `caps` keeps only entries whose
    /// `manifest.caps` is a superset of every listed cap, `target` keeps
    /// only entries whose `manifest.target` matches. Reply:
    /// [`ListEngineBinariesResult`].
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.engine.list_binaries")]
    pub struct ListEngineBinaries {
        pub chassis: Option<String>,
        pub caps: Vec<String>,
        pub target: Option<String>,
    }

    /// Reply to [`ListEngineBinaries`] (ADR-0115, issue 1953): every stored
    /// binary matching the filter, each as a [`BinaryEntry`] carrying its
    /// hash, optional name, and `--describe` manifest.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.list_binaries_result")]
    pub struct ListEngineBinariesResult {
        pub binaries: Vec<BinaryEntry>,
    }

    /// What a stored component *is*, read straight from the wasm at upload
    /// time — no `--describe` execution step (ADR-0116, issue 1956). A
    /// component embeds its manifest in the `aether.kinds.inputs` /
    /// `aether.namespace` custom sections (ADR-0028 / ADR-0033 / ADR-0096),
    /// so the hub reads it with `wasmparser`, the same reader the substrate
    /// uses at load. The store sidecars one of these next to each
    /// component entry's bytes, and [`ListComponentBinariesResult`] returns
    /// it per entry so an observer (and the resolve query) can select a component
    /// by what it is.
    ///
    /// - `namespaces` — every exported actor's `Addressable::NAMESPACE`. A
    ///   single-actor module yields one; a multi-actor module
    ///   (`export!(A, B, …)`) yields one per type, the entry type first.
    /// - `actors` — one [`ComponentActor`] per exported actor type, the
    ///   `module@actor` selector axis (ADR-0096 export selector).
    /// - `handled_kinds` — the union of every actor's handled `KindId`s
    ///   (ADR-0030), the handled-kind selector axis.
    /// - `fallback` — whether any exported actor declares a `#[fallback]`.
    /// - `provenance` — the wasm `producers` custom section rendered as a
    ///   short string (`"<tool> <version>; …"`), or empty when absent.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct ComponentManifest {
        pub namespaces: Vec<String>,
        pub actors: Vec<ComponentActor>,
        pub handled_kinds: Vec<aether_data::KindId>,
        pub fallback: bool,
        pub provenance: String,
    }

    /// One exported actor type within a (possibly multi-actor) component
    /// module (ADR-0096, issue 1956). `namespace` is the type's
    /// `Addressable::NAMESPACE` — the `@actor` half of a `module@actor` selector;
    /// `handled_kinds` is the kind ids this actor handles; `fallback` is
    /// whether it declares a `#[fallback]`. A single-actor module's
    /// implicit group reports `namespace` as the module's `aether.namespace`
    /// section value.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct ComponentActor {
        pub namespace: String,
        pub handled_kinds: Vec<aether_data::KindId>,
        pub fallback: bool,
    }

    /// One stored component in a [`ListComponentBinariesResult`] (ADR-0116,
    /// issue 1956). `hash` is the sha256 hex over the wasm's raw bytes — the
    /// content-address key. `name` is the optional human-readable name the
    /// latest upload pointed at this hash (`Addressable::NAMESPACE` is the
    /// natural one). `manifest` is what the wasm self-reported.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct ComponentEntry {
        pub hash: String,
        pub name: Option<String>,
        pub manifest: ComponentManifest,
    }

    /// How a [`ResolveComponent`] names the component wasm to load — a
    /// registry selector resolved against the hub's content-addressed store
    /// (ADR-0116), never a host filesystem path (the path is retired from
    /// `load_component` entirely). The engines cap resolves it in this
    /// order:
    ///
    /// - `query` is the exact selector token: a sha256 content `hash`, a
    ///   `name@version` (treated as `name` latest in v1 — no per-name
    ///   version index yet, ADR-0116), or a `name` an upload pointed at a
    ///   hash. `module@actor` resolves the `module` part as the
    ///   `name`/`hash` and the `@actor` part as the [`ResolveComponentResult`]
    ///   `export` (the actor `Addressable::NAMESPACE` to instantiate, ADR-0096).
    /// - `namespace` / `handled_kind` are an attribute query over the
    ///   type-tagged component manifests, consulted when `query` is `None`:
    ///   keep only components exporting that `namespace`, or handling that
    ///   `KindId`. An attribute query that matches more than one component
    ///   is a clean ambiguity error, not a silent pick.
    ///
    /// An exact `query` wins first; absent one, the attribute query
    /// resolves. A selector that resolves to no stored component fails.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
    pub struct ComponentSelector {
        pub query: Option<String>,
        pub namespace: Option<String>,
        pub handled_kind: Option<aether_data::KindId>,
    }

    /// `aether.engine.upload_component` — ingest a component wasm into the
    /// hub's content-addressed store (ADR-0116, issue 1956). `staged_path`
    /// is an absolute host path the hub reads itself (aether-mcp never reads
    /// the bytes — too large for the tool channel); the cap sha256-hashes
    /// the bytes, dedups against the existing store, reads the manifest
    /// straight from the wasm (no execution step — `aether.kinds.inputs` +
    /// `aether.namespace` + the `producers` section), and stores both.
    /// `name`, when set, points that human-readable name at the resulting
    /// hash. Reply: [`UploadComponentResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.upload_component")]
    pub struct UploadComponent {
        pub staged_path: String,
        pub name: Option<String>,
    }

    /// Reply to [`UploadComponent`] (ADR-0116, issue 1956). `Ok` carries the
    /// content-address `hash` the bytes stored under (a re-upload of
    /// identical bytes returns the same hash) and the `name` now pointing
    /// at it, if any. `Err` carries a free-form reason — an unreadable
    /// `staged_path` or a wasm whose manifest can't be parsed.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.upload_component_result")]
    pub enum UploadComponentResult {
        Ok { hash: String, name: Option<String> },
        Err { error: String },
    }

    /// `aether.engine.resolve_component` — resolve a [`ComponentSelector`]
    /// to a stored component's wasm bytes + manifest (ADR-0116, issue
    /// 1956). aether-mcp calls this hub-local before forwarding a
    /// `LoadComponent` to the target substrate, so the resolve hop keeps the
    /// load seam path-free. Reply: [`ResolveComponentResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.resolve_component")]
    pub struct ResolveComponent {
        pub selector: ComponentSelector,
    }

    /// Reply to [`ResolveComponent`] (ADR-0116, issue 1956). `Ok` carries
    /// the resolved content `hash`, the `wasm` bytes the load forwards, the
    /// `name` pointing at the hash (if any), the `manifest` the store read
    /// from the wasm, and `export` — the `@actor` half of a `module@actor`
    /// selector, threaded into the forwarded `LoadComponent.export` so a
    /// specific actor type is instantiated from a multi-actor module
    /// (ADR-0096); `None` for a plain selector. `Err` carries a free-form
    /// reason — a selector that resolves to no stored component, or an
    /// attribute query matching more than one (a clean ambiguity error).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.resolve_component_result")]
    pub enum ResolveComponentResult {
        Ok {
            hash: String,
            wasm: Vec<u8>,
            name: Option<String>,
            manifest: ComponentManifest,
            export: Option<String>,
        },
        Err {
            error: String,
        },
    }

    /// `aether.engine.list_components` — enumerate the hub's stored
    /// components (ADR-0116, issue 1956). The filter fields are
    /// AND-combined and each defaults to "no constraint": `namespace` keeps
    /// only entries exporting that actor namespace, `handled_kind` keeps
    /// only entries handling that `KindId`. Reply:
    /// [`ListComponentBinariesResult`].
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.engine.list_components")]
    pub struct ListComponentBinaries {
        pub namespace: Option<String>,
        pub handled_kind: Option<aether_data::KindId>,
    }

    /// Reply to [`ListComponentBinaries`] (ADR-0116, issue 1956): every
    /// stored component matching the filter, each as a [`ComponentEntry`]
    /// carrying its hash, optional name, and the manifest read from the wasm.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.list_components_result")]
    pub struct ListComponentBinariesResult {
        pub components: Vec<ComponentEntry>,
    }
}

mod control_plane {
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;

    use serde::{Deserialize, Serialize};

    /// `aether.component.load` — request the substrate load a WASM
    /// component into a freshly allocated mailbox. Carries the raw
    /// WASM bytes and an optional human-readable name. The
    /// component's kind vocabulary ships embedded in the wasm's
    /// `aether.kinds` custom section (ADR-0028) — the substrate
    /// reads it directly and the loader doesn't need to declare
    /// anything. Substrate replies with `LoadResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.load")]
    pub struct LoadComponent {
        pub wasm: Vec<u8>,
        pub name: Option<String>,
        /// ADR-0090 (issue 1257): optional init-config bytes handed to
        /// the guest's typed `WasmActor::init` at instantiate-time. An
        /// empty vec means "no config" — the c1 ABI short-circuits it
        /// to `&[]`, which a `Config = ()` guest decodes uniformly via
        /// `impl Kind for ()`. The carrier is raw bytes, not a typed
        /// kind, so the substrate stays byte-transparent: the hub /
        /// MCP encode the config struct to bytes at the edge
        /// (SDK-typed, not wire-typed), matching `wasm`'s `Vec<u8>`.
        pub config: Vec<u8>,
        /// ADR-0096: which exported actor type to instantiate from a
        /// multi-actor module, named by its `Addressable::NAMESPACE`. `None`
        /// loads the **entry** type (the first in the module's
        /// `export!` list), which is also the only type a single-actor
        /// module has — so an unset selector preserves the pre-ADR-0096
        /// load. An export that the module doesn't declare is a clean
        /// `LoadResult::Err`.
        pub export: Option<String>,
    }

    /// Reply to `LoadComponent`. `Ok` carries the assigned mailbox id,
    /// the resolved name (so callers that omitted `name` learn the
    /// substrate-defaulted one), and the component's advertised
    /// receive-side capabilities parsed from `aether.kinds.inputs`
    /// (ADR-0033). `Err` carries the failure reason — kind-descriptor
    /// conflict, invalid WASM, name conflict, etc.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.load_result")]
    pub enum LoadResult {
        Ok {
            mailbox_id: aether_data::MailboxId,
            name: String,
            capabilities: ComponentCapabilities,
        },
        Err {
            error: String,
        },
    }

    /// ADR-0033 receive-side capability surface for a component. Built
    /// from the `aether.kinds.inputs` wasm custom section at load time;
    /// the substrate extracts the structured handler / fallback /
    /// component-doc records from the raw section bytes and packs them
    /// into this shape so the hub can store and the MCP harness can
    /// render without a second parser. Empty `handlers` + `None`
    /// fallback + `None` doc describes a component that shipped
    /// without the `#[actor]` macro (ADR-0027 shape) — the hub can
    /// tell those apart from a truly empty receive surface.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
    pub struct ComponentCapabilities {
        pub handlers: Vec<HandlerCapability>,
        pub fallback: Option<FallbackCapability>,
        pub doc: Option<String>,
        /// ADR-0090 (issue 1257): the kind the component expects as its
        /// boot config, when it declared a `type Config` other than the
        /// synthesized `()`. `None` for a no-config component. The
        /// capability carries the config kind's id + name; its full
        /// schema is reachable through the engine registry /
        /// `describe_kinds` because the `#[actor]` macro emits a
        /// retention static for the config kind on load, exactly as for
        /// handler kinds.
        pub config: Option<ConfigCapability>,
    }

    /// One `#[handler]` method's advertised capability. `id` is the
    /// compile-time `<K as Kind>::ID` (ADR-0030); `name` is `K::NAME`;
    /// `doc` carries the author's rustdoc filtered through the
    /// `# Agent` section convention when present, else the full doc.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct HandlerCapability {
        pub id: aether_data::KindId,
        pub name: String,
        pub doc: Option<String>,
        /// ADR-0112: the handler's reply class — `None` / `One(R)` for a
        /// single-class handler (the ADR-0109 return-type contract),
        /// `Manual` for a manual-class handler that replies by hand,
        /// `Stream(R)` reserved. Lets `describe_component` report the real
        /// `In -> Out` so a caller reads what a call returns before issuing
        /// it. Native chassis caps report `None` until the native handler
        /// manifest lands (ADR-0109 §5, a follow-on).
        pub reply: aether_data::ReplyContract,
    }

    /// A `#[fallback]` method's advertised presence + optional doc.
    /// Components without a fallback are strict receivers; absence of
    /// this field on `ComponentCapabilities` means "no catchall — mail
    /// for unhandled kinds will land as `DISPATCH_UNKNOWN_KIND`".
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct FallbackCapability {
        pub doc: Option<String>,
    }

    /// ADR-0090 (issue 1257) the component's declared boot-config kind.
    /// `id` is the compile-time `<C::Config as Kind>::ID`; `name` is
    /// `C::Config::NAME`. Present only when the component declared a
    /// `type Config` other than the synthesized `()` — a no-config
    /// component leaves `ComponentCapabilities.config` `None`. The
    /// kind's full schema rides the `aether.kinds` section (the macro's
    /// retention static), so `describe_kinds` resolves it by id.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct ConfigCapability {
        pub id: aether_data::KindId,
        pub name: String,
    }

    /// `aether.component.drop` — remove a component from the
    /// substrate and invalidate its mailbox id. Reply: `DropResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.drop")]
    pub struct DropComponent {
        pub mailbox_id: aether_data::MailboxId,
    }

    /// Reply to `DropComponent`. `Ok` on success; `Err` if the
    /// mailbox was unknown, wasn't a component, or already dropped.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.drop_result")]
    pub enum DropResult {
        Ok,
        Err { error: String },
    }

    /// `aether.component.replace` — atomically rebind a target
    /// mailbox id to a freshly instantiated component. ADR-0022: the
    /// substrate freezes the target, drains in-flight mail through
    /// the old instance, then swaps. If the drain exceeds
    /// `drain_timeout_ms` (default 5000) the replace fails with
    /// `ReplaceResult::Err` and the old instance stays bound. Kind
    /// vocabulary rides in the wasm's `aether.kinds` custom section
    /// (ADR-0028). Reply: `ReplaceResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.replace")]
    pub struct ReplaceComponent {
        pub mailbox_id: aether_data::MailboxId,
        pub wasm: Vec<u8>,
        pub drain_timeout_ms: Option<u32>,
        /// ADR-0090 (issue 1257): optional init-config bytes for the
        /// replacement instance, threaded through to its typed `init`
        /// the same way [`LoadComponent::config`] is on first load. An
        /// empty vec means "no config".
        pub config: Vec<u8>,
        /// ADR-0096: which exported actor type to instantiate from the
        /// replacement module, named by its `Addressable::NAMESPACE`. `None`
        /// reuses the trampoline's **current hosted type** (not
        /// necessarily the entry), so a bare replace preserves
        /// today's behaviour byte-for-byte. `Some(ns)` instantiates the
        /// named export — mirroring [`LoadComponent::export`] — and an
        /// export the replacement module doesn't declare is a clean
        /// `ReplaceResult::Err`.
        pub export: Option<String>,
    }

    /// Reply to `ReplaceComponent`. Carries the new component's
    /// advertised capabilities on `Ok` so the hub's cached state
    /// reflects the swapped binary; `Err` carries a free-form reason.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.replace_result")]
    pub enum ReplaceResult {
        Ok { capabilities: ComponentCapabilities },
        Err { error: String },
    }

    /// `aether.component.list` — enumerate the components an engine has
    /// actually loaded and registered, addressed to its `aether.component`
    /// mailbox (issue 2020). Fieldless: the query is a definitive snapshot
    /// of the live trampoline set, the only part of the registry whose
    /// membership varies (chassis caps are boot-present and static). A
    /// consumer that spawned an engine with a boot-manifest autoload
    /// (ADR-0116) polls this to learn deterministically when a requested
    /// component is loaded and registered at its lineage address, instead
    /// of inferring liveness by proxy. Reply: `ListComponentsResult`.
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.component.list")]
    pub struct ListComponents {}

    /// Reply to `ListComponents` (issue 2020): the ADR-0099 lineage name of
    /// every currently-loaded component (each registered at
    /// `aether.component/<name>`). `names` only — no mailbox id: the id is a
    /// deterministic hash-chain over the lineage the `name` already renders
    /// (ADR-0099), and routing is the substrate's job (a caller addresses by
    /// `recipient_name` and the substrate resolves it), so the handle has no
    /// use at the caller.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.list_result")]
    pub struct ListComponentsResult {
        pub names: Vec<String>,
    }

    /// `aether.component.describe` — introspect one loaded component's
    /// ADR-0033 receive-side `ComponentCapabilities` (handler kinds, docs,
    /// fallback, config kind), addressed to its `aether.component` mailbox
    /// by lineage `name` (the `aether.component/<name>` address that
    /// `ListComponents` / `LoadResult.name` hand back; iamacoffeepot/aether#2421).
    /// Name-addressed because a boot-manifest-loaded component never returns
    /// a mailbox id to its spawner — the substrate is the only process that
    /// always holds the live loaded set, so it owns the answer. Reply:
    /// `DescribeComponentResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.describe")]
    pub struct DescribeComponent {
        /// The component's ADR-0099 lineage name (e.g.
        /// `aether.embedded:aether.camera`), as returned by
        /// `ListComponentsResult.names` or `LoadResult.name`.
        pub name: String,
    }

    /// Reply to `DescribeComponent` (iamacoffeepot/aether#2421): the full
    /// `ComponentCapabilities` on `Ok`, or a free-form reason on `Err` (no
    /// component registered at that lineage name).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.describe_result")]
    pub enum DescribeComponentResult {
        Ok { capabilities: ComponentCapabilities },
        Err { error: String },
    }

    /// Reference-image comparison for a `CaptureFrame.similarity` request
    /// (iamacoffeepot/aether#1780). The capture handler reads the PNG at
    /// `reference_path` from the `namespace` assets directory before
    /// dispatching to the render thread; the render thread scores the
    /// captured RGBA against the decoded reference and returns
    /// `similarity_score` + `similarity_pass` on
    /// `CaptureFrameResult::Ok`. Only the `"assets"` namespace is
    /// supported in v1. `threshold` is the maximum normalised MAE
    /// `[0.0, 1.0]` that still counts as a match (`0` = identical only;
    /// `1` = any frame passes); `similarity_pass` is `true` when
    /// `similarity_score <= threshold`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct SimilarityCheck {
        pub namespace: String,
        pub reference_path: String,
        /// Maximum normalised MAE `[0.0, 1.0]` that counts as a match.
        pub threshold: f32,
    }

    /// `aether.render.capture_frame` — request the substrate grab the
    /// current frame contents and reply-to-sender with an encoded
    /// PNG. Carries two optional bundles: `mails` dispatched *before*
    /// capturing (state-changing mail whose effects should appear in
    /// the frame) and `after_mails` dispatched *after* the readback
    /// completes (cleanup, e.g. restoring a flag the caller flipped
    /// for the capture). Both bundles plus the capture land in one
    /// atomic tool call. The render thread's existing mail-drain
    /// barrier before the capture ensures every `mails` entry has
    /// been fully processed by the time the frame is read back.
    /// Empty vecs mean "just capture the current state" /
    /// "no cleanup".
    ///
    /// Abort-on-first-failure policy: if *any* envelope in *either*
    /// bundle fails to resolve (unknown kind or recipient), no mail
    /// is dispatched and the reply is `CaptureFrameResult::Err`. The
    /// whole request aborts before touching the queue.
    ///
    /// `checks` requests a substrate-side verdict scored on the exact
    /// RGBA the PNG is built from — the de-padded, swizzled frame the
    /// render thread maps before the PNG encode (ADR-0105 capture path,
    /// iamacoffeepot/aether#1777). Each entry names one
    /// `test_bench::visual` reduction plus its lit/background partition
    /// params; the results ride back on `CaptureFrameResult::Ok.verdict`.
    /// Empty means "PNG only, no verdict" — the prior behaviour.
    ///
    /// `similarity` requests a reference-image MAE comparison scored on
    /// the same raw RGBA (iamacoffeepot/aether#1780). The handler reads
    /// the reference PNG from the assets namespace before dispatching to
    /// the render thread; the render thread runs the comparison and
    /// returns `similarity_score` / `similarity_pass` on
    /// `CaptureFrameResult::Ok`. `None` means "no similarity check".
    ///
    /// Reply: `CaptureFrameResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.capture_frame")]
    pub struct CaptureFrame {
        pub mails: Vec<NamedMail>,
        pub after_mails: Vec<NamedMail>,
        pub checks: Vec<FrameCheck>,
        /// Optional reference-image similarity check
        /// (iamacoffeepot/aether#1780). `None` means no comparison.
        pub similarity: Option<SimilarityCheck>,
    }

    /// One mail in a `CaptureFrame.mails` bundle. Structurally mirrors
    /// `aether_data::MailFrame` — a pre-encoded payload plus
    /// the name-level addressing the substrate uses to resolve it.
    /// The hub encodes each entry's `payload` via the kind's
    /// descriptor before wrapping it into the bundle, so the
    /// substrate side just pushes `Mail::new(mailbox, kind_id,
    /// payload, count)` directly.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct NamedMail {
        pub recipient_name: String,
        pub kind_name: String,
        pub payload: Vec<u8>,
        pub count: u32,
    }

    /// Reply to `CaptureFrame`. `Ok` carries the PNG bytes for the
    /// captured frame plus an optional [`FrameVerdict`] (present iff the
    /// request carried `checks`) and an optional similarity score (present
    /// iff the request carried `similarity`); `Err` carries a free-form
    /// reason — capture not supported on this surface, map failed, encode
    /// failed, reference image not found / undecodable, or a
    /// bundle-resolution failure aborting before any mail was dispatched.
    ///
    /// `similarity_score` is the normalised MAE in `[0.0, 1.0]`
    /// (0 = identical, 1 = maximally different).
    /// `similarity_pass` is `true` when `similarity_score <=
    /// SimilarityCheck.threshold` (iamacoffeepot/aether#1780).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.capture_frame_result")]
    pub enum CaptureFrameResult {
        Ok {
            png: Vec<u8>,
            verdict: Option<FrameVerdict>,
            /// Normalised MAE score `[0.0, 1.0]`; `None` when no
            /// `similarity` was requested.
            similarity_score: Option<f32>,
            /// `true` when `similarity_score <= threshold`; `None` when
            /// no `similarity` was requested.
            similarity_pass: Option<bool>,
        },
        Err {
            error: String,
        },
    }

    /// Build a [`CaptureFrameResult`] from the raw GPU `render_and_capture`
    /// result shape. Every capture handler in `aether-substrate-bundle`
    /// (test-bench inline, in-process bench, desktop driver) needs this
    /// same `Ok((png, verdict, score, pass)) → Ok { … }` /
    /// `Err(error) → Err { error }` flip. `verdict` is `None` when the
    /// request carried no `checks`; `similarity_score` / `similarity_pass`
    /// are `None` when no `similarity` was requested
    /// (iamacoffeepot/aether#1780).
    impl From<Result<(Vec<u8>, Option<FrameVerdict>, Option<f32>, Option<bool>), String>>
        for CaptureFrameResult
    {
        fn from(
            result: Result<(Vec<u8>, Option<FrameVerdict>, Option<f32>, Option<bool>), String>,
        ) -> Self {
            match result {
                Ok((png, verdict, similarity_score, similarity_pass)) => Self::Ok {
                    png,
                    verdict,
                    similarity_score,
                    similarity_pass,
                },
                Err(error) => Self::Err { error },
            }
        }
    }

    /// One reduction requested in a [`CaptureFrame::checks`] list. The
    /// `reduction` names which `test_bench::visual` check to run;
    /// `tolerance` is the per-channel threshold that partitions pixels
    /// into the lit/background mask the silhouette reductions share; and
    /// `background` pins the reference RGB — `None` falls back to the
    /// frame's top-left pixel, the `differs_from_background` convention.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct FrameCheck {
        pub reduction: FrameReduction,
        pub tolerance: u8,
        pub background: Option<[u8; 3]>,
    }

    /// Which `test_bench::visual` reduction a [`FrameCheck`] runs. The
    /// names mirror the public reduction functions one-for-one.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FrameReduction {
        /// `not_all_black` — at least one pixel has a non-zero RGB.
        NotAllBlack,
        /// `differs_from_background` — at least one pixel exceeds the
        /// tolerance band around the background reference.
        DiffersFromBackground,
        /// `coverage` — lit fraction of the frame in `[0.0, 1.0]`.
        Coverage,
        /// `centroid` — mean lit-pixel `(x, y)`.
        Centroid,
        /// `bounding_box` — inclusive lit-pixel extent.
        BoundingBox,
    }

    /// Substrate-side verdict over a captured frame: the frame
    /// dimensions plus one [`FrameCheckResult`] per requested reduction,
    /// scored on the exact de-padded RGBA the PNG was encoded from
    /// (iamacoffeepot/aether#1777). Rides on `CaptureFrameResult::Ok`
    /// when the request carried `checks`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct FrameVerdict {
        pub width: u32,
        pub height: u32,
        pub results: Vec<FrameCheckResult>,
    }

    /// Result of one requested reduction. The variant matches the
    /// [`FrameReduction`] requested; the assertion-style checks
    /// (`NotAllBlack` / `DiffersFromBackground`) report `passed` plus a
    /// `detail` failure string (`None` on pass), and the silhouette
    /// reductions echo the `background` they partitioned against
    /// alongside their scalar / coordinate result (`None` when the lit
    /// mask was empty).
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub enum FrameCheckResult {
        NotAllBlack {
            passed: bool,
            detail: Option<String>,
        },
        DiffersFromBackground {
            passed: bool,
            detail: Option<String>,
        },
        Coverage {
            background: [u8; 3],
            fraction: f32,
        },
        Centroid {
            background: [u8; 3],
            centroid: Option<[f32; 2]>,
        },
        BoundingBox {
            background: [u8; 3],
            rect: Option<FrameRect>,
        },
    }

    /// Inclusive axis-aligned pixel extent of a lit region — the wire
    /// mirror of `test_bench::visual::Rect`. `min`/`max` are the smallest
    /// and largest lit column (`x`) and row (`y`); a single lit pixel
    /// yields `min == max`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FrameRect {
        pub min_x: u32,
        pub min_y: u32,
        pub max_x: u32,
        pub max_y: u32,
    }

    // ADR-0105 textured-quad render surface. The texture + quad draw
    // kinds (`CreateTexture` / `CreateTextureResult` / `UpdateTexture` /
    // `TexturedQuad` / `DrawTexturedQuads` / `SolidQuad` /
    // `DrawSolidQuads`) moved to `aether_capabilities::render::kinds`
    // (ADR-0121). The `QuadScale` / `QuadSpace` projection types stay
    // central: the `aether.text.draw` kind below consumes `QuadSpace`,
    // and `aether-kinds` has no dependency on `aether-capabilities`, so
    // moving them would close a cycle — they're sibling-kind-consumed and
    // therefore pinned here.

    /// How a `QuadSpace::World` quad's clip-space scale factor `k`
    /// relates on-screen size to distance (ADR-0105).
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub enum QuadScale {
        /// `k` is a constant derived from `reference_distance`, so the
        /// perspective divide shrinks the quad as the anchor recedes; the
        /// quad's pixel size holds exactly at `reference_distance`. The
        /// above-the-head label mode.
        Distance { reference_distance: f32 },
        /// `k = clip.w`, cancelling the perspective divide for constant
        /// on-screen size regardless of distance.
        Pixels,
    }

    /// Projection a `DrawTexturedQuads` batch draws under (ADR-0105).
    ///
    /// `Screen` quads are window-pixel rects drawn in an overlay pass
    /// after the world pass under an ortho matrix derived from the surface
    /// size, no depth. `World` quads transform only `anchor` through the
    /// camera's `view_proj`, then apply each quad's pixel offsets in clip
    /// space, so the quad faces the camera and never skews; `scale` picks
    /// the distance-vs-size relationship.
    ///
    /// The render cap implements `Screen`; `World` ships in the vocabulary
    /// now but warn-drops at encode until the world-anchor path lands.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub enum QuadSpace {
        Screen,
        World { anchor: [f32; 3], scale: QuadScale },
    }

    // ADR-0105 text surface. The `aether.text` capability composes the
    // textured-quad surface above into glyphs: load a TTF off the hot
    // path under a session-scoped `font_id`, then draw a string every
    // frame in immediate mode. Structured-shaped; `space` reuses
    // `QuadSpace` so a screen-space HUD string and a world-anchored
    // label ride the same discriminant.

    /// One glyph's horizontal advance, in font units (em-square
    /// subdivisions), keyed by the Unicode scalar value (`char as u32`)
    /// it maps to through the font's cmap. Scale to pixels with
    /// `advance_units * size_pixels / units_per_em`. Not a kind on its
    /// own — only addressable inside `FontMetrics.advances`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct GlyphAdvance {
        /// Unicode scalar value (`char as u32`) this advance applies to.
        pub codepoint: u32,
        /// Horizontal advance in font units.
        pub advance_units: f32,
    }

    /// Size-independent metrics for one loaded font (ADR-0105). Every
    /// measure is in font units — the em square's `units_per_em`
    /// subdivisions — so a consumer caches this table once and scales any
    /// measure to a draw size locally with
    /// `value * size_pixels / units_per_em`, the exact linear scaling the
    /// `aether.text` cap applies as it lays a string out. The
    /// per-codepoint `advances` fold the cmap in; a codepoint the font
    /// has no glyph for advances by `default_advance` (the `.notdef`
    /// glyph's advance), matching the draw path. Carried in
    /// `FontMetricsResult::Ok`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct FontMetrics {
        /// Em-square subdivisions — the denominator that turns a
        /// font-unit measure into a fraction of `size_pixels`.
        pub units_per_em: f32,
        /// Highest point glyphs reach above the baseline, in font units.
        pub ascent: f32,
        /// Lowest point glyphs reach below the baseline, in font units
        /// (typically negative).
        pub descent: f32,
        /// Recommended gap between one line's descent and the next line's
        /// ascent, in font units.
        pub line_gap: f32,
        /// Advance for a codepoint the font has no glyph for — the
        /// `.notdef` glyph's advance, in font units.
        pub default_advance: f32,
        /// Per-codepoint horizontal advances in font units, sorted by
        /// `codepoint`, the cmap folded in.
        pub advances: Vec<GlyphAdvance>,
    }

    /// The three window presentation modes. `Windowed` has no fields —
    /// the current size lives on `SetWindowModeResult`.
    /// `FullscreenExclusive` carries the specific video mode; the
    /// substrate matches against the active monitor's supported modes
    /// and fails the request if none matches (loud rather than
    /// silently falling back).
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum WindowMode {
        Windowed,
        FullscreenBorderless,
        FullscreenExclusive {
            width: u32,
            height: u32,
            refresh_mhz: u32,
        },
    }

    /// `aether.window.set_mode` — switch the substrate's
    /// window presentation mode. `width` / `height` apply only when
    /// `mode == Windowed`; fullscreen modes size themselves from the
    /// monitor / requested video mode. Reply carries the new state
    /// so callers don't have to follow up with a `platform_info`
    /// query.
    ///
    /// Fullscreen-exclusive requests fail with `Err` if no
    /// `VideoMode` on the current monitor matches the `(width,
    /// height, refresh_mhz)` triple exactly. Use `platform_info`
    /// first to enumerate supported modes.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.set_mode")]
    pub struct SetWindowMode {
        pub mode: WindowMode,
        pub width: Option<u32>,
        pub height: Option<u32>,
    }

    /// Reply to `SetWindowMode`. `Ok` carries the resolved state
    /// after the mode change applied; `Err` carries the reason the
    /// request was rejected (unknown video mode, window not ready,
    /// etc.) with no state change.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.set_mode_result")]
    pub enum SetWindowModeResult {
        Ok {
            mode: WindowMode,
            width: u32,
            height: u32,
        },
        Err {
            error: String,
        },
    }

    /// `aether.window.set_title` — update the substrate
    /// window's title at runtime. `winit::Window::set_title` is
    /// infallible on every supported platform, so the desktop reply
    /// always echoes the applied title back on `Ok`. Headless and hub
    /// chassis reply `Err { error: "unsupported on headless..." }`.
    /// Boot-time default comes from `AETHER_WINDOW_TITLE`; unset falls
    /// back to the substrate's name.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.set_title")]
    pub struct SetWindowTitle {
        pub title: String,
    }

    /// Reply to `SetWindowTitle`. `Ok` echoes the applied title — same
    /// value the caller sent, returned so MCP logs and agent memory
    /// see the resulting state in one place. `Err` is reserved for
    /// chassis that don't own a window (headless, hub) or for a
    /// pre-window-ready request.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.set_title_result")]
    pub enum SetWindowTitleResult {
        Ok { title: String },
        Err { error: String },
    }

    /// `aether.window.focus` — bring the substrate window to the
    /// foreground (un-minimize, show if hidden, raise + focus). Takes
    /// no fields: focus is a single imperative with no parameters.
    ///
    /// Motivating use (iamacoffeepot/aether#1318): an MCP-driven
    /// session that wants to `capture_frame` against a backgrounded /
    /// minimized / hidden window has no programmatic lever to raise it
    /// otherwise. The desktop driver applies `set_minimized(false)` +
    /// `set_visible(true)` + `focus_window()`. Headless / hub chassis
    /// reply `Err` (no window peripheral).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.focus")]
    pub struct FocusWindow {}

    /// Reply to `FocusWindow`. `Ok` confirms the window was raised
    /// (winit's `focus_window` is best-effort per the platform docs,
    /// but the substrate has applied the three calls). `Err` carries
    /// the reason — a pre-window-ready request, or a chassis without a
    /// window peripheral (headless, hub).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.focus_result")]
    pub enum FocusWindowResult {
        Ok,
        Err { error: String },
    }

    // ADR-0088 §6 reverse-lookup inventory actor. The `aether.inventory`
    // mailbox serves the per-build reverse-lookup inventory over mail so
    // an out-of-process observer (the MCP harness) reads the running
    // substrate's *own* inventory instead of a drift-prone compiled-in
    // copy. Two request kinds:
    //
    //   - `aether.inventory.manifest` → the compile-time manifest: every
    //     declared `NameEntry` + every instanced-family `TemplateEntry`.
    //     Templates keep their *family shape* (the client expands a
    //     `Bounded` range / `Declared` domain itself); the manifest does
    //     NOT flatten to a hash → name map (ADR-0088 §6).
    //   - `aether.inventory.resolve { ids }` → per-id `Option<String>`,
    //     the dynamic-instance arm of the resolve chain (ADR-0088 §5) the
    //     client can't compute from the manifest alone.
    //
    // The link-time `aether_data::name_inventory::{NameEntry,
    // TemplateEntry, ParamKind}` are `&'static` (not wire types), so the
    // shapes below are owned, schema-hashed mirrors. `domain` rides as
    // raw bytes (the byte-domain prefix an id is hashed under, e.g.
    // `MAILBOX_DOMAIN` / `THREAD_DOMAIN`) so the client recomputes hashes
    // exactly without depending on the substrate's domain consts.

    /// How a [`TemplateEntryWire`]'s single `{…}` hole is filled — the
    /// wire mirror of `aether_data::name_inventory::ParamKind` (ADR-0088
    /// §4). The variants preserve the family shape so the client can
    /// expand / prehash a `Bounded` range or `Declared` domain locally
    /// the same way the substrate's static reverse map does at boot.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.inventory.param_kind")]
    pub enum ParamKindWire {
        /// Finite inclusive integer range (`aether-worker-{0..=255}`).
        /// The client enumerates `lo..=hi`, substitutes each value into
        /// the template, and hashes the result for an exact reverse.
        Bounded { lo: u64, hi: u64 },
        /// The hole ranges over every [`NameEntryWire`] whose `domain`
        /// equals `domain` (`aether-root-{NAMESPACE}` over the declared
        /// mailbox namespaces).
        Declared { domain: Vec<u8> },
        /// Instances are minted at runtime from an unbounded parameter
        /// (`aether-instanced-{full_name}`). The template declares only
        /// the family's existence + shape; individual instances reverse
        /// via `aether.inventory.resolve`, not local expansion.
        Dynamic,
    }

    /// A declared name on the wire — the mirror of
    /// `aether_data::name_inventory::NameEntry` (ADR-0088 §3). `domain`
    /// is the byte-domain prefix the name is hashed under; `name` is the
    /// declared name (`"aether.fs"`). The client rehashes `name` under
    /// `domain` to recover the id space exactly.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct NameEntryWire {
        pub domain: Vec<u8>,
        pub name: String,
    }

    /// A name template for an instanced family on the wire — the mirror
    /// of `aether_data::name_inventory::TemplateEntry` (ADR-0088 §4).
    /// `template` carries one `{…}` hole; [`ParamKindWire`] (the shape
    /// axis) says how it is filled. Preserving the template (rather than
    /// its expansion) keeps the family shape so the client can declare
    /// "ids in this family exist and look like *this*" even for `Dynamic`
    /// families it cannot enumerate.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct TemplateEntryWire {
        pub domain: Vec<u8>,
        pub template: String,
        pub param: ParamKindWire,
    }

    /// `aether.inventory.manifest` — request the running substrate's
    /// compile-time reverse-lookup manifest (ADR-0088 §6). Empty payload;
    /// the request *is* the signal. Mailed to the `"aether.inventory"`
    /// mailbox; reply: [`ManifestResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.inventory.manifest")]
    pub struct Manifest {}

    /// Reply to [`Manifest`] (ADR-0088 §6). Carries every link-time
    /// [`NameEntryWire`] (declared names: chassis mailbox namespaces +
    /// kinds + transforms) and every [`TemplateEntryWire`] (instanced
    /// families, `Bounded`/`Declared`/`Dynamic`). The client folds
    /// `names` into a hash → name map and expands `Bounded`/`Declared`
    /// templates locally; `Dynamic` templates resolve per-id via
    /// [`Resolve`]. This is the *authoritative, per-build* inventory —
    /// the served form is always the running substrate's own.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.inventory.manifest_result")]
    pub struct ManifestResult {
        pub names: Vec<NameEntryWire>,
        pub templates: Vec<TemplateEntryWire>,
    }

    /// `aether.inventory.resolve` — request per-id reverse lookup
    /// (ADR-0088 §5/§6). `ids` are ADR-0064 tagged-id strings
    /// (`mbx-…` / `knd-…` / `thr-…` / `trn-…`) — the same wire form the
    /// MCP surface carries elsewhere. Used on a *local miss*: the client
    /// resolves statics + expandable templates from the manifest itself,
    /// then asks the substrate only for dynamic-instance ids it can't
    /// compute. Mailed to the `"aether.inventory"` mailbox; reply:
    /// [`ResolveResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.inventory.resolve")]
    pub struct Resolve {
        pub ids: Vec<String>,
    }

    /// One id → name pairing in a [`ResolveResult`] (ADR-0088 §6). `id`
    /// echoes the request's tagged-id string so the caller correlates
    /// without relying on positional order; `name` is the resolved origin
    /// name, or `None` on a full miss (the id wasn't in the static map,
    /// any prehashed template, or the runtime registry — the caller falls
    /// back to rendering the tagged-id string per ADR-0064, exactly what
    /// it showed before the inventory existed). Per the explicit-nulls
    /// convention every entry addresses its `name` Option directly.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct ResolvedName {
        pub id: String,
        pub name: Option<String>,
    }

    /// Reply to [`Resolve`] (ADR-0088 §6). One [`ResolvedName`] per
    /// requested id, in request order (and each echoing its `id` so the
    /// caller can correlate without depending on order). An id that fails
    /// to parse as a tagged-id string is reported as `name: None` rather
    /// than aborting the batch — one bad id doesn't sink its siblings.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.inventory.resolve_result")]
    pub struct ResolveResult {
        pub resolved: Vec<ResolvedName>,
    }

    /// One kind in a [`ListKindsResult`] (ADR-0091). `id` is the
    /// substrate's authoritative [`KindId`](aether_data::KindId) for the
    /// kind; `name` is its declared `Kind::NAME`; `schema_wire` is
    /// the kind's [`SchemaType`](aether_data::SchemaType) encoded with
    /// the wire format (the wire enum carries the full nominal shape).
    ///
    /// The schema rides as opaque wire bytes rather than a directly
    /// embedded `SchemaType` because `SchemaType` itself has no
    /// `Schema` impl (it *is* the schema vocabulary, not a value in
    /// it); shipping it as `Bytes` keeps `KindDescriptorWire` and the
    /// whole reply derivable via [`aether_data::Schema`] without a
    /// hand-roll, at the cost of one extra `wire::from_bytes` on
    /// the harness side. Cap encodes via `wire::to_vec`
    /// against `descriptor.schema`; client decodes via
    /// `wire::from_bytes`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct KindDescriptorWire {
        pub id: aether_data::KindId,
        pub name: String,
        pub schema_wire: Vec<u8>,
    }

    /// `aether.inventory.kinds` — request the running substrate's
    /// authoritative kind vocabulary (ADR-0091): every
    /// [`KindId`](aether_data::KindId) the engine's `Registry`
    /// currently holds, with its full
    /// [`SchemaType`](aether_data::SchemaType). Empty payload; the
    /// request *is* the signal. Mailed to the `"aether.inventory"`
    /// mailbox; reply: [`ListKindsResult`].
    ///
    /// The MCP harness uses this to refresh its per-engine encode-
    /// cache after a `load_component` registers a component's own
    /// kinds — the substrate's `Registry` is the single source of
    /// truth, projected onto the wire by the inventory cap.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.inventory.kinds")]
    pub struct ListKinds {}

    /// Reply to [`ListKinds`] (ADR-0091). One [`KindDescriptorWire`] per
    /// kind currently registered in the substrate's `Registry`, sorted
    /// by name (the registry's `list_kind_descriptors` ordering). The
    /// harness folds this into its per-engine encode cache; component-
    /// defined kinds (loaded via `aether.component.load`) show up here
    /// alongside the substrate's static vocabulary the moment the load
    /// returns, no separate notification.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.inventory.kinds_result")]
    pub struct ListKindsResult {
        pub kinds: Vec<KindDescriptorWire>,
    }

    /// One native actor's per-handler reply contract on the wire — the
    /// mirror of `aether_data::name_inventory::HandlerEntry` (ADR-0109
    /// §5) and the native analogue of the wasm [`HandlerCapability`].
    /// `namespace` is the owning cap's mailbox; `id` / `name` are the
    /// handler's input kind; `reply` is its declared reply kind id
    /// (`None` for a `-> ()` fire-and-forget handler, `Some` for a
    /// `-> R` synchronous or `-> Pending<R>` deferred reply). Carries no
    /// `doc` — the native link-time inventory holds ids + names, so a
    /// native cap's per-handler docs are out of scope here (the wasm
    /// `HandlerCapability` carries them from the custom section instead).
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct HandlerEntryWire {
        pub namespace: String,
        pub id: aether_data::KindId,
        pub name: String,
        pub reply: Option<aether_data::KindId>,
    }

    /// `aether.inventory.handlers` — request the running substrate's
    /// native handler manifest (ADR-0109 §5): every native chassis cap's
    /// per-handler `{ namespace, input kind, reply kind }`, collected at
    /// link time. Empty payload; the request *is* the signal. Mailed to
    /// the `"aether.inventory"` mailbox; reply: [`HandlersResult`].
    ///
    /// The MCP harness uses this to surface a native cap's `In -> Out`
    /// the way `describe_component` surfaces a wasm component's — the
    /// reply contract for the caps the driver leans on most
    /// (`aether.fs`, `aether.render`, `aether.audio`).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.inventory.handlers")]
    pub struct ListHandlers {}

    /// Reply to [`ListHandlers`] (ADR-0109 §5). One [`HandlerEntryWire`]
    /// per `#[handler]` across every native actor linked into the
    /// substrate, in link order. The harness folds these per `namespace`
    /// so each native cap reads as a `describe_component`-style handler
    /// list carrying its `In -> Out` reply contract.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.inventory.handlers_result")]
    pub struct HandlersResult {
        pub handlers: Vec<HandlerEntryWire>,
    }

    // Mesh-viewer structured load replies (issue 964). The mesh-viewer
    // component's `aether.mesh.load` was fire-and-forget — failures
    // warn-logged and the prior cache stayed, with no wire signal a
    // scenario harness or MCP `send_mail` caller could read. These two
    // reply kinds give the load path the same structured Ok/Err shape
    // the `aether.fs.*_result` family carries (ADR-0041), echoing the
    // request's `namespace` + `path` for correlation per the
    // explicit-nulls convention (every `Option` addressed, never an
    // absent field).
    //
    // Flat-struct shape (`ok: bool` + `error: Option<String>`) rather
    // than an Ok/Err enum so a caller reads success/failure off one
    // field without matching a variant, and so `warnings` rides along
    // on a successful load (e.g. a clamped sphere subdivision) without
    // forcing a third variant. The diagnostic *content* of `error` /
    // `warnings` is a sibling issue — this kind ships only the shape.

    /// `aether.mesh.load_result` — reply to `aether.mesh.load`
    /// (`aether_mesh_viewer::LoadMesh`). Echoes the request's
    /// `namespace` + `path` so the caller correlates the reply to its
    /// source without a pending-op queue — operation identity comes
    /// from the reply kind, target identity from the echoed fields.
    /// `ok` is the single success/failure read; `error` is `Some` iff
    /// `ok` is false (read / utf-8 / parse / mesh / unknown-extension
    /// failure); `warnings` carries non-fatal notes (e.g. an
    /// auto-clamped sphere subdivision) on an otherwise-successful
    /// load. Whole-mesh atomic-replace semantics are preserved: a
    /// failed load leaves the prior cached triangles intact.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.mesh.load_result")]
    pub struct MeshLoadResult {
        pub ok: bool,
        pub namespace: String,
        pub path: String,
        pub error: Option<String>,
        pub warnings: Vec<String>,
    }

    /// `aether.scene.load_result` — reply to a future `aether.scene.load`
    /// (issue 964 ships the reply shape ahead of the multi-instance
    /// scene loader; the wire is the bottleneck its sibling issues fill).
    /// Echoes the request's `namespace` + `path`. Whole-scene
    /// atomic-replace semantics are preserved — `ok` is the overall
    /// verdict, `instances_loaded` counts the instances that landed,
    /// and `instance_errors` maps each failed instance name to its
    /// failure reason so a partial scene is diagnosable per-instance.
    /// `error` carries a whole-scene failure (e.g. the scene file
    /// itself failed to read / parse) distinct from the per-instance
    /// `instance_errors`. `BTreeMap` rather than `HashMap` because
    /// `aether-kinds` is `no_std` + `alloc` and the `Schema` derive
    /// encodes `BTreeMap` as `SchemaType::Map` (it rejects `HashMap`);
    /// the keyed-by-instance-name semantics are identical.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.scene.load_result")]
    pub struct SceneLoadResult {
        pub ok: bool,
        pub namespace: String,
        pub path: String,
        pub error: Option<String>,
        pub instance_errors: BTreeMap<String, String>,
        pub instances_loaded: u32,
        pub warnings: Vec<String>,
    }

    // ADR-0081 per-actor log storage. Each actor owns an
    // `ActorLogRing` (in `aether-actor::log`); one wire kind pair
    // drives the query path:
    //
    // - `LogTail` / `LogTailResult` — per-actor query, every actor
    //   responds via the framework-built-in dispatch arm. The MCP
    //   `actor_logs` tool wraps this for a named mailbox; cross-
    //   actor aggregation (when callers want it) is client-side
    //   composition over the same per-actor surface (filed as
    //   iamacoffeepot/aether#960 for the missing fan-out primitive
    //   if substrate-side aggregation ever becomes worthwhile).
    //
    // `LogBatch` / `LogEvent` (the pre-ADR-0081 flush-hop kinds) and
    // `LogRead` / `LogReadResult` (the issue 776 pull surface that
    // `LogCapability` served) retired alongside `LogCapability`.

    /// One log entry as it appears on the wire when an MCP caller
    /// queries an actor's ring via [`LogTail`] / [`LogTailResult`].
    ///
    /// `level` follows the same `0 = trace .. 4 = error` mapping the
    /// rest of `aether.log.*` uses. `origin` is the `MailboxId` of
    /// the actor whose ring buffered the entry: `None` from the
    /// per-actor framework reply (the responder IS the origin —
    /// stamped at client side if the caller is merging across
    /// actors).
    ///
    /// `sequence` is monotonic *per actor's ring*, starting at 1.
    /// Callers walk a single actor's ring via `LogTail::since`; the
    /// cursor is per-actor.
    ///
    /// Not a `Kind` — only addressable as an element of
    /// `LogTailResult::Ok::entries`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct LogEntry {
        pub timestamp_unix_ms: u64,
        pub level: u8,
        pub target: String,
        pub message: String,
        pub sequence: u64,
        pub origin: Option<aether_data::MailboxId>,
    }

    /// `aether.log.tail` — query one actor's `ActorLogRing`.
    /// Routed to a specific actor by `MailboxId`; the framework's
    /// dispatch loop services this directly (every native actor and
    /// every wasm trampoline answers without the author writing a
    /// handler). Reply: [`LogTailResult`].
    ///
    /// - `max == 0` resolves to the substrate-default cap (currently
    ///   100) — the reply slice never exceeds `MAX_TAIL_MAX` (1000;
    ///   defined in `aether_actor::log`) even on a full ring.
    /// - `min_level: None` returns every level; `Some(2)` returns
    ///   info and above; same `0..=4` mapping the rest of
    ///   `aether.log.*` uses.
    /// - `since: None` returns from the oldest entry in the ring;
    ///   `Some(n)` returns only entries with `sequence > n`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.log.tail")]
    pub struct LogTail {
        pub max: u32,
        pub min_level: Option<u8>,
        pub since: Option<u64>,
    }

    /// Reply to [`LogTail`]. `Ok::entries` slices the responder's
    /// ring matching `(min_level, since)`, ordered oldest-to-newest
    /// (ascending `sequence`). `next_since` is the highest `sequence`
    /// in `entries` (or the caller's `since` echoed back on an empty
    /// reply) — thread it into the next `LogTail::since` for a
    /// stable per-actor cursor. `truncated_before` is set when the
    /// ring evicted entries the caller hadn't seen yet (the lowest
    /// `sequence` still in the ring): callers either accept the gap
    /// or poll more often. `entries[i].origin` is `None` — the
    /// responder IS the origin; client-side merge code stamps it if
    /// aggregating across actors.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.log.tail_result")]
    pub enum LogTailResult {
        Ok {
            entries: Vec<LogEntry>,
            next_since: u64,
            truncated_before: Option<u64>,
        },
        Err {
            error: String,
        },
    }

    // iamacoffeepot/aether#1128 per-handler execution-cost EWMA dump.
    // Each actor folds `(Finished.t − Received.t)` from the dispatch
    // trace bracket into a per-handler `CostCell` (in `aether-actor`);
    // one wire kind pair drives the read-only diagnostic dump, the
    // sibling of `LogTail` / `trace::TraceTail`. Measure-only — Phase 0
    // of iamacoffeepot/aether#1127's cost-aware recruiter, no scheduling
    // change.

    /// One handler's folded execution-cost row as it appears on the
    /// wire when a caller dumps an actor's cost table via [`CostTail`] /
    /// [`CostTailResult`]. `mean_nanos` / `mad_nanos` are the
    /// fixed-point-nanos EWMA mean and mean-absolute-deviation;
    /// `samples` is the folded-sample count (`0` is the neutral seed —
    /// a handler the actor declares but hasn't run yet). `kind_name` is
    /// the substrate-resolved kind name when known, else `None` (a
    /// component-defined kind the dumping engine can't name).
    ///
    /// Not a `Kind` — only addressable as an element of
    /// [`CostTailResult::Ok::rows`].
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct CostRow {
        pub kind_id: aether_data::KindId,
        pub kind_name: Option<String>,
        pub mean_nanos: u64,
        pub mad_nanos: u64,
        pub samples: u64,
    }

    /// `aether.cost.tail` — dump one actor's per-handler execution-cost
    /// EWMA table (iamacoffeepot/aether#1128). Routed to a specific
    /// actor by `MailboxId`; the framework dispatch loop services it
    /// directly (every native actor and every wasm trampoline answers
    /// without the author writing a handler), the same surface
    /// [`LogTail`] / [`crate::trace::TraceTail`] established. Reply:
    /// [`CostTailResult`].
    ///
    /// - `kind: None` returns every handler row the actor declares;
    ///   `Some(id)` returns only that one handler's row (or an empty
    ///   `rows` if the actor has no such handler).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.cost.tail")]
    pub struct CostTail {
        pub kind: Option<aether_data::KindId>,
    }

    /// Reply to [`CostTail`]. `Ok::rows` is one [`CostRow`] per handler
    /// the responding actor declares (filtered to `CostTail::kind` when
    /// set), in unspecified order. `Err` carries a free-form reason
    /// (the actor had no stamped slots / cost cache — a substrate
    /// invariant violation in practice).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.cost.tail_result")]
    pub enum CostTailResult {
        Ok { rows: Vec<CostRow> },
        Err { error: String },
    }

    // ADR-0066: camera control kinds (`aether.camera.{create, destroy,
    // set_active, set_mode, orbit.set, topdown.set}` + `OrbitParams` /
    // `TopdownParams` / `ModeInit`) live in the `aether-kit::camera`
    // trunk module. The `aether.camera` view_proj sink contract above stays
    // here — it's a chassis primitive consumed by the desktop chassis's
    // `aether.render` mailbox (the camera mailbox folded into
    // render per ADR-0074 §Decision 7; the kind name is unchanged).
    // The migrated kinds are still wire-compatible (kind names +
    // schemas unchanged); only the source-side home moved.

    // ADR-0066: `aether.mesh.load` moved to the `aether-mesh-viewer`
    // trunk crate.

    /// `aether.test_bench.advance` — request the test-bench chassis
    /// step the world forward by `ticks` Tick events. Each tick
    /// dispatches a `Tick` mail to every subscriber, drains the
    /// resulting mail to quiescence, and renders one frame. Replies
    /// with `AdvanceResult` once all ticks have completed.
    ///
    /// The test-bench chassis is event-driven (ADR-0067): without
    /// an `advance` request the world doesn't tick at all. Smoke
    /// scripts pair this with `capture_frame` to drive deterministic
    /// "send mail → step N → capture" cycles. Other chassis reply
    /// `Err { error: "unsupported on <kind> chassis" }` so callers
    /// fail fast.
    ///
    /// Reply: `AdvanceResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.test_bench.advance")]
    pub struct Advance {
        pub ticks: u32,
    }

    /// Reply to `Advance`. `Ok` echoes the number of ticks completed
    /// (always equal to the request's `ticks` on the happy path —
    /// the variant is structured so a future partial-completion
    /// outcome can extend it without widening the kind). `Err`
    /// carries a free-form reason: chassis doesn't support advance,
    /// dispatcher wedged mid-advance, etc.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.test_bench.advance_result")]
    pub enum AdvanceResult {
        Ok { ticks_completed: u32 },
        Err { error: String },
    }

    /// One environment variable pair carried in [`Spawn::env`]. Pairs
    /// rather than a `HashMap` because structured wire kinds
    /// don't have a `Schema` impl for tuple element types and a
    /// keyed-collection schema isn't load-bearing here — duplicate
    /// keys aren't expected and last-write-wins matches the env
    /// `HashMap` the hub builds anyway.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct EnvVar {
        pub key: String,
        pub value: String,
    }

    /// `aether.process.spawn` — request the hub chassis launch a
    /// substrate binary as a child process and return the assigned
    /// engine id once the child completes its `Hello` handshake
    /// (ADR-0078 Phase 1, supersedes ADR-0009 §3 for the post-actor
    /// model spawn path). `binary_path` is the absolute filesystem
    /// path to the substrate binary. `args` and `env` are forwarded
    /// to the child verbatim; the hub also injects `AETHER_HUB_URL`
    /// pointing at its engine listener so the child dials back.
    /// `handshake_timeout_ms` caps how long the hub waits for the
    /// child's `Hello` before declaring the spawn failed (default
    /// 5000 ms when `None`). Reply: `SpawnResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.process.spawn")]
    pub struct Spawn {
        pub binary_path: String,
        pub args: Vec<String>,
        pub env: Vec<EnvVar>,
        pub handshake_timeout_ms: Option<u32>,
    }

    /// Reply to `Spawn`. `Ok` carries the freshly minted engine id
    /// in tagged-string form (`eng-...` per ADR-0064 — `EngineId`
    /// doesn't implement `Schema`, so the wire carries the
    /// authoritative string the substrate registry already uses
    /// at the MCP boundary). The hub adopted the child into its
    /// registry; lifetime is tied to the connection until `Terminate`
    /// or external exit. `Err` carries a free-form reason — io
    /// failure, missing pid, handshake timeout.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.process.spawn_result")]
    pub enum SpawnResult {
        Ok { engine_id: String, pid: u32 },
        Err { error: String },
    }

    /// `aether.process.terminate` — request the hub chassis shut down
    /// a previously-spawned substrate. Sends SIGTERM, waits up to
    /// `grace_ms` (default 2000 ms when `None`), then escalates to
    /// SIGKILL if the child is still running. `engine_id` is the
    /// Uuid string form the hub returned from `Spawn`. Reply:
    /// `TerminateResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.process.terminate")]
    pub struct Terminate {
        pub engine_id: String,
        pub grace_ms: Option<u32>,
    }

    /// Reply to `Terminate`. `Ok` reports the child's exit code (if
    /// the kernel returned one) and whether escalation to SIGKILL
    /// was needed. `Err` is for unknown engine ids, externally-
    /// connected engines the hub didn't spawn, or os-level signal
    /// failure.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.process.terminate_result")]
    pub enum TerminateResult {
        Ok {
            exit_code: Option<i32>,
            sigkilled: bool,
        },
        Err {
            error: String,
        },
    }

    /// `aether.process.exited` — broadcast emitted by the hub's
    /// reaper when a spawned child terminates (whether via
    /// `Terminate` mail or external exit). Fire-and-forget; lands
    /// on every attached MCP session via `egress_broadcast`. The
    /// reaper task converts `Child::wait` completion into this kind
    /// so any cap or operator that wants to react to "engine X
    /// exited" subscribes to broadcast rather than threading a
    /// callback through `EngineRegistry`. `engine_id` is the
    /// Uuid string form the hub used while the child was alive.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.process.exited")]
    pub struct ProcessExited {
        pub engine_id: String,
        pub exit_code: Option<i32>,
        pub reason: String,
    }

    // ADR-0050 per-provider content-gen caps. The `aether.anthropic`
    // kinds (Role, Message, AnthropicError, MessagesSend, CliSend,
    // MessagesSendResult, CliSendResult) are owned by the capability and
    // live in `aether-capabilities::anthropic::kinds` (ADR-0121). `Usage`
    // stays central — it is shared with the `aether.gemini` result kinds.

    /// Token + wall-clock accounting returned on a successful
    /// content-gen completion. Shared across the Anthropic text kinds
    /// (issue 1014) and the Gemini media kinds (issue 1015). The CLI
    /// backend can only report `wall_clock_millis` (the subprocess gives no
    /// token counts), leaving the token / cost fields zero / `None`;
    /// the Messages API and the Gemini APIs populate the rest where the
    /// provider reports them.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Usage {
        pub input_tokens: u32,
        pub output_tokens: u32,
        pub wall_clock_millis: u32,
        pub cost_micros: Option<u64>,
    }
}

mod trajectory {
    use alloc::string::String;
    use alloc::vec::Vec;

    use serde::{Deserialize, Serialize};

    /// One per-tick sample from a moving point's grid position and a
    /// scalar accumulator value. Sent by a producer to the
    /// `TrajectoryRecorderCapability` (`aether.trajectory`) every tick
    /// to record the point's current state. `seed` keys the session:
    /// all samples sharing a seed are accumulated into the same
    /// `TrajectoryLog`, emitted when `TrajectoryEnd` arrives for that
    /// seed. Fire-and-forget; the recorder has no per-sample reply.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.trajectory.sample")]
    pub struct TrajectorySample {
        /// Session discriminator. All samples with the same seed are
        /// accumulated into a single `TrajectoryLog` handle.
        pub seed: u64,
        /// Tick counter at which this sample was captured. Preserved
        /// verbatim in the log so offline transforms replay in tick
        /// order.
        pub tick: u32,
        /// Grid column the point occupied at this tick.
        pub x: u32,
        /// Grid row the point occupied at this tick.
        pub y: u32,
        /// Scalar accumulator value at this tick (e.g. a score,
        /// resource count, or distance travelled — domain-agnostic).
        pub value: u32,
    }

    /// Reason a trajectory session ended. Domain-free and self-describing
    /// so an LLM caller can interpret the terminal event without needing
    /// additional context (ADR memory: design for machine consumers).
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum TrajectoryEndReason {
        /// The session ran to its natural conclusion (e.g. the point
        /// reached its target or exhausted a fixed step budget).
        Completed,
        /// The session was cut short by a soft limit (e.g. a time
        /// budget was exceeded, or a step cap was reached before the
        /// natural end condition).
        Truncated,
        /// The session was cancelled by the producer before it reached
        /// a natural or soft-limit end condition.
        Aborted,
    }

    /// Terminal marker for a trajectory session. Signals the
    /// `TrajectoryRecorderCapability` to build the `TrajectoryLog` for
    /// `seed` from all accumulated `TrajectorySample`s and return it
    /// inline. Reply: `RecordResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.trajectory.end")]
    pub struct TrajectoryEnd {
        /// The same seed used in the `TrajectorySample` stream being
        /// terminated. Selects which buffer the recorder flushes.
        pub seed: u64,
        /// Why the session ended. Carried verbatim into `TrajectoryLog`
        /// so offline analysis can filter by outcome.
        pub reason: TrajectoryEndReason,
    }

    /// One tick's worth of position + accumulator data, as stored in a
    /// `TrajectoryLog`. Separates the on-wire sample shape
    /// (`TrajectorySample`, which also carries `seed`) from the stored
    /// shape (which doesn't need `seed` since all entries share the log's
    /// single `seed` field).
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct TrajectorySampleEntry {
        pub tick: u32,
        pub x: u32,
        pub y: u32,
        pub value: u32,
    }

    /// A complete, tick-ordered record of one trajectory session
    /// (`aether.trajectory.log`, `TrajectoryLog::ID`). Built by
    /// `TrajectoryRecorderCapability` at terminal time (on
    /// `TrajectoryEnd`) keyed by `seed` and returned inline in
    /// `RecordResult`. Offline analysis transforms decode this value to
    /// replay the session's path.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.trajectory.log")]
    pub struct TrajectoryLog {
        /// The session seed — matches the `seed` on every
        /// `TrajectorySample` in this log.
        pub seed: u64,
        /// Tick-ordered list of recorded samples. The recorder appends
        /// in the order `TrajectorySample` mails arrive; a well-behaved
        /// producer sends them in ascending-tick order.
        pub samples: Vec<TrajectorySampleEntry>,
        /// Why the session ended, propagated from `TrajectoryEnd`.
        pub end_reason: TrajectoryEndReason,
    }

    /// Reply to `TrajectoryEnd`. `Ok` carries the seed and the complete
    /// `TrajectoryLog` for the session inline; `Err` is returned when
    /// `seed` has no in-flight session (unknown or already terminated).
    /// An oversized inline reply spills to a file on the MCP side, so the
    /// caller reads the log back the same way regardless of its size.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.trajectory.record_result")]
    pub enum RecordResult {
        Ok { seed: u64, log: TrajectoryLog },
        Err { seed: u64, error: String },
    }
}
