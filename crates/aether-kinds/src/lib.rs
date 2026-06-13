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

pub mod dag;
pub mod descriptors;
pub mod keycode;
pub mod trace;

pub use dag::*;

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
/// the lifecycle-family counterpart of [`UnsubscribeAll`] for
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
/// [`SubscribeInputResult`] for input subscriptions.
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

/// A single world-space vertex with per-vertex color. Matches the
/// substrate's `VertexBufferLayout`: `(pos: vec3<f32>, color: vec3<f32>)`,
/// 24 bytes on the wire. Positions are world-space; the shader
/// multiplies by the camera's `view_proj` uniform to produce clip
/// space. Not a kind on its own — only addressable as the element
/// type inside `DrawTriangle.verts`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Schema)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// A draw-triangle item. One `DrawTriangle` is three vertices; the mail
/// `count` field is the number of triangles in the payload when
/// sent as a slice.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.draw_triangle")]
pub struct DrawTriangle {
    pub verts: [Vertex; 3],
}

/// Wire size of one `aether.draw_triangle` item: three `Vertex`es.
/// Property of the wire shape, lives next to `DrawTriangle` so any
/// chassis / sink that needs to clamp at whole-triangle boundaries
/// has one canonical source. `repr(C)` + `Pod` + `[Vertex; 3]` packs
/// without padding, so `size_of::<DrawTriangle>()` is exactly the
/// per-triangle wire footprint.
pub const DRAW_TRIANGLE_BYTES: usize = size_of::<DrawTriangle>();

/// Camera state: column-major `view_proj` matrix (world → clip). The
/// desktop chassis's `camera` sink writes the latest payload into the
/// GPU uniform every frame; the WGSL vertex shader multiplies each
/// vertex position by this matrix. Column-major layout matches wgpu's
/// uniform upload — 64 bytes uploaded verbatim, no transpose. Camera
/// components emit this on every `Tick`; the substrate reads only the
/// most recent value before issuing the next draw. Before the first
/// `Camera` arrives, the uniform holds identity and vertices render
/// in clip-space 1:1 (the pre-camera behaviour).
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.camera")]
pub struct Camera {
    pub view_proj: [f32; 16],
}

// `aether.camera.*` control kinds (CameraCreate / CameraDestroy /
// CameraSetActive / CameraSetMode / CameraOrbitSet / CameraTopdownSet)
// live in `mod control_plane` below — they're postcard-shaped because
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
/// the node boundary consistent: a DAG `Source` encodes its output and
/// the transform decodes its input through the same shape-agnostic
/// `Kind` path, so cast bytes agree on both sides.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.math.mat4_apply")]
pub struct Mat4Apply {
    pub matrix: Mat4,
    pub vector: Vec4,
}

/// Start a note playing on the desktop chassis's MIDI synth (ADR-0039).
/// `pitch` is a standard MIDI note number (0–127, middle C = 60).
/// `velocity` is 0–127 (MIDI convention; 0 has the same effect as a
/// `NoteOff`, but agents should prefer `NoteOff` for clarity).
/// `instrument_id` indexes the substrate-resident instrument registry
/// — v1 ships a fixed set; future patch-based instruments (Phase 2
/// follow-up) will extend the id space without a wire change. The
/// substrate keys the allocated voice by `(sender_mailbox, instrument_id,
/// pitch)` so same-pitch notes from different senders or different
/// instruments don't stomp each other. Fire-and-forget; no reply.
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
#[kind(name = "aether.audio.note_on")]
pub struct NoteOn {
    pub pitch: u8,
    pub velocity: u8,
    pub instrument_id: u8,
}

/// Release a note previously started with `NoteOn`. The substrate
/// matches on `(sender_mailbox, instrument_id, pitch)` — the sender
/// is taken from the mail envelope, not carried in the payload. A
/// `NoteOff` that doesn't match any live voice is silently ignored
/// (normal during race windows between envelope release and late
/// note-offs). Fire-and-forget; no reply.
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
#[kind(name = "aether.audio.note_off")]
pub struct NoteOff {
    pub pitch: u8,
    pub instrument_id: u8,
}

/// Set the substrate's master audio gain. `gain` is a linear scalar
/// applied to the summed voice output before the cpal device buffer;
/// `1.0` is unity, `0.0` mutes, values above `1.0` are clamped to
/// avoid clipping. This is the only substrate-level gain control —
/// per-source and bus-level attenuation are user-space concerns (ADR-0039).
/// Desktop-only: headless and hub chassis reply with an
/// `unsupported on <chassis>` error. Fire-and-forget in the happy path.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.audio.set_master_gain")]
pub struct SetMasterGain {
    pub gain: f32,
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
// fields are postcard-encoded on the wire, hub-encodable from agent
// params (no more `payload_bytes` workaround), and the substrate
// decodes them with `postcard::from_bytes` against the same types
// that ship as the kind.
//
// Gated behind `descriptors` because the types use `String`/`Vec`/
// `Option` — wasm guests that don't enable descriptors stay free of
// the alloc-heavy payload types (and have no business loading
// components anyway).

pub use control_plane::*;
pub use engine::*;
pub use rpc::*;
pub use tcp::*;

mod tcp {
    use alloc::string::String;
    use alloc::vec::Vec;

    use serde::{Deserialize, Serialize};

    /// `aether.tcp.bind_listener` — request the singleton
    /// `TcpCapability` to spawn a fresh `TcpListenerActor` bound to
    /// `addr`. The cap parses `addr` via `std::net::ToSocketAddrs`
    /// (so `"127.0.0.1:8080"` and `"0.0.0.0:0"` both work; the
    /// latter asks the OS to pick a free port). Optional `name`
    /// overrides the default subname (the bound port string); pass
    /// `None` for the default. Reply: `BindListenerResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.tcp.bind_listener")]
    pub struct BindListener {
        pub addr: String,
        pub name: Option<String>,
    }

    /// Reply to `BindListener`. `Ok` carries the resolved listener
    /// name (the deterministic subname under
    /// `aether.tcp.listener:<name>`), the listener's `MailboxId`,
    /// and the actually-bound local port (load-bearing when `addr`
    /// requested port 0). `Err` carries a human-readable reason —
    /// addr parse failures, port-in-use, OS bind errors, namespace
    /// collisions.
    ///
    /// Per project memory `feedback_mcp_mailbox_id_json_precision`:
    /// `MailboxId` round-trips imprecisely over JSON. Agents
    /// addressing the listener via subsequent MCP calls should use
    /// `listener_name` (the deterministic full name); `listener_id`
    /// is the wire id for native peers.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.tcp.bind_listener_result")]
    pub enum BindListenerResult {
        Ok {
            listener_name: String,
            listener_id: aether_data::MailboxId,
            local_port: u16,
        },
        Err {
            addr: String,
            reason: String,
        },
    }

    /// `aether.tcp.unbind_listener` — request the singleton
    /// `TcpCapability` to close a listener by subname. The cap
    /// resolves the listener via `chassis.resolve_actor`, mails
    /// `Close` to it, monitors its close, and replies once
    /// `MonitorNotice` arrives. Asynchronous reply: the response
    /// only fires after the listener's accept thread has joined
    /// and its slot has tombstoned.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.tcp.unbind_listener")]
    pub struct UnbindListener {
        pub listener_name: String,
    }

    /// Reply to `UnbindListener`. `Ok` once the listener has
    /// tombstoned (the cap waited on `MonitorNotice` before
    /// replying). `Err` for unknown listener names, listeners
    /// already tombstoned at the time of the unbind request, or
    /// fan-out failures.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.tcp.unbind_listener_result")]
    pub enum UnbindListenerResult {
        Ok {
            listener_name: String,
        },
        Err {
            listener_name: String,
            reason: String,
        },
    }

    /// `aether.tcp.list_listeners` — enumerate every live listener
    /// the singleton knows about. The cap reaches for
    /// `chassis.resolve_actors::<TcpListenerActor>()` (Phase 5)
    /// and walks the live fleet. Reply: `ListListenersResult`.
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.tcp.list_listeners")]
    pub struct ListListeners {}

    /// One entry in `ListListenersResult`. `name` is the subname
    /// (e.g. `"8080"`); `addr` is the requested bind addr passed
    /// to `BindListener`; `port` is the actually-bound local port.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct ListenerInfo {
        pub name: String,
        pub addr: String,
        pub port: u16,
    }

    /// Reply to `ListListeners`. Always `Ok` — listing has no
    /// failure mode that can't be expressed by an empty list.
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.tcp.list_listeners_result")]
    pub struct ListListenersResult {
        pub listeners: Vec<ListenerInfo>,
    }

    /// `aether.tcp.close` — peer asks a `TcpListenerActor` to
    /// gracefully close. Mailed by `TcpCapability::on_unbind`; the
    /// listener's handler signals its accept thread, joins, and
    /// calls `ctx.shutdown()`. Fire-and-forget at the kind level
    /// (the close response rides via the cap's monitor on the
    /// listener, not via this kind).
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.tcp.close")]
    pub struct Close {}

    /// `aether.tcp.connection_ready` — sidecar accept thread → listener
    /// dispatcher wake. Issue 607 Phase 6b: the listener's accept
    /// thread blocks on `accept()`, pushes the resulting `TcpStream`
    /// over an mpsc into the dispatcher, then fires this mail at its
    /// own listener mailbox to wake the handler. The handler drains
    /// the mpsc and spawns a `TcpSessionActor` per pending stream.
    /// Empty payload — the actual stream rides the mpsc, not the mail
    /// envelope (a live `TcpStream` is not wire-shaped).
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.tcp.connection_ready")]
    pub struct ConnectionReady {}

    /// `aether.tcp.session_data_ready` — sidecar read thread → session
    /// dispatcher wake. Mirror of [`ConnectionReady`] for the session
    /// read path: the read thread blocks on `read()`, pushes bytes via
    /// mpsc, fires this mail at its own session mailbox. The handler
    /// drains the mpsc and broadcasts each chunk as [`SessionData`].
    /// Empty payload.
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.tcp.session_data_ready")]
    pub struct SessionDataReady {}

    /// `aether.tcp.session_data` — broadcast emitted by a
    /// `TcpSessionActor` on each chunk read from its peer. Carries
    /// the session subname (`conn-N`), the peer address as a string,
    /// and the bytes received in one `read()` call. Postcard-shaped
    /// (variable-length payload) — agents drain via `receive_mail`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.tcp.session_data")]
    pub struct SessionData {
        pub session_name: String,
        pub peer: String,
        pub bytes: Vec<u8>,
    }

    /// `aether.tcp.session_write` — peer mails this to a
    /// `TcpSessionActor` to write `bytes` to the connected stream.
    /// Fire-and-forget; the session's handler does a blocking write
    /// on the dispatcher thread (writes are typically fast and
    /// dispatcher-thread initiated, so a sidecar isn't needed for
    /// the write path).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.tcp.session_write")]
    pub struct SessionWrite {
        pub bytes: Vec<u8>,
    }

    /// `aether.tcp.session_close` — peer asks the session to close
    /// gracefully. Mailed via `ctx.actor::<TcpSessionActor>(...)` or
    /// resolved by subname. The session's handler calls
    /// `ctx.shutdown()`; the close fan-out fires `MonitorNotice` to
    /// the parent listener (which spawned it).
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.tcp.session_close")]
    pub struct SessionClose {}

    /// `aether.tcp.session_closed` — broadcast emitted on session
    /// close. Carries the session subname, the peer address, and a
    /// human-readable reason ("eof", "read error: ...", "explicit
    /// close", etc.). Agents observe via `receive_mail` to know when
    /// a session terminated and clean up any per-session state they
    /// were tracking.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.tcp.session_closed")]
    pub struct SessionClosed {
        pub session_name: String,
        pub peer: String,
        pub reason: String,
    }
}

mod rpc {
    use serde::{Deserialize, Serialize};

    /// `aether.rpc.inbound_ready` — sidecar accept / read thread →
    /// `RpcServerCapability` dispatcher wake. Issue 750. Mirrors the
    /// `ConnectionReady` / `SessionDataReady` pattern for `aether.tcp`:
    /// the sidecar pushes work over an internal mpsc and fires this
    /// (empty-payload) mail at the cap's mailbox so the dispatcher
    /// handler drains the queue. The mpsc carries the live data
    /// (`TcpStream`, frame bytes, close reason) — a `TcpStream` isn't
    /// wire-shaped and a frame's payload may be megabytes, so the mail
    /// is only the wakeup signal.
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.rpc.inbound_ready")]
    pub struct RpcInboundReady {}
}

mod engine {
    use alloc::string::String;
    use alloc::vec::Vec;

    use serde::{Deserialize, Serialize};

    /// `aether.engine.forward` — hand a per-engine proxy
    /// (`aether.engine.proxy:<id>`) one mail to relay to its substrate
    /// over the proxy's outbound RPC connection. Issue 763 P3.
    ///
    /// Carries the *remote* target explicitly: a plain mail to the
    /// proxy is only `kind` + `payload` — it can't say *which mailbox
    /// on the substrate* to deliver to. `ForwardEnvelope` is that
    /// carrier. The proxy wraps `mailbox` + `kind` + the already-encoded
    /// `payload` into an RPC `Call`; the substrate's
    /// `RpcServerCapability` dispatches it into its local actor system.
    /// Any reply streams back through the proxy and routes to whoever
    /// sent this `ForwardEnvelope` — the proxy keys reply correlation
    /// off the inbound mail's `Source`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.forward")]
    pub struct ForwardEnvelope {
        pub mailbox: aether_data::MailboxId,
        pub kind: aether_data::KindId,
        pub payload: Vec<u8>,
    }

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

    /// `aether.engine.list_result` — reply to [`ListEngines`]: every
    /// engine the cap supervises right now. Issue 763 P4.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.list_result")]
    pub struct ListEnginesResult {
        pub engines: Vec<EngineDescriptor>,
    }

    /// `aether.engine.spawn` — ask the engines cap to fork+exec a
    /// substrate binary and connect a per-engine proxy to it. Issue
    /// 763 P4.
    ///
    /// The cap picks a free localhost port for the substrate's
    /// `RpcServerCapability`, injects it as `AETHER_RPC_PORT`, forks
    /// `binary_path` with `args` forwarded verbatim, then boots an
    /// `aether.engine.proxy:<id>` actor that dials it. Reply:
    /// [`SpawnEngineResult`].
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
        pub binary_path: String,
        pub args: Vec<String>,
        pub boot_manifest: Option<String>,
    }

    /// Reply to [`SpawnEngine`]. Issue 763 P4.
    ///
    /// `Ok` carries the freshly minted `engine_id` (plain UUID string —
    /// pass it back to [`TerminateEngine`]) and the `rpc_port` the cap
    /// assigned. `Err` carries a free-form reason — fork failure, or
    /// the proxy failing to connect within the substrate's startup
    /// window. On `Err` no child process is left running.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.spawn_result")]
    pub enum SpawnEngineResult {
        Ok { engine_id: String, rpc_port: u16 },
        Err { error: String },
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

    /// `aether.engine.route` — ask the engines cap (`aether.engine`) to
    /// relay one mail to a *specific* engine's substrate. Issue 763 P5a.
    ///
    /// The engine-addressed sibling of [`ForwardEnvelope`]: where
    /// `ForwardEnvelope` already names a proxy and only needs the
    /// substrate-local `mailbox` + `kind` + `payload`, `RouteEnvelope`
    /// also carries the `engine_id`, because the sender (the hub's
    /// `RpcServerCapability`, relaying an `engine = Some(_)` wire
    /// `Call`) doesn't know which proxy hosts that engine. The engines
    /// cap looks the engine up in its table and re-emits a
    /// `ForwardEnvelope` at the right `aether.engine.proxy:<id>`,
    /// propagating the original reply-to so the substrate's reply
    /// streams back to the originating `RpcServerCapability`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.route")]
    pub struct RouteEnvelope {
        pub engine_id: String,
        pub mailbox: aether_data::MailboxId,
        pub kind: aether_data::KindId,
        pub payload: Vec<u8>,
    }

    /// `aether.engine.call_settled` — a per-engine proxy's signal that
    /// a forwarded RPC call has run to completion. Issue 763 P5a.
    ///
    /// When the proxy relays a [`ForwardEnvelope`] as an RPC `Call`,
    /// the substrate eventually answers with a wire `ReplyEnd`. The
    /// proxy lifts that terminal frame into this kind and pushes it
    /// back to whoever opened the call (correlation preserved) — the
    /// hub's `RpcServerCapability` matches it to the in-flight wire
    /// call and writes its own `ReplyEnd` to the RPC client. (Local,
    /// non-forwarded calls close on chassis settlement instead; a
    /// forwarded call has no local chain to settle, so it needs this
    /// explicit terminal signal.) `Err` carries the wire `RpcError`
    /// rendered as a string — the structured variant doesn't survive
    /// the `aether-kinds` layer, which can't depend on the RPC crate.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.call_settled")]
    pub enum CallSettled {
        Ok,
        Err { error: String },
    }

    /// `aether.engine.heartbeat_tick` — the per-engine proxy's own
    /// liveness timer wake (issue 1339). Internal control-plane mail,
    /// not a user surface: a sidecar thread the proxy spawns at init
    /// fires this (empty-payload) at the proxy's own mailbox every
    /// heartbeat interval, the same wake-mail shape `RpcInboundReady`
    /// uses for the reader sidecar. The handler pings the substrate and
    /// counts consecutive misses, evicting the engine once the miss
    /// limit is crossed.
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.engine.heartbeat_tick")]
    pub struct EngineHeartbeatTick {}

    /// `aether.engine.died` — a per-engine proxy telling the engines
    /// cap (`aether.engine`) that its substrate is gone, so the cap
    /// drops it from the supervised-engine table (issue 1339). The
    /// proxy sends this when it observes the connection close (`Bye` /
    /// `eof`) or when the liveness heartbeat crosses its miss limit —
    /// the positive signal the lazy connection-drop path misses for a
    /// wedged-but-alive engine. Idempotent on the cap side: a `died`
    /// for an already-removed engine (e.g. one a concurrent
    /// `TerminateEngine` already dropped) is a no-op. `engine_id` is
    /// the plain UUID string, matching [`TerminateEngine`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.died")]
    pub struct EngineDied {
        pub engine_id: String,
    }

    /// `aether.engine.alive` — a per-engine proxy reporting a confirmed
    /// liveness signal (a `Pong` answering its heartbeat `Ping`) to the
    /// engines cap (issue 1339). The cap stamps the engine's
    /// last-seen-alive time so [`ListEnginesResult`] can report
    /// `last_heartbeat_age_millis`. Fire-and-forget; an `alive` for an
    /// unknown engine is a no-op. `engine_id` is the plain UUID string.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.alive")]
    pub struct EngineAlive {
        pub engine_id: String,
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
        /// the guest's typed `FfiActor::init` at instantiate-time. An
        /// empty vec means "no config" — the c1 ABI short-circuits it
        /// to `&[]`, which a `Config = ()` guest decodes uniformly via
        /// `impl Kind for ()`. The carrier is raw bytes, not a typed
        /// kind, so the substrate stays byte-transparent: the hub /
        /// MCP encode the config struct to bytes at the edge
        /// (SDK-typed, not wire-typed), matching `wasm`'s `Vec<u8>`.
        pub config: Vec<u8>,
        /// ADR-0096: which exported actor type to instantiate from a
        /// multi-actor module, named by its `Actor::NAMESPACE`. `None`
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

    // ADR-0021 publish/subscribe routing for substrate input streams,
    // ADR-0068 keying. The substrate maintains one subscriber set per
    // input `KindId`; a `SubscribeInput` names the kind id and the
    // mailbox to add. Issue 638 Phase 2 rehomed these kinds from
    // `aether.control.*` to `aether.input.*`; the chassis-owned
    // `InputCapability` handles them inline and replies via
    // reply-to-sender.

    /// `aether.input.subscribe` — add `mailbox` to the subscriber set
    /// for `kind`. Idempotent: subscribing a mailbox already in the
    /// set is still `Ok` (subscriptions are a set, not a counter).
    /// Reply: `SubscribeInputResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.input.subscribe")]
    pub struct SubscribeInput {
        pub kind: aether_data::KindId,
        pub mailbox: aether_data::MailboxId,
    }

    /// `aether.input.subscribe_self` — reflexive counterpart of
    /// [`SubscribeInput`]: subscribe the *sending* actor to the input
    /// stream for `kind`, with no explicit `mailbox` field. The cap
    /// resolves the subscriber from the inbound envelope's host-stamped
    /// `Source` (ADR-0083) via `ctx.source_mailbox()`, so the
    /// subscriber cannot be forged and the op is gated to in-process
    /// actors by construction — an external session or another engine
    /// has no local mailbox and gets an `Err` reply, pushing it onto
    /// the named [`SubscribeInput`] form. This is the common
    /// "subscribe me" case. Reply: `SubscribeInputResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.input.subscribe_self")]
    pub struct SubscribeInputSelf {
        pub kind: aether_data::KindId,
    }

    /// `aether.input.unsubscribe` — remove `mailbox` from the
    /// subscriber set for `kind`. Idempotent: unsubscribing a mailbox
    /// that isn't subscribed is still `Ok`. Reply:
    /// `SubscribeInputResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.input.unsubscribe")]
    pub struct UnsubscribeInput {
        pub kind: aether_data::KindId,
        pub mailbox: aether_data::MailboxId,
    }

    /// `aether.input.unsubscribe_self` — reflexive counterpart of
    /// [`UnsubscribeInput`]: unsubscribe the *sending* actor from the
    /// input stream for `kind`, with no explicit `mailbox` field. The
    /// cap resolves the subscriber from the inbound envelope's
    /// host-stamped `Source` (ADR-0083), the same gating as
    /// [`SubscribeInputSelf`]. Idempotent on "not currently
    /// subscribed." Reply: `SubscribeInputResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.input.unsubscribe_self")]
    pub struct UnsubscribeInputSelf {
        pub kind: aether_data::KindId,
    }

    /// Reply to subscribe / unsubscribe / `unsubscribe_all` (ADR-0021 §2).
    /// Only failure mode: the target mailbox id doesn't name a live
    /// component (unknown, a sink, or already dropped).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.input.subscribe_result")]
    pub enum SubscribeInputResult {
        Ok,
        Err { error: String },
    }

    /// `aether.input.unsubscribe_all` — remove `mailbox` from every
    /// input stream's subscriber set. Issued by
    /// `ComponentHostCapability` on `DropComponent` so the cap's
    /// fan-out tables don't keep firing at a dropped trampoline.
    /// Idempotent: a mailbox with no subscriptions is still a no-op.
    /// Fire-and-forget; no reply. Cast-shape (Pod) — one
    /// `MailboxId`, fixed size.
    #[repr(C)]
    #[derive(
        Copy,
        Clone,
        Debug,
        PartialEq,
        Eq,
        bytemuck::Pod,
        bytemuck::Zeroable,
        aether_data::Kind,
        aether_data::Schema,
    )]
    #[kind(name = "aether.input.unsubscribe_all")]
    pub struct UnsubscribeAll {
        pub mailbox: aether_data::MailboxId,
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
    /// Reply: `CaptureFrameResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.capture_frame")]
    pub struct CaptureFrame {
        pub mails: Vec<MailEnvelope>,
        pub after_mails: Vec<MailEnvelope>,
        pub checks: Vec<FrameCheck>,
    }

    /// One mail in a `CaptureFrame.mails` bundle. Structurally mirrors
    /// `aether_data::MailFrame` — a pre-encoded payload plus
    /// the name-level addressing the substrate uses to resolve it.
    /// The hub encodes each envelope's `payload` via the kind's
    /// descriptor before wrapping it into the bundle, so the
    /// substrate side just pushes `Mail::new(mailbox, kind_id,
    /// payload, count)` directly.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct MailEnvelope {
        pub recipient_name: String,
        pub kind_name: String,
        pub payload: Vec<u8>,
        pub count: u32,
    }

    /// Reply to `CaptureFrame`. `Ok` carries the PNG bytes for the
    /// captured frame plus an optional [`FrameVerdict`] (present iff the
    /// request carried `checks`); `Err` carries a free-form reason —
    /// capture not supported on this surface, map failed, encode failed,
    /// or a bundle-resolution failure (unknown kind / mailbox) aborting
    /// before any mail was dispatched.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.capture_frame_result")]
    pub enum CaptureFrameResult {
        Ok {
            png: Vec<u8>,
            verdict: Option<FrameVerdict>,
        },
        Err {
            error: String,
        },
    }

    /// Build a [`CaptureFrameResult`] from the raw GPU `render_and_capture`
    /// result shape. Every capture handler in `aether-substrate-bundle`
    /// (test-bench inline, in-process bench, desktop driver) needs this
    /// same `Ok((png, verdict)) → Ok { png, verdict }` / `Err(error) → Err
    /// { error }` flip. `verdict` is `None` when the request carried no
    /// `checks`.
    impl From<Result<(Vec<u8>, Option<FrameVerdict>), String>> for CaptureFrameResult {
        fn from(result: Result<(Vec<u8>, Option<FrameVerdict>), String>) -> Self {
            match result {
                Ok((png, verdict)) => Self::Ok { png, verdict },
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

    // ADR-0105 textured-quad render surface. Three kinds on the
    // `aether.render` mailbox compose the generic texture surface text /
    // sprites / HUD images share: register an RGBA8 texture, overwrite a
    // sub-rect of one, and draw a batch of textured alpha-blended quads
    // in either projection. Postcard-shaped — `CreateTexture` /
    // `UpdateTexture` carry `Vec<u8>` pixels, `DrawTexturedQuads` carries
    // a `space` enum and a `Vec` of quads.

    /// `aether.render.create_texture` — register an RGBA8 texture in the
    /// render cap's session-scoped texture registry. `pixels` is exactly
    /// `width * height * 4` bytes (RGBA8, row-major, top-down). The cap
    /// validates the dimensions, assigns the next `texture_id` past any
    /// previously created texture (the same id-assignment shape ADR-0103
    /// uses for instrument ids), stages the pixels CPU-side, and replies
    /// as soon as the id is assigned — the wgpu texture is realized lazily
    /// at the next frame record. Reply: `CreateTextureResult`. Desktop-
    /// only — the headless chassis replies `Err` (fail-fast, ADR-0105).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.create_texture")]
    pub struct CreateTexture {
        pub width: u32,
        pub height: u32,
        pub pixels: Vec<u8>,
    }

    /// Reply to `CreateTexture`. `Ok` carries the assigned `texture_id` —
    /// thread it into `DrawTexturedQuads.texture_id` and
    /// `UpdateTexture.texture_id`. `Err` carries a human-readable reason —
    /// a zero dimension, or a `pixels` length that doesn't match
    /// `width * height * 4`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.create_texture_result")]
    pub enum CreateTextureResult {
        Ok { texture_id: u32 },
        Err { error: String },
    }

    /// `aether.render.update_texture` — overwrite a sub-rectangle of a
    /// previously-created texture's pixels (atlas growth — e.g. the text
    /// cap rasterizing a new glyph into its atlas). `pixels` is exactly
    /// `width * height * 4` bytes covering the `(x, y, width, height)`
    /// sub-rect. Fire-and-forget; a bad `texture_id` or an out-of-bounds
    /// rect logs and drops. The staged pixels update immediately; the GPU
    /// texture re-uploads at the next frame record.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.update_texture")]
    pub struct UpdateTexture {
        pub texture_id: u32,
        pub x: u32,
        pub y: u32,
        pub width: u32,
        pub height: u32,
        pub pixels: Vec<u8>,
    }

    /// One textured quad in a `DrawTexturedQuads` batch. `(x, y)` is the
    /// top-left corner and `(width, height)` the size, both in the unit
    /// the batch's `space` selects — window pixels for `Screen`, pixel
    /// offsets from the anchor for `World`. `(u0, v0)`–`(u1, v1)` is the
    /// uv sub-rect sampled from the batch's texture (`0,0` top-left to
    /// `1,1` bottom-right). `tint` is a linear RGBA multiplier applied to
    /// the sampled texel — `[1.0; 4]` draws the texture unmodified; the
    /// alpha channel scales the blend. Not a kind on its own — only
    /// addressable inside `DrawTexturedQuads.quads`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct TexturedQuad {
        pub x: f32,
        pub y: f32,
        pub width: f32,
        pub height: f32,
        pub u0: f32,
        pub v0: f32,
        pub u1: f32,
        pub v1: f32,
        pub tint: [f32; 4],
    }

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

    /// `aether.render.draw_textured_quads` — draw a batch of textured,
    /// alpha-blended quads sampling one texture, in the projection `space`
    /// selects. Accumulated per frame with the same immediate-mode
    /// contract as `aether.draw_triangle`: send it every frame the quads
    /// should appear, or they vanish next frame. `texture_id` is a
    /// registry id from a prior `CreateTexture`; an unknown id warn-drops
    /// the batch. Fire-and-forget; no reply.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.draw_textured_quads")]
    pub struct DrawTexturedQuads {
        pub texture_id: u32,
        pub space: QuadSpace,
        pub quads: Vec<TexturedQuad>,
    }

    /// One flat-colored quad in a `DrawSolidQuads` batch. `(x, y)` is the
    /// top-left corner and `(width, height)` the size, both in the unit
    /// the batch's `space` selects — window pixels for `Screen`, pixel
    /// offsets from the anchor for `World`. `color` is a linear RGBA value;
    /// the alpha channel scales the blend. Not a kind on its own — only
    /// addressable inside `DrawSolidQuads.quads`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct SolidQuad {
        pub x: f32,
        pub y: f32,
        pub width: f32,
        pub height: f32,
        pub color: [f32; 4],
    }

    /// `aether.render.draw_solid_quads` — draw a batch of flat-colored,
    /// alpha-blended quads in the projection `space` selects. Accumulated
    /// per frame with the same immediate-mode contract as
    /// `aether.draw_triangle`: send it every frame the quads should appear,
    /// or they vanish next frame. Reuses the textured-quad overlay pipeline
    /// with a reserved internal 1×1 white texture tinted by `color` — no
    /// new GPU pipeline. Fire-and-forget; no reply.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.draw_solid_quads")]
    pub struct DrawSolidQuads {
        pub space: QuadSpace,
        pub quads: Vec<SolidQuad>,
    }

    // ADR-0105 text surface. The `aether.text` capability composes the
    // textured-quad surface above into glyphs: load a TTF off the hot
    // path under a session-scoped `font_id`, then draw a string every
    // frame in immediate mode. Postcard-shaped; `space` reuses
    // `QuadSpace` so a screen-space HUD string and a world-anchored
    // label ride the same discriminant.

    /// `aether.text.load_font` — fetch a TTF through `aether.fs` and
    /// register it under a session-scoped `font_id` (assigned the same
    /// way ADR-0103 assigns instrument ids). `namespace` / `path` address
    /// the file the same way `aether.fs.read` does (e.g. `"assets"` /
    /// `"fonts/RobotoMono.ttf"`). The capability forwards the read,
    /// parses the font off the hot path, and replies `LoadFontResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.text.load_font")]
    pub struct LoadFont {
        pub namespace: String,
        pub path: String,
    }

    /// Reply to `LoadFont`. `Ok` carries the assigned `font_id` — thread
    /// it into `DrawText.font_id` — the derived `name` (the file stem),
    /// and `resident_bytes` (the parsed TTF's byte length). `Err` echoes
    /// the `namespace` / `path` for correlation plus a human-readable
    /// reason — a bad path, or a file fontdue could not parse as a font.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.text.load_font_result")]
    pub enum LoadFontResult {
        Ok {
            font_id: u32,
            name: String,
            resident_bytes: u64,
        },
        Err {
            namespace: String,
            path: String,
            error: String,
        },
    }

    /// `aether.text.draw` — lay out and draw `text` in the font named by
    /// `font_id` at `size_pixels`, every frame the string should appear
    /// (the same immediate-mode contract as `aether.draw_triangle`: send
    /// it each frame or it vanishes). `color` is a linear RGBA multiplier
    /// over the glyph coverage — the alpha channel scales the blend.
    /// `origin` is the screen-pixel top-left the string flows from along
    /// the baseline in `Screen` mode — `[0.0, 0.0]` is the window's
    /// top-left corner, the same as the pre-origin behavior. In `World`
    /// mode `origin` is ignored; the `anchor` positions the string there.
    /// `space` selects the projection: `Screen` flows the string from
    /// `origin` along the baseline; `World { anchor, scale }` anchors it
    /// in the scene. An unknown `font_id` warn-drops. Fire-and-forget; no
    /// reply.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.text.draw")]
    pub struct DrawText {
        pub font_id: u32,
        pub text: String,
        pub size_pixels: f32,
        pub color: [f32; 4],
        /// Screen-pixel top-left the string flows from in `Screen` mode.
        /// `[0.0, 0.0]` is the window's top-left corner. Ignored in
        /// `World` mode — the `anchor` positions there.
        pub origin: [f32; 2],
        pub space: QuadSpace,
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

    /// Reply to `SetMasterGain` (ADR-0039). `Ok` echoes the gain the
    /// substrate actually applied — values above `1.0` are clamped, so
    /// callers that sent `1.5` learn they got `1.0`. `Err` fires on
    /// chassis without an audio device (headless, hub) or when audio
    /// was disabled at boot via `AETHER_AUDIO_DISABLE`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.audio.set_master_gain_result")]
    pub enum SetMasterGainResult {
        Ok { applied_gain: f32 },
        Err { error: String },
    }

    // ADR-0104 scheduled note events. One `aether.audio.schedule` mail
    // carries a whole tune as a batch of timed note events; the audio cap
    // schedules them against its own sample clock so relative timing is
    // sample-accurate. Postcard-shaped — the batch is a `Vec`, not a
    // cast-eligible `#[repr(C)]` body.

    /// One note action in a scheduled batch (ADR-0104). The payload
    /// mirrors `note_on` / `note_off` exactly — a scheduled note allocates
    /// from the same voice pool, obeys the same steal policy, and keys
    /// note-off matching by the scheduling sender, as if the equivalent
    /// mail had arrived at the event's due instant.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum ScheduledNote {
        On {
            pitch: u8,
            velocity: u8,
            instrument_id: u8,
        },
        Off {
            pitch: u8,
            instrument_id: u8,
        },
    }

    /// A timed entry in an `aether.audio.schedule` batch (ADR-0104).
    /// `at_millis` is the play-at offset relative to the batch's arrival
    /// at the audio callback, so every event in one batch shares a single
    /// timebase and simultaneous events (a chord) stay aligned. Offsets
    /// run forward from receipt; there is no notion of a past due time.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct ScheduledEvent {
        pub at_millis: u32,
        pub event: ScheduledNote,
    }

    /// `aether.audio.schedule` — dispatch a batch of timed note events in
    /// a single mail (ADR-0104), so a melody plays with correct relative
    /// timing instead of collapsing into a cluster chord. The cap
    /// validates the batch synchronously — an events-per-batch cap and a
    /// horizon cap on `at_millis`, rejecting the whole batch atomically on
    /// any invalid entry — and replies `ScheduleResult` in-handler. The
    /// accepted batch crosses to the audio callback as one event; the
    /// synth converts each `at_millis` to an absolute due frame at receipt
    /// and fires the events sample-accurately inside its render loop.
    /// Desktop-only — chassis without an audio device reply `Err`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.audio.schedule")]
    pub struct Schedule {
        pub events: Vec<ScheduledEvent>,
    }

    /// Reply to `Schedule`. `Ok { accepted }` reports how many events the
    /// batch admitted — a score player can trust that an `Ok` batch plays
    /// in full, since validation is atomic. `Err` carries a human-readable
    /// reason — an over-cap batch size, an over-horizon `at_millis`, or a
    /// chassis without an audio device — loud rather than logged-and-
    /// dropped (ADR-0104).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.audio.schedule_result")]
    pub enum ScheduleResult {
        Ok { accepted: u32 },
        Err { error: String },
    }

    // ADR-0103 track playback. The audio cap plays a decoded audio asset
    // (music, ambience) in its own mixer lane, addressed by fs namespace
    // + path the way the rest of the substrate addresses files (ADR-0041).
    // Postcard-shaped because every field is a `String` / `f32` / `bool`,
    // not a cast-eligible `#[repr(C)]` body like `NoteOn`.

    /// `aether.audio.play_track` — fetch, decode, and play an audio asset
    /// through the audio cap. The cap forwards an `aether.fs.read` for
    /// `namespace://path`, decodes + resamples the bytes off the realtime
    /// path, and mixes the track in its own lane — never counted against
    /// the voice pool, never voice-stolen. `gain` is a linear per-track
    /// scalar applied at play time; `looping` wraps the track to its start
    /// on completion instead of retiring it. Re-playing the same
    /// `(sender, lane, namespace, path)` key restarts the track. Reply:
    /// `PlayTrackResult`. Desktop-only — chassis without an audio device
    /// reply `Err` (ADR-0103 §7).
    ///
    /// `lane` augments the track key so callers that share a source
    /// mailbox can each own a distinct track under the same
    /// `(namespace, path)`. Senders are distinguished by their envelope
    /// mailbox, but non-component senders — MCP sessions, substrate-
    /// internal mail — all collapse to one mailbox id, so two such callers
    /// would otherwise alias to a single track and stop or restart each
    /// other. Each passes its own `lane` string to stay isolated; `None`
    /// is exactly the unlaned behavior. Isolation is cooperative, not
    /// enforced — a sender that names another's `(sender, lane)` collides
    /// deliberately, which is the right strength inside one trust domain.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.audio.play_track")]
    pub struct PlayTrack {
        pub namespace: String,
        pub path: String,
        pub gain: f32,
        pub looping: bool,
        pub lane: Option<String>,
    }

    /// Reply to `PlayTrack`. Both arms echo the originating `lane` +
    /// `namespace` + `path` for correlation — a caller running several
    /// lanes over the same path tells the replies apart by `lane`. `Ok`
    /// fires once the asset has decoded and the track started in the mixer
    /// lane; `Err` carries a human-readable reason — a typo'd path (the fs
    /// error), a malformed / unsupported file (the decode error), or a
    /// chassis without an audio device. A bad path comes back loud rather
    /// than logged-and-dropped because it is the common agent failure
    /// (ADR-0103 §2).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.audio.play_track_result")]
    pub enum PlayTrackResult {
        Ok {
            namespace: String,
            path: String,
            lane: Option<String>,
        },
        Err {
            namespace: String,
            path: String,
            lane: Option<String>,
            error: String,
        },
    }

    /// `aether.audio.stop_track` — fade out and retire a track started by
    /// `PlayTrack`. Matched on `(sender, lane, namespace, path)` — the
    /// sender is taken from the mail envelope, not the payload — so one
    /// component cannot stop another's track. `lane` must match the value
    /// the `PlayTrack` carried (an unlaned track stops with `None`); it
    /// lets callers that share a source mailbox stop only their own lane.
    /// Releases through a short (~5 millisecond) linear fade to avoid a
    /// click. Stopping a track that isn't playing is a no-op, matching
    /// `note_off`. Fire-and-forget; no reply.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.audio.stop_track")]
    pub struct StopTrack {
        pub namespace: String,
        pub path: String,
        pub lane: Option<String>,
    }

    // ADR-0103 sampled instrument banks. The audio cap loads a bank of
    // pitched samples at runtime, appends it to the instrument registry
    // past the compiled-in built-ins, and plays it through the unchanged
    // `note_on` / `note_off` surface (a third voice kernel beside the
    // oscillator and partial-bank patches). Postcard-shaped — the request
    // carries `String` namespace/path, the reply a numeric id + name.

    /// `aether.audio.load_instrument` — load a sampled instrument bank
    /// from an `.sfz` file in an fs namespace. The cap fetches the `.sfz`
    /// through `aether.fs`, parses the SFZ subset (regions, key / velocity
    /// ranges, root pitch), fetches every WAV it references, decodes and
    /// resamples them off the realtime path, assembles the bank, and
    /// appends it to the registry at the next id past the built-ins. The
    /// assigned id rides the reply; a subsequent `note_on` with that id
    /// plays the sampled instrument. Loaded ids are session-scoped — they
    /// depend on load order and do not survive a restart (ADR-0103 §4).
    /// Reply: `LoadInstrumentResult`. Desktop-only — chassis without an
    /// audio device reply `Err` (ADR-0103 §7).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.audio.load_instrument")]
    pub struct LoadInstrument {
        pub namespace: String,
        pub path: String,
    }

    /// Reply to `LoadInstrument`. `Ok` carries the `instrument_id` the
    /// bank was assigned (thread it into `NoteOn.instrument_id` to play
    /// it), the `name` derived from the `.sfz` filename, and
    /// `resident_bytes` — the decoded PCM the bank holds resident, so an
    /// agent can see what a load is spending (no bank unload in v1, ADR-0103
    /// §4). `Err` echoes the originating `namespace` + `path` with a
    /// human-readable reason — a typo'd path (the fs error), a malformed
    /// `.sfz` or sample (the parse / decode error), or a chassis without
    /// an audio device — loud rather than logged-and-dropped (ADR-0103 §2).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.audio.load_instrument_result")]
    pub enum LoadInstrumentResult {
        Ok {
            instrument_id: u8,
            name: String,
            resident_bytes: u64,
        },
        Err {
            namespace: String,
            path: String,
            error: String,
        },
    }

    // ADR-0041 substrate file I/O. Four request kinds on the
    // `"aether.fs"` mailbox (read / write / delete / list), paired
    // 1:1 with reply kinds
    // that carry a structured `FsError` on failure. All postcard-
    // shaped because every request carries String namespace/path
    // fields and writes carry `Vec<u8>` bytes.
    //
    // `namespace` is the logical prefix without the `://`: mail
    // carries `"save"`, not `"save://"`. Paths are relative to the
    // namespace root; `..` and absolute prefixes are rejected at the
    // adapter boundary as `FsError::Forbidden`.

    /// Structured failure reason for an I/O request (ADR-0041 §1).
    /// Components can pattern-match on the variant to decide whether
    /// to retry (`AdapterError`), prompt the user (`NotFound`), or
    /// surface a bug (`Forbidden` / `UnknownNamespace`). `AdapterError`
    /// preserves backend-specific detail as free-form text — e.g.
    /// permission-denied text from the OS, an HTTP status from a
    /// future cloud adapter — without locking the enum shape to any
    /// one backend.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum FsError {
        NotFound,
        Forbidden,
        UnknownNamespace,
        AdapterError(String),
    }

    /// `aether.fs.read` — request the substrate read a file and reply
    /// with its bytes. Mailed to the `"aether.fs"` mailbox; reply
    /// lands via `reply_mail` as `ReadResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.fs.read")]
    pub struct Read {
        pub namespace: String,
        pub path: String,
    }

    /// Reply to `Read`. Both arms echo the `namespace` + `path` from
    /// the originating `Read` so the caller can correlate the reply
    /// to its source request without threading a pending-op queue or
    /// allocating correlation ids — operation identity comes from the
    /// reply kind itself (`aether.fs.read_result`), target identity
    /// from the echoed fields. `Ok` carries the full file contents;
    /// `Err` carries an `FsError` variant.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.fs.read_result")]
    pub enum ReadResult {
        Ok {
            namespace: String,
            path: String,
            bytes: Vec<u8>,
        },
        Err {
            namespace: String,
            path: String,
            error: FsError,
        },
    }

    /// `aether.fs.write` — request the substrate write `bytes` to
    /// `namespace://path`. v1's local-file adapter stages to a
    /// temporary sibling and `rename`s on success so a crash
    /// mid-write leaves either the old contents or the new, never a
    /// torn file. Reply: `WriteResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.fs.write")]
    pub struct Write {
        pub namespace: String,
        pub path: String,
        pub bytes: Vec<u8>,
    }

    /// Reply to `Write`. Both arms echo `namespace` + `path` for
    /// correlation; the request's `bytes` field is *not* echoed so the
    /// reply payload stays small even when the write was megabytes
    /// (correlation needs the identity of the write, not its contents).
    /// `Err` carries an `FsError` — `Forbidden` for read-only
    /// namespaces (e.g. `assets://`), `AdapterError` for disk-full /
    /// permission / rename failures.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.fs.write_result")]
    pub enum WriteResult {
        Ok {
            namespace: String,
            path: String,
        },
        Err {
            namespace: String,
            path: String,
            error: FsError,
        },
    }

    /// `aether.fs.delete` — request the substrate remove a file.
    /// Missing files surface as `NotFound` (not silent success) so
    /// callers that care about the distinction can tell; callers
    /// that don't ignore it. Reply: `DeleteResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.fs.delete")]
    pub struct Delete {
        pub namespace: String,
        pub path: String,
    }

    /// Reply to `Delete`. Both arms echo `namespace` + `path` for
    /// correlation. `Ok` on successful removal; `Err` on any
    /// adapter-reported failure, including `NotFound` for a file that
    /// wasn't there to delete.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.fs.delete_result")]
    pub enum DeleteResult {
        Ok {
            namespace: String,
            path: String,
        },
        Err {
            namespace: String,
            path: String,
            error: FsError,
        },
    }

    /// `aether.fs.list` — enumerate entries under `prefix` in
    /// `namespace`. Shallow (no recursion) and prefix-filtered —
    /// callers that want a tree walk paginate themselves. Empty
    /// `prefix` lists the namespace root. Reply: `ListResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.fs.list")]
    pub struct List {
        pub namespace: String,
        pub prefix: String,
    }

    /// Reply to `List`. Both arms echo the originating `namespace` +
    /// `prefix` for correlation. `Ok` carries the matching entry
    /// names — bare file/dir names, not fully-qualified paths — so the
    /// caller composes `{prefix}{entry}` when turning an entry back
    /// into a read. Empty `entries` means "namespace exists, nothing
    /// matched"; `Err { UnknownNamespace }` means the namespace itself
    /// wasn't registered.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.fs.list_result")]
    pub enum ListResult {
        Ok {
            namespace: String,
            prefix: String,
            entries: Vec<String>,
        },
        Err {
            namespace: String,
            prefix: String,
            error: FsError,
        },
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

    /// *How many* instances a [`TemplateEntryWire`] family can have — the
    /// wire mirror of `aether_data::name_inventory::Cardinality` (ADR-0088
    /// §4 v2). Orthogonal to [`ParamKindWire`] (the *shape* axis): the
    /// client expands / prehashes templates off `param`, while
    /// `cardinality` is the self-describing "how many" the manifest
    /// surfaces so a consumer reads "trampoline = one mailbox per loaded
    /// component" rather than an opaque `Dynamic` family. Struct variants
    /// (not tuple) so the wire JSON carries named fields, matching
    /// [`ParamKindWire`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.inventory.cardinality")]
    pub enum CardinalityWire {
        /// A compile-time-known finite instance bound (`aether-worker-{N}`
        /// prehashes `count` instantiations).
        Bounded { count: u64 },
        /// One instance per live entity of the named kind — the
        /// relationship the four instanced actors carry (`"component"`,
        /// `"connection"`, `"listener"`, `"engine"`).
        OnePer { entity: String },
        /// Open-ended, runtime-minted, no fixed relationship
        /// (`aether-instanced-{full_name}`).
        Unbounded,
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
    /// axis) says how it is filled and [`CardinalityWire`] (the how-many
    /// axis) says how many instances exist. Preserving the template
    /// (rather than its expansion) keeps the family shape so the client
    /// can declare "ids in this family exist and look like *this*" even
    /// for `Dynamic` families it cannot enumerate; `cardinality` makes
    /// that declaration self-describing.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct TemplateEntryWire {
        pub domain: Vec<u8>,
        pub template: String,
        pub param: ParamKindWire,
        pub cardinality: CardinalityWire,
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
    /// kind; `name` is its declared `Kind::NAME`; `schema_postcard` is
    /// the kind's [`SchemaType`](aether_data::SchemaType) postcard-
    /// serialized (the wire enum carries the full nominal shape).
    ///
    /// The schema rides as opaque postcard bytes rather than a directly
    /// embedded `SchemaType` because `SchemaType` itself has no
    /// `Schema` impl (it *is* the schema vocabulary, not a value in
    /// it); shipping it as `Bytes` keeps `KindDescriptorWire` and the
    /// whole reply derivable via [`aether_data::Schema`] without a
    /// hand-roll, at the cost of one extra `postcard::from_bytes` on
    /// the harness side. Cap encodes via `postcard::to_allocvec`
    /// against `descriptor.schema`; client decodes via
    /// `postcard::from_bytes`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct KindDescriptorWire {
        pub id: aether_data::KindId,
        pub name: String,
        pub schema_postcard: Vec<u8>,
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

    // ADR-0043 substrate HTTP egress. One request kind + one reply
    // kind on the `"aether.http"` sink, plus supporting `HttpMethod`,
    // `HttpHeader`, and `HttpError` shapes. All postcard-shaped
    // (Strings, Vecs, Option<u32>).
    //
    // Reply correlation follows the ADR-0041 pattern: the reply
    // echoes the originating `url` so callers match reply-to-request
    // without threading a pending-op queue. Request `body` is not
    // echoed — correlation needs the identity of the request, not
    // its contents, and a multi-MB upload should not round-trip its
    // bytes. Components needing strict per-op correlation (same URL
    // fired back-to-back, non-idempotent POST) lean on ADR-0042's
    // per-Source correlation ids via `prev_correlation_p32` rather
    // than a per-kind field.

    /// HTTP method carried on `Fetch`. Enumerating at the schema
    /// layer keeps `"get"` / `"GET"` / `"Get"` from disagreeing
    /// across guests; the substrate maps each variant to its
    /// canonical uppercase name when calling the HTTP backend.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum HttpMethod {
        Get,
        Post,
        Put,
        Delete,
        Patch,
        Head,
        Options,
    }

    /// One HTTP header on a `Fetch` request or `FetchResult`
    /// response. Expressed as a named-field struct because
    /// `aether_data::Schema` has no blanket impl for tuples — if
    /// that lands later the wire shape here is source-compatible
    /// (same two fields in the same order).
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct HttpHeader {
        pub name: String,
        pub value: String,
    }

    /// Structured failure reason for an HTTP request (ADR-0043 §1).
    /// Typed variants cover the branches agents routinely need to
    /// match on — `Timeout` → retry, `AllowlistDenied` → config
    /// issue, `BodyTooLarge` → chunk the response, `Disabled` →
    /// surface to the operator. `InvalidUrl` carries the offending
    /// URL text; `AdapterError` is the catchall preserving backend-
    /// specific detail (DNS failure, TLS handshake, connection
    /// refused, etc.) as free-form text.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum HttpError {
        InvalidUrl(String),
        Timeout,
        BodyTooLarge,
        AllowlistDenied,
        Disabled,
        AdapterError(String),
    }

    /// `aether.http.fetch` — request the substrate perform an HTTP
    /// request and reply with the response. Mailed to the
    /// `"aether.http"` sink; reply lands via `reply_mail` as
    /// `FetchResult`.
    /// `timeout_ms` overrides the chassis default
    /// (`AETHER_HTTP_TIMEOUT_MS`, default 30000) when set; `None`
    /// uses the default.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.http.fetch")]
    pub struct Fetch {
        pub url: String,
        pub method: HttpMethod,
        pub headers: Vec<HttpHeader>,
        pub body: Vec<u8>,
        pub timeout_ms: Option<u32>,
    }

    /// Reply to `Fetch`. Both arms echo the originating `url` so the
    /// caller correlates reply-to-request without threading a
    /// pending-op queue — operation identity comes from the reply
    /// kind itself (`aether.http.fetch_result`). Request `body` is
    /// deliberately not echoed: correlation needs the identity of
    /// the request, not its contents, and a multi-MB upload should
    /// not round-trip. `Ok` carries the HTTP status, response
    /// headers, and response body (bounded by
    /// `AETHER_HTTP_MAX_BODY_BYTES`, default 16MB); `Err` carries an
    /// `HttpError` variant.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.http.fetch_result")]
    pub enum FetchResult {
        Ok {
            url: String,
            status: u16,
            headers: Vec<HttpHeader>,
            body: Vec<u8>,
        },
        Err {
            url: String,
            error: HttpError,
        },
    }

    // ADR-0108 HTTP server kinds. Two public kinds shared by the server
    // capability (#1760) and the handler component (#1762): an inbound
    // request delivered to the handler, and an outbound response returned
    // by the handler. Both reuse `HttpMethod` / `HttpHeader` from ADR-0043
    // so the inbound vocabulary is symmetric with the client.

    /// Inbound HTTP request delivered to a handler component by the server
    /// capability (ADR-0108). `query` is always present — empty string when
    /// the URL carries no query component. `body` is raw bytes so binary
    /// uploads round-trip without loss.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.http.server.request")]
    pub struct HttpServerRequest {
        pub method: HttpMethod,
        pub path: String,
        pub query: String,
        pub headers: Vec<HttpHeader>,
        pub body: Vec<u8>,
    }

    /// Outbound HTTP response produced by a handler component and forwarded
    /// to the waiting client by the server capability (ADR-0108). `status`
    /// is the raw HTTP status code; `body` is raw bytes so binary responses
    /// round-trip without loss.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.http.server.response")]
    pub struct HttpServerResponse {
        pub status: u16,
        pub headers: Vec<HttpHeader>,
        pub body: Vec<u8>,
    }

    /// `aether.http.server.inbound_ready` — accept / reader sidecar →
    /// `HttpServerCapability` dispatcher wake (ADR-0108, issue 1760).
    /// The HTTP-server analog of `RpcInboundReady`: the sidecar pushes
    /// the live work (an accepted `TcpStream`, a parsed request, a close
    /// reason) over the cap's internal mpsc and fires this empty-payload
    /// mail at the cap's own mailbox so the dispatcher handler drains the
    /// queue. A `TcpStream` isn't wire-shaped and a request body may be
    /// large, so the mail is only the wakeup signal.
    #[derive(
        aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default,
    )]
    #[kind(name = "aether.http.server.inbound_ready")]
    pub struct HttpInboundReady {}

    // ADR-0045 typed-handle store. Four request kinds on the
    // `"aether.handle"` sink (`publish` / `release` / `pin` / `unpin`),
    // paired 1:1 with reply kinds. Components mail `HandlePublish`
    // with `kind_id` + payload bytes and receive a fresh ephemeral
    // handle id back in `HandlePublishResult::Ok`; subsequent mail
    // can carry the handle on the wire as `Ref::Handle { id,
    // kind_id }`. The substrate's dispatch path resolves the handle
    // to its `Ref::Inline` form before delivery.
    //
    // Mail rather than host fns: keeps the privileged FFI surface
    // small (ADR-0002), folds capability gating (ADR-0044) into
    // the existing per-sink permission model, gives Claude
    // observability into handle traffic for free.
    //
    // Reply correlation echoes the operation's identity: `publish`
    // echoes `kind_id`; `release` / `pin` / `unpin` echo `id`. v1
    // semantics are mostly idempotent — `release` past zero
    // saturates, `pin` of a pinned entry is a no-op — so the only
    // real failure surface is `UnknownHandle` for ops on a missing
    // id.

    /// Structured failure reason for a handle operation. Mirrors
    /// `FsError` / `HttpError`'s tagged-enum shape so guests can
    /// pattern-match on the variant rather than parsing strings.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum HandleError {
        /// No handle entry under the requested id. Surfaces from
        /// `release` / `pin` / `unpin` against an id the substrate
        /// has never seen (or has already evicted).
        UnknownHandle,
        /// Eviction couldn't free enough room for the publish —
        /// every existing entry is pinned or refcounted at the
        /// store's byte cap.
        EvictionFailed,
        /// The substrate has no handle store wired (e.g. a
        /// chassis without handle support). Treated as fatal by
        /// the SDK; callers see `Ctx::publish` return `None`.
        NoStore,
        /// Free-form adapter detail — kind-id mismatch on
        /// re-publish, internal state, etc. Free-form text for
        /// the same reasons `FsError::AdapterError` is.
        AdapterError(String),
    }

    /// `aether.handle.publish` — request the substrate stash
    /// `bytes` in the handle store under `kind_id` and reply with
    /// a fresh ephemeral id. Mailed to the `"aether.handle"` sink;
    /// reply lands as `HandlePublishResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.publish")]
    pub struct HandlePublish {
        pub kind_id: aether_data::KindId,
        pub bytes: Vec<u8>,
    }

    /// Reply to `HandlePublish`. Both arms echo the originating
    /// `kind_id` for correlation; `Ok` carries the minted `id`.
    /// The request's `bytes` aren't echoed — correlation needs the
    /// identity of the publish, not its contents.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.publish_result")]
    pub enum HandlePublishResult {
        Ok {
            kind_id: aether_data::KindId,
            id: aether_data::HandleId,
        },
        Err {
            kind_id: aether_data::KindId,
            error: HandleError,
        },
    }

    /// `aether.handle.release` — drop one reference on `id`. Reply:
    /// `HandleReleaseResult`. The substrate's `dec_ref` saturates
    /// at zero, so calling release on an already-released handle
    /// is a no-op success rather than `UnknownHandle`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.release")]
    pub struct HandleRelease {
        pub id: aether_data::HandleId,
    }

    /// Reply to `HandleRelease`. Both arms echo the originating
    /// `id`. `Err` only fires when no entry exists at that id.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.release_result")]
    pub enum HandleReleaseResult {
        Ok {
            id: aether_data::HandleId,
        },
        Err {
            id: aether_data::HandleId,
            error: HandleError,
        },
    }

    /// `aether.handle.pin` — protect `id` from LRU eviction even
    /// when its refcount drops to zero. Reply: `HandlePinResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.pin")]
    pub struct HandlePin {
        pub id: aether_data::HandleId,
    }

    /// Reply to `HandlePin`. Both arms echo the originating `id`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.pin_result")]
    pub enum HandlePinResult {
        Ok {
            id: aether_data::HandleId,
        },
        Err {
            id: aether_data::HandleId,
            error: HandleError,
        },
    }

    /// `aether.handle.unpin` — clear the pinned flag on `id`.
    /// Doesn't drop the entry; only makes it eligible for LRU
    /// eviction once `refcount == 0`. Reply: `HandleUnpinResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.unpin")]
    pub struct HandleUnpin {
        pub id: aether_data::HandleId,
    }

    /// Reply to `HandleUnpin`. Both arms echo the originating `id`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.unpin_result")]
    pub enum HandleUnpinResult {
        Ok {
            id: aether_data::HandleId,
        },
        Err {
            id: aether_data::HandleId,
            error: HandleError,
        },
    }

    /// `aether.handle.describe` — ask the substrate's `HandleCapability`
    /// for a summary of the persistent store (ADR-0049 §10). Reply:
    /// `HandleDescribeResult`. `max` caps the top-N lists; the cap
    /// clamps it to a sane ceiling.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.describe")]
    pub struct HandleDescribe {
        pub max: u32,
    }

    /// One handle's summary line in a `HandleDescribeResult`. Carries
    /// the identity + size + durability fields the operator triages on.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct HandleSummary {
        pub handle_id: aether_data::HandleId,
        pub kind_id: aether_data::KindId,
        pub bytes_len: u32,
        pub pinned: bool,
        pub refcount: u32,
        pub created_at_ms: u64,
    }

    /// Reply to `HandleDescribe` — the store summary (ADR-0049 §10).
    /// `top_by_size` is descending by `bytes_len`; `top_by_recency` is
    /// descending by `created_at_ms`. Both are capped at the request's
    /// (clamped) `max`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.describe_result")]
    pub struct HandleDescribeResult {
        pub total_entries: u32,
        pub in_memory_entries: u32,
        pub on_disk_entries: u32,
        pub pinned_entries: u32,
        pub in_memory_bytes: u64,
        pub on_disk_bytes: u64,
        pub on_disk_budget_bytes: u64,
        pub top_by_size: Vec<HandleSummary>,
        pub top_by_recency: Vec<HandleSummary>,
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
    // `TopdownParams` / `ModeInit`) moved to the `aether-camera` trunk
    // crate. The `aether.camera` view_proj sink contract above stays
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
    /// rather than a `HashMap` because postcard-shaped wire kinds
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
    // cap (issue 1014) exposes two sibling text-completion request
    // kinds — `messages.send` (HTTPS to the official Messages API) and
    // `cli.send` (the local `claude` subprocess against the user's
    // subscription) — with identical input schemas; the routing choice
    // is the visible kind name, not an opaque adapter detail. Both
    // reply with a `*_result` Ok/Err enum carrying the shared `Usage`
    // accounting (also consumed by the `aether.gemini` media kinds,
    // issue 1015) on `Ok` and a provider-specific `AnthropicError` on
    // `Err`. All postcard-shaped — every request carries `String` /
    // `Vec` / `Option` fields.

    /// Conversation role on a [`Message`]. The Messages API only
    /// distinguishes user vs assistant turns; `system` rides as a
    /// separate top-level field on the request, not a role.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Role {
        User,
        Assistant,
    }

    /// One turn in an Anthropic completion request. `content` is the
    /// flat text of the turn (v1 doesn't model multi-part content
    /// blocks); `role` distinguishes user from assistant.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct Message {
        pub role: Role,
        pub content: String,
    }

    /// Token + wall-clock accounting returned on a successful
    /// content-gen completion. Shared across the Anthropic text kinds
    /// (issue 1014) and the Gemini media kinds (issue 1015). The CLI
    /// backend can only report `wall_clock_ms` (the subprocess gives no
    /// token counts), leaving the token / cost fields zero / `None`;
    /// the Messages API and the Gemini APIs populate the rest where the
    /// provider reports them.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Usage {
        pub input_tokens: u32,
        pub output_tokens: u32,
        pub wall_clock_ms: u32,
        pub cost_micros: Option<u64>,
    }

    /// Structured failure reason for an Anthropic completion (ADR-0050
    /// §1). Typed variants cover the branches a caller routinely
    /// matches on — `Overloaded` / `RateLimited` → back off,
    /// `ContextLengthExceeded` → trim the prompt, `Unauthorized` →
    /// config issue, `ContentPolicyRefused` → surface to the user,
    /// `CliNotFound` → the `claude` binary isn't on PATH,
    /// `UnknownModel` → typo / unsupported id,
    /// `Timeout` → a backend call (notably the `claude` subprocess)
    /// exceeded the cap's per-request deadline and the child was killed.
    /// `ParamNotSupported` → the request set a knob the backend has no
    /// way to honor (e.g. `max_tokens` / `temperature` on the CLI path,
    /// which the `claude` binary exposes no flag for — reject rather than
    /// silently drop). `AdapterError` is the catchall preserving
    /// backend-specific detail as free-form text.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum AnthropicError {
        Overloaded,
        RateLimited {
            retry_after_ms: Option<u32>,
        },
        ContextLengthExceeded {
            limit: u32,
        },
        Unauthorized,
        ContentPolicyRefused,
        CliNotFound,
        UnknownModel {
            model: String,
            supported: Vec<String>,
        },
        Timeout {
            elapsed_ms: u32,
        },
        ParamNotSupported {
            param: String,
            reason: String,
        },
        AdapterError(String),
    }

    /// `aether.anthropic.messages.send` — request a text completion via
    /// the official Anthropic Messages API (HTTPS). Mailed to the
    /// `"aether.anthropic"` mailbox; reply lands as
    /// `MessagesSendResult`. `request_id` correlates the reply
    /// (caller-minted, echoed on both arms). `model` selects the
    /// Messages model; `max_tokens` / `temperature` / `system` are the
    /// usual completion knobs.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.anthropic.messages.send")]
    pub struct MessagesSend {
        pub request_id: u64,
        pub model: String,
        pub messages: Vec<Message>,
        pub max_tokens: Option<u32>,
        pub temperature: Option<f32>,
        pub system: Option<String>,
    }

    /// `aether.anthropic.cli.send` — request a text completion via the
    /// local `claude` subprocess (the user's subscription rail).
    /// Identical input schema to [`MessagesSend`]; the routing choice
    /// is the kind name. Reply lands as `CliSendResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.anthropic.cli.send")]
    pub struct CliSend {
        pub request_id: u64,
        pub model: String,
        pub messages: Vec<Message>,
        pub max_tokens: Option<u32>,
        pub temperature: Option<f32>,
        pub system: Option<String>,
    }

    /// Reply to [`MessagesSend`]. Both arms echo the originating
    /// `request_id` for correlation. `Ok` carries the completion text,
    /// the model the provider actually served, and `Usage` accounting;
    /// `Err` carries an `AnthropicError`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.anthropic.messages.send_result")]
    pub enum MessagesSendResult {
        Ok {
            request_id: u64,
            text: String,
            model_used: String,
            usage: Usage,
        },
        Err {
            request_id: u64,
            error: AnthropicError,
        },
    }

    /// Reply to [`CliSend`]. Same shape as [`MessagesSendResult`]; the
    /// CLI backend populates only `Usage.wall_clock_ms` (the subprocess
    /// reports no token counts).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.anthropic.cli.send_result")]
    pub enum CliSendResult {
        Ok {
            request_id: u64,
            text: String,
            model_used: String,
            usage: Usage,
        },
        Err {
            request_id: u64,
            error: AnthropicError,
        },
    }

    // ADR-0050 `aether.gemini` cap (issue 1015). Media generation only
    // — image via Nano Banana, music via Lyria; no text completion (the
    // user defaults to the Claude CLI per ADR-0050 §3). Two request
    // kinds on the `aether.gemini` mailbox, each replying with a
    // `*_result` Ok/Err enum carrying the shared `Usage` accounting on
    // `Ok` and a provider-specific `GeminiError` on `Err`. Generated
    // binary bytes never ride the wire: the reply carries a
    // `save://gen/<uuid>.{png,wav}` path. The image schema is fixed by
    // a 2026-05 API survey; per-model validation absorbs vendor drift.

    /// Aspect ratio for a Nano Banana image. The cross-model set covers
    /// `ASPECT_RATIO_1_1` … `ASPECT_RATIO_21_9`; the `ASPECT_RATIO_1_4` /
    /// `ASPECT_RATIO_1_8` / `ASPECT_RATIO_4_1` / `ASPECT_RATIO_8_1`
    /// extreme ratios are NB2-only and rejected on older models by the
    /// adapter's per-model validation.
    // Variant names mirror the provider's `W:H` aspect-ratio labels
    // verbatim (`ASPECT_RATIO_16_9` = 16:9) so the wire vocabulary reads
    // the same as the API survey; the `WxH`-camel form (`Ar16x9`) would
    // obscure the mapping for the LLM caller building these.
    #[allow(non_camel_case_types)]
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AspectRatio {
        ASPECT_RATIO_1_1,
        ASPECT_RATIO_2_3,
        ASPECT_RATIO_3_2,
        ASPECT_RATIO_3_4,
        ASPECT_RATIO_4_3,
        ASPECT_RATIO_4_5,
        ASPECT_RATIO_5_4,
        ASPECT_RATIO_9_16,
        ASPECT_RATIO_16_9,
        ASPECT_RATIO_21_9,
        ASPECT_RATIO_1_4,
        ASPECT_RATIO_1_8,
        ASPECT_RATIO_4_1,
        ASPECT_RATIO_8_1,
    }

    /// Output image size for a Nano Banana image. `S512` is NB2-only;
    /// `K1` is supported by every model; `K2` / `K4` by NB Pro and NB2
    /// (not the legacy NB1). The adapter enforces the per-model matrix.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ImageSize {
        S512,
        K1,
        K2,
        K4,
    }

    /// Reasoning-effort knob for Nano Banana 2. `Minimal` / `High`;
    /// rejected on older models by per-model validation.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ThinkingLevel {
        Minimal,
        High,
    }

    /// Grounding metadata returned when `use_grounding=true` — the
    /// search queries and source URLs the model consulted. Free-form
    /// strings; the shape mirrors the provider's grounding payload
    /// without locking the cap to a specific schema version.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct GroundingMetadata {
        pub search_queries: Vec<String>,
        pub source_urls: Vec<String>,
    }

    /// Structured failure reason for a Gemini media generation
    /// (ADR-0050 §1). `RateLimited` / `ContentPolicyRefused` /
    /// `Unauthorized` mirror the Anthropic taxonomy; the
    /// `*NotSupportedByModel` variants carry the rejected value plus the
    /// model's supported set so the caller can correct and retry, and
    /// `MissingRequiredField` names a per-model required field the
    /// request omitted. `AdapterError` is the free-form catchall.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum GeminiError {
        RateLimited {
            retry_after_ms: Option<u32>,
        },
        ContentPolicyRefused,
        Unauthorized,
        UnknownModel {
            model: String,
            supported: Vec<String>,
        },
        AspectRatioNotSupportedByModel {
            model: String,
            aspect_ratio: AspectRatio,
            supported: Vec<AspectRatio>,
        },
        ImageSizeNotSupportedByModel {
            model: String,
            image_size: ImageSize,
            supported: Vec<ImageSize>,
        },
        MissingRequiredField {
            model: String,
            field: String,
        },
        AdapterError(String),
    }

    /// `aether.gemini.nanobanana.generate` — request an image from the
    /// Nano Banana family. `model` selects `gemini-2.5-flash-image` /
    /// `gemini-3-pro-image-preview` / `gemini-3.1-flash-image-preview`
    /// (NB2, the default). Reference inputs arrive as file paths the cap
    /// reads before dispatch. Per-model validation of `aspect_ratio` /
    /// `image_size` / reference-path counts runs before any network
    /// dispatch. Reply: `NanobananaGenerateResult` carrying a staged
    /// `save://gen/<uuid>.png` path.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.gemini.nanobanana.generate")]
    pub struct NanobananaGenerate {
        pub request_id: u64,
        pub model: String,
        pub prompt: String,
        pub aspect_ratio: AspectRatio,
        pub image_size: Option<ImageSize>,
        pub thinking_level: Option<ThinkingLevel>,
        pub include_thoughts: Option<bool>,
        pub object_reference_paths: Vec<String>,
        pub character_reference_paths: Vec<String>,
        pub use_grounding: Option<bool>,
        /// Opt-in / default-off. `None` / `Some(false)` clears the
        /// `thought_signature` from the reply (a signature can run to
        /// multiple MB and dominate the result); `Some(true)` retains it
        /// for a multi-turn continuation. Cross-model (Pro emits a
        /// signature too); gates only the reply populate, not validation.
        pub include_thought_signature: Option<bool>,
    }

    /// Reply to [`NanobananaGenerate`]. Both arms echo `request_id`.
    /// `Ok` carries the staged image path (never inline bytes), the
    /// model served, `Usage`, the NB2 `thought_signature` (passed back
    /// unchanged for multi-turn), and grounding metadata when
    /// `use_grounding=true`. `Err` carries a `GeminiError`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.gemini.nanobanana.generate_result")]
    pub enum NanobananaGenerateResult {
        Ok {
            request_id: u64,
            output_path: String,
            model_used: String,
            usage: Usage,
            thought_signature: Option<String>,
            grounding: Option<GroundingMetadata>,
        },
        Err {
            request_id: u64,
            error: GeminiError,
        },
    }

    /// `aether.gemini.lyria.generate` — request music from the Lyria
    /// family (snapshot 2026-05-20 of the Vertex AI Lyria API). `model`
    /// selects `lyria-2` / `lyria-3` / `lyria-3-pro`. `seed` and
    /// `sample_count` are mutually exclusive — the adapter rejects
    /// both-set. Each clip is a fixed ~30s WAV at 48 kHz; there is no
    /// `duration_s`. Reply: `LyriaGenerateResult` carrying one staged
    /// `save://gen/<uuid>.wav` path per generated clip.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.gemini.lyria.generate")]
    pub struct LyriaGenerate {
        pub request_id: u64,
        pub model: String,
        pub prompt: String,
        pub negative_prompt: Option<String>,
        pub seed: Option<u32>,
        pub sample_count: Option<u32>,
    }

    /// Reply to [`LyriaGenerate`]. Both arms echo `request_id`. `Ok`
    /// carries one staged WAV path per clip (`sample_count` controls
    /// the count, hence the plural `output_paths`), the model served,
    /// and `Usage`. `Err` carries a `GeminiError`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.gemini.lyria.generate_result")]
    pub enum LyriaGenerateResult {
        Ok {
            request_id: u64,
            output_paths: Vec<String>,
            model_used: String,
            usage: Usage,
        },
        Err {
            request_id: u64,
            error: GeminiError,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::{Kind, decode, decode_slice, encode, encode_slice};
    #[test]
    fn key_roundtrip() {
        let k = Key { code: 42 };
        let bytes = encode(&k);
        assert_eq!(bytes.len(), 4);
        let back: Key = decode(&bytes).expect("test setup: Key cast round-trip decodes");
        assert_eq!(back, k);
    }

    #[test]
    fn mouse_move_roundtrip() {
        let m = MouseMove { x: 1.5, y: -3.25 };
        let bytes = encode(&m);
        assert_eq!(bytes.len(), 8);
        let back: MouseMove =
            decode(&bytes).expect("test setup: MouseMove cast round-trip decodes");
        assert_eq!(back, m);
    }

    #[test]
    fn spawn_engine_roundtrip_carries_boot_manifest() {
        // `boot_manifest` rides the wire (postcard path — non-`repr(C)`
        // struct with `Vec<String>` + `Option<String>`); both `Some`
        // (a spawn carrying a component list) and `None` (a bare spawn)
        // must survive the engines-cap encode/decode.
        use alloc::string::ToString;
        use alloc::vec;

        let spawn = SpawnEngine {
            binary_path: "/abs/aether-substrate-headless".to_string(),
            args: vec!["--tick-hz".to_string(), "30".to_string()],
            boot_manifest: Some("/tmp/aether-boot-manifest.json".to_string()),
        };
        let back = SpawnEngine::decode_from_bytes(&spawn.encode_into_bytes())
            .expect("test setup: SpawnEngine decodes");
        assert_eq!(back.binary_path, spawn.binary_path);
        assert_eq!(back.args, spawn.args);
        assert_eq!(
            back.boot_manifest.as_deref(),
            Some("/tmp/aether-boot-manifest.json"),
        );

        let bare = SpawnEngine {
            binary_path: "/abs/aether-substrate".to_string(),
            args: vec![],
            boot_manifest: None,
        };
        let back = SpawnEngine::decode_from_bytes(&bare.encode_into_bytes())
            .expect("test setup: bare SpawnEngine decodes");
        assert_eq!(back.boot_manifest, None);
    }

    #[test]
    fn draw_triangle_slice_size() {
        let v = Vertex {
            x: 0.0,
            y: 0.5,
            z: 0.0,
            r: 1.0,
            g: 0.0,
            b: 0.0,
        };
        let tris = [
            DrawTriangle { verts: [v, v, v] },
            DrawTriangle { verts: [v, v, v] },
        ];
        let bytes = encode_slice(&tris);
        assert_eq!(bytes.len(), 2 * 72);
        let back: &[DrawTriangle] =
            decode_slice(&bytes).expect("test setup: DrawTriangle slice decodes zero-copy");
        assert_eq!(back, &tris);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn kind_names_are_stable() {
        assert_eq!(Tick::NAME, "aether.lifecycle.tick");
        assert_eq!(InitCaps::NAME, "aether.lifecycle.init_caps");
        assert_eq!(InitComponents::NAME, "aether.lifecycle.init_components");
        assert_eq!(Render::NAME, "aether.lifecycle.render");
        assert_eq!(Present::NAME, "aether.lifecycle.present");
        assert_eq!(Shutdown::NAME, "aether.lifecycle.shutdown");
        assert_eq!(Quit::NAME, "aether.lifecycle.quit");
        assert_eq!(LifecycleAdvance::NAME, "aether.lifecycle.advance");
        assert_eq!(
            LifecycleAdvanceComplete::NAME,
            "aether.lifecycle.advance_complete"
        );
        assert_eq!(LifecycleSubscribe::NAME, "aether.lifecycle.subscribe");
        assert_eq!(
            LifecycleSubscribeSelf::NAME,
            "aether.lifecycle.subscribe_self"
        );
        assert_eq!(LifecycleUnsubscribe::NAME, "aether.lifecycle.unsubscribe");
        assert_eq!(
            LifecycleUnsubscribeSelf::NAME,
            "aether.lifecycle.unsubscribe_self"
        );
        assert_eq!(
            LifecycleUnsubscribeAll::NAME,
            "aether.lifecycle.unsubscribe_all"
        );
        assert_eq!(
            LifecycleSubscribeResult::NAME,
            "aether.lifecycle.subscribe_result"
        );
        assert_eq!(Key::NAME, "aether.key");
        assert_eq!(KeyRelease::NAME, "aether.key_release");
        assert_eq!(MouseButton::NAME, "aether.mouse_button");
        assert_eq!(MouseMove::NAME, "aether.mouse_move");
        assert_eq!(DrawTriangle::NAME, "aether.draw_triangle");
        assert_eq!(Ping::NAME, "aether.ping");
        assert_eq!(Pong::NAME, "aether.pong");
        assert_eq!(LoadComponent::NAME, "aether.component.load");
        assert_eq!(ReplaceComponent::NAME, "aether.component.replace");
        assert_eq!(DropComponent::NAME, "aether.component.drop");
        assert_eq!(LoadResult::NAME, "aether.component.load_result");
        assert_eq!(DropResult::NAME, "aether.component.drop_result");
        assert_eq!(ReplaceResult::NAME, "aether.component.replace_result");
        assert_eq!(SubscribeInput::NAME, "aether.input.subscribe");
        assert_eq!(SubscribeInputSelf::NAME, "aether.input.subscribe_self");
        assert_eq!(UnsubscribeInput::NAME, "aether.input.unsubscribe");
        assert_eq!(UnsubscribeInputSelf::NAME, "aether.input.unsubscribe_self");
        assert_eq!(SubscribeInputResult::NAME, "aether.input.subscribe_result");
        assert_eq!(CaptureFrame::NAME, "aether.render.capture_frame");
        assert_eq!(
            CaptureFrameResult::NAME,
            "aether.render.capture_frame_result"
        );
        assert_eq!(CreateTexture::NAME, "aether.render.create_texture");
        assert_eq!(
            CreateTextureResult::NAME,
            "aether.render.create_texture_result"
        );
        assert_eq!(UpdateTexture::NAME, "aether.render.update_texture");
        assert_eq!(DrawTexturedQuads::NAME, "aether.render.draw_textured_quads");
        assert_eq!(DrawSolidQuads::NAME, "aether.render.draw_solid_quads");
        assert_eq!(LoadFont::NAME, "aether.text.load_font");
        assert_eq!(LoadFontResult::NAME, "aether.text.load_font_result");
        assert_eq!(DrawText::NAME, "aether.text.draw");
        assert_eq!(SetWindowMode::NAME, "aether.window.set_mode");
        assert_eq!(SetWindowModeResult::NAME, "aether.window.set_mode_result");
        assert_eq!(SetWindowTitle::NAME, "aether.window.set_title");
        assert_eq!(SetWindowTitleResult::NAME, "aether.window.set_title_result");
        assert_eq!(FocusWindow::NAME, "aether.window.focus");
        assert_eq!(FocusWindowResult::NAME, "aether.window.focus_result");
        assert_eq!(Camera::NAME, "aether.camera");
        // ADR-0066: aether.camera.{create,destroy,set_active,set_mode,
        // orbit.set,topdown.set} kind-name asserts live in
        // `aether-camera`'s tests; the `aether.mesh.load` *request*
        // lives in `aether-mesh-viewer`'s tests. The view-proj sink
        // contract (`aether.camera`) stays here as a chassis primitive.
        // The structured load *reply* kinds (issue 964) live in this
        // crate, so their names are pinned here.
        assert_eq!(MeshLoadResult::NAME, "aether.mesh.load_result");
        assert_eq!(SceneLoadResult::NAME, "aether.scene.load_result");
        assert_eq!(NoteOn::NAME, "aether.audio.note_on");
        assert_eq!(NoteOff::NAME, "aether.audio.note_off");
        assert_eq!(SetMasterGain::NAME, "aether.audio.set_master_gain");
        assert_eq!(
            SetMasterGainResult::NAME,
            "aether.audio.set_master_gain_result"
        );
        assert_eq!(Schedule::NAME, "aether.audio.schedule");
        assert_eq!(ScheduleResult::NAME, "aether.audio.schedule_result");
        assert_eq!(PlayTrack::NAME, "aether.audio.play_track");
        assert_eq!(PlayTrackResult::NAME, "aether.audio.play_track_result");
        assert_eq!(StopTrack::NAME, "aether.audio.stop_track");
        assert_eq!(LoadInstrument::NAME, "aether.audio.load_instrument");
        assert_eq!(
            LoadInstrumentResult::NAME,
            "aether.audio.load_instrument_result"
        );
        assert_eq!(MonitorNotice::NAME, "aether.actor.monitor_notice");
        assert_eq!(LogTail::NAME, "aether.log.tail");
        assert_eq!(LogTailResult::NAME, "aether.log.tail_result");
        assert_eq!(CostTail::NAME, "aether.cost.tail");
        assert_eq!(CostTailResult::NAME, "aether.cost.tail_result");
        assert_eq!(Read::NAME, "aether.fs.read");
        assert_eq!(ReadResult::NAME, "aether.fs.read_result");
        assert_eq!(Write::NAME, "aether.fs.write");
        assert_eq!(WriteResult::NAME, "aether.fs.write_result");
        assert_eq!(Delete::NAME, "aether.fs.delete");
        assert_eq!(DeleteResult::NAME, "aether.fs.delete_result");
        assert_eq!(List::NAME, "aether.fs.list");
        assert_eq!(ListResult::NAME, "aether.fs.list_result");
        assert_eq!(Manifest::NAME, "aether.inventory.manifest");
        assert_eq!(ManifestResult::NAME, "aether.inventory.manifest_result");
        assert_eq!(Resolve::NAME, "aether.inventory.resolve");
        assert_eq!(ResolveResult::NAME, "aether.inventory.resolve_result");
        assert_eq!(ListKinds::NAME, "aether.inventory.kinds");
        assert_eq!(ListKindsResult::NAME, "aether.inventory.kinds_result");
    }

    // ADR-0019 PR 3 — every kind below now has a derived `Schema` impl
    // (gated on `descriptors`). These tests pin the derive output so
    // PR 5's switch-over of `descriptors.rs` from legacy `Pod`/`Signal`
    // arms to `Schema(...)` doesn't drift on wire bytes for cast-shaped
    // kinds.
    mod schema {
        use super::*;
        use aether_data::{CastEligible, Schema};
        use aether_data::{Primitive, SchemaType};
        #[test]
        fn unit_kinds_emit_schema_unit() {
            assert!(matches!(<Tick as Schema>::SCHEMA, SchemaType::Unit));
            assert!(matches!(<MouseButton as Schema>::SCHEMA, SchemaType::Unit));
        }

        #[test]
        fn cast_kinds_pick_repr_c_true() {
            const { assert!(<Key as CastEligible>::ELIGIBLE) };
            const { assert!(<MouseMove as CastEligible>::ELIGIBLE) };
            const { assert!(<Vertex as CastEligible>::ELIGIBLE) };
            const { assert!(<DrawTriangle as CastEligible>::ELIGIBLE) };
            const { assert!(<Ping as CastEligible>::ELIGIBLE) };
            const { assert!(<Pong as CastEligible>::ELIGIBLE) };
        }

        #[test]
        fn key_schema_is_one_u32_field() {
            let SchemaType::Struct { repr_c, fields } = &<Key as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert!(*repr_c);
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "code");
            assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
        }

        #[test]
        fn draw_triangle_schema_recurses_into_vertex() {
            let SchemaType::Struct { repr_c, fields } = &<DrawTriangle as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert!(*repr_c);
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "verts");
            let SchemaType::Array { element, len } = &fields[0].ty else {
                panic!("expected Array");
            };
            assert_eq!(*len, 3);
            let SchemaType::Struct {
                repr_c: nested_repr,
                fields: nested_fields,
            } = &**element
            else {
                panic!("expected nested Struct");
            };
            assert!(*nested_repr);
            assert_eq!(nested_fields.len(), 6);
            assert_eq!(nested_fields[0].name, "x");
            assert_eq!(nested_fields[2].name, "z");
            assert_eq!(nested_fields[5].name, "b");
        }
    }

    // iamacoffeepot/aether#1777 capture-verdict kind roundtrips. The
    // request gains an optional-background `checks` list and the result
    // gains an optional `verdict` carrying scalar / coordinate reduction
    // results; postcard roundtrip proves the derived Serialize/Deserialize
    // agree on the wire for the new shapes.
    mod capture_verdict_roundtrips {
        use super::*;
        use alloc::string::ToString;
        use alloc::vec;

        #[test]
        fn capture_frame_checks_roundtrip() {
            let frame = CaptureFrame {
                mails: vec![],
                after_mails: vec![],
                checks: vec![
                    FrameCheck {
                        reduction: FrameReduction::NotAllBlack,
                        tolerance: 0,
                        background: None,
                    },
                    FrameCheck {
                        reduction: FrameReduction::Coverage,
                        tolerance: 5,
                        background: Some([69, 79, 105]),
                    },
                ],
            };
            let bytes =
                postcard::to_allocvec(&frame).expect("test setup: postcard encodes CaptureFrame");
            let back: CaptureFrame =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes CaptureFrame");
            assert_eq!(back.checks.len(), 2);
            assert_eq!(back.checks[0].reduction, FrameReduction::NotAllBlack);
            assert_eq!(back.checks[0].background, None);
            assert_eq!(back.checks[1].reduction, FrameReduction::Coverage);
            assert_eq!(back.checks[1].tolerance, 5);
            assert_eq!(back.checks[1].background, Some([69, 79, 105]));
        }

        #[test]
        fn capture_frame_result_verdict_roundtrip() {
            let ok = CaptureFrameResult::Ok {
                png: vec![0x89, 0x50, 0x4E, 0x47],
                verdict: Some(FrameVerdict {
                    width: 64,
                    height: 48,
                    results: vec![
                        FrameCheckResult::NotAllBlack {
                            passed: true,
                            detail: None,
                        },
                        FrameCheckResult::DiffersFromBackground {
                            passed: false,
                            detail: Some("all pixels within tolerance".to_string()),
                        },
                        FrameCheckResult::Coverage {
                            background: [69, 79, 105],
                            fraction: 0.25,
                        },
                        FrameCheckResult::Centroid {
                            background: [69, 79, 105],
                            centroid: Some([31.5, 23.5]),
                        },
                        FrameCheckResult::BoundingBox {
                            background: [69, 79, 105],
                            rect: Some(FrameRect {
                                min_x: 16,
                                min_y: 12,
                                max_x: 40,
                                max_y: 30,
                            }),
                        },
                    ],
                }),
            };
            let bytes = postcard::to_allocvec(&ok)
                .expect("test setup: postcard encodes CaptureFrameResult::Ok");
            let back: CaptureFrameResult = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes CaptureFrameResult::Ok");
            match back {
                CaptureFrameResult::Ok { png, verdict } => {
                    assert_eq!(png, vec![0x89, 0x50, 0x4E, 0x47]);
                    let verdict = verdict.expect("verdict survives the roundtrip");
                    assert_eq!((verdict.width, verdict.height), (64, 48));
                    assert_eq!(verdict.results.len(), 5);
                    assert_eq!(
                        verdict.results[2],
                        FrameCheckResult::Coverage {
                            background: [69, 79, 105],
                            fraction: 0.25,
                        },
                    );
                    assert_eq!(
                        verdict.results[4],
                        FrameCheckResult::BoundingBox {
                            background: [69, 79, 105],
                            rect: Some(FrameRect {
                                min_x: 16,
                                min_y: 12,
                                max_x: 40,
                                max_y: 30,
                            }),
                        },
                    );
                }
                CaptureFrameResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn capture_frame_result_no_verdict_roundtrip() {
            let ok = CaptureFrameResult::Ok {
                png: vec![1, 2, 3],
                verdict: None,
            };
            let bytes = postcard::to_allocvec(&ok)
                .expect("test setup: postcard encodes CaptureFrameResult::Ok");
            let back: CaptureFrameResult = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes CaptureFrameResult::Ok");
            match back {
                CaptureFrameResult::Ok { verdict, .. } => assert!(verdict.is_none()),
                CaptureFrameResult::Err { .. } => panic!("expected Ok"),
            }
        }
    }

    // ADR-0105 textured-quad render-surface kind roundtrips. The request
    // types carry `Vec<u8>` pixels and a `space` enum carrying nested
    // arrays; postcard roundtrip proves the derived Serialize/Deserialize
    // agree on the wire for each shape.
    mod render_quad_roundtrips {
        use super::*;
        use alloc::string::ToString;
        use alloc::vec;

        #[test]
        fn create_texture_request_roundtrip() {
            let c = CreateTexture {
                width: 2,
                height: 2,
                pixels: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            };
            let bytes =
                postcard::to_allocvec(&c).expect("test setup: postcard encodes CreateTexture");
            let back: CreateTexture =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes CreateTexture");
            assert_eq!(back.width, 2);
            assert_eq!(back.height, 2);
            assert_eq!(back.pixels.len(), 16);
        }

        #[test]
        fn create_texture_result_roundtrip_both_arms() {
            let ok = CreateTextureResult::Ok { texture_id: 7 };
            let bytes = postcard::to_allocvec(&ok)
                .expect("test setup: postcard encodes CreateTextureResult::Ok");
            let back: CreateTextureResult = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes CreateTextureResult::Ok");
            match back {
                CreateTextureResult::Ok { texture_id } => assert_eq!(texture_id, 7),
                CreateTextureResult::Err { .. } => panic!("expected Ok"),
            }

            let err = CreateTextureResult::Err {
                error: "pixels length mismatch".to_string(),
            };
            let bytes = postcard::to_allocvec(&err)
                .expect("test setup: postcard encodes CreateTextureResult::Err");
            let back: CreateTextureResult = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes CreateTextureResult::Err");
            match back {
                CreateTextureResult::Err { error } => assert_eq!(error, "pixels length mismatch"),
                CreateTextureResult::Ok { .. } => panic!("expected Err"),
            }
        }

        #[test]
        fn update_texture_request_roundtrip() {
            let u = UpdateTexture {
                texture_id: 3,
                x: 4,
                y: 5,
                width: 1,
                height: 1,
                pixels: vec![9, 8, 7, 6],
            };
            let bytes =
                postcard::to_allocvec(&u).expect("test setup: postcard encodes UpdateTexture");
            let back: UpdateTexture =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes UpdateTexture");
            assert_eq!(back.texture_id, 3);
            assert_eq!((back.x, back.y), (4, 5));
            assert_eq!(back.pixels, vec![9, 8, 7, 6]);
        }

        #[test]
        fn draw_textured_quads_screen_roundtrip() {
            let d = DrawTexturedQuads {
                texture_id: 1,
                space: QuadSpace::Screen,
                quads: vec![TexturedQuad {
                    x: 10.0,
                    y: 8.0,
                    width: 20.0,
                    height: 16.0,
                    u0: 0.0,
                    v0: 0.0,
                    u1: 1.0,
                    v1: 1.0,
                    tint: [1.0, 1.0, 1.0, 1.0],
                }],
            };
            let bytes =
                postcard::to_allocvec(&d).expect("test setup: postcard encodes DrawTexturedQuads");
            let back: DrawTexturedQuads = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes DrawTexturedQuads");
            assert_eq!(back.texture_id, 1);
            assert_eq!(back.space, QuadSpace::Screen);
            assert_eq!(back.quads.len(), 1);
            assert_eq!(back.quads[0].width, 20.0);
            assert_eq!(back.quads[0].tint, [1.0, 1.0, 1.0, 1.0]);
        }

        #[test]
        fn draw_textured_quads_world_roundtrip_carries_anchor_and_scale() {
            let d = DrawTexturedQuads {
                texture_id: 2,
                space: QuadSpace::World {
                    anchor: [1.0, 2.0, 3.0],
                    scale: QuadScale::Distance {
                        reference_distance: 5.0,
                    },
                },
                quads: vec![],
            };
            let bytes = postcard::to_allocvec(&d)
                .expect("test setup: postcard encodes DrawTexturedQuads (World)");
            let back: DrawTexturedQuads = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes DrawTexturedQuads (World)");
            match back.space {
                QuadSpace::World { anchor, scale } => {
                    assert_eq!(anchor, [1.0, 2.0, 3.0]);
                    assert_eq!(
                        scale,
                        QuadScale::Distance {
                            reference_distance: 5.0
                        }
                    );
                }
                QuadSpace::Screen => panic!("expected World"),
            }
        }

        #[test]
        fn draw_solid_quads_screen_roundtrip() {
            let d = DrawSolidQuads {
                space: QuadSpace::Screen,
                quads: vec![SolidQuad {
                    x: 10.0,
                    y: 8.0,
                    width: 20.0,
                    height: 16.0,
                    color: [1.0, 0.0, 0.5, 1.0],
                }],
            };
            let bytes =
                postcard::to_allocvec(&d).expect("test setup: postcard encodes DrawSolidQuads");
            let back: DrawSolidQuads =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes DrawSolidQuads");
            assert_eq!(back.space, QuadSpace::Screen);
            assert_eq!(back.quads.len(), 1);
            assert_eq!(back.quads[0].width, 20.0);
            assert_eq!(back.quads[0].color, [1.0, 0.0, 0.5, 1.0]);
        }

        #[test]
        fn load_font_request_roundtrip() {
            let r = LoadFont {
                namespace: "assets".to_string(),
                path: "fonts/RobotoMono.ttf".to_string(),
            };
            let bytes = postcard::to_allocvec(&r).expect("test setup: postcard encodes LoadFont");
            let back: LoadFont =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes LoadFont");
            assert_eq!(back.namespace, r.namespace);
            assert_eq!(back.path, r.path);
        }

        #[test]
        fn load_font_result_roundtrip_both_arms() {
            let ok = LoadFontResult::Ok {
                font_id: 3,
                name: "RobotoMono".to_string(),
                resident_bytes: 183_700,
            };
            let bytes = postcard::to_allocvec(&ok)
                .expect("test setup: postcard encodes LoadFontResult::Ok");
            let back: LoadFontResult = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes LoadFontResult::Ok");
            match back {
                LoadFontResult::Ok {
                    font_id,
                    name,
                    resident_bytes,
                } => {
                    assert_eq!(font_id, 3);
                    assert_eq!(name, "RobotoMono");
                    assert_eq!(resident_bytes, 183_700);
                }
                LoadFontResult::Err { .. } => panic!("expected Ok"),
            }

            let err = LoadFontResult::Err {
                namespace: "assets".to_string(),
                path: "missing.ttf".to_string(),
                error: "file read failed".to_string(),
            };
            let bytes = postcard::to_allocvec(&err)
                .expect("test setup: postcard encodes LoadFontResult::Err");
            let back: LoadFontResult = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes LoadFontResult::Err");
            match back {
                LoadFontResult::Err {
                    namespace,
                    path,
                    error,
                } => {
                    assert_eq!(namespace, "assets");
                    assert_eq!(path, "missing.ttf");
                    assert_eq!(error, "file read failed");
                }
                LoadFontResult::Ok { .. } => panic!("expected Err"),
            }
        }

        #[test]
        fn draw_text_screen_roundtrip() {
            let d = DrawText {
                font_id: 1,
                text: "hello aether".to_string(),
                size_pixels: 32.0,
                color: [1.0, 0.5, 0.25, 1.0],
                origin: [24.0, 48.0],
                space: QuadSpace::Screen,
            };
            let bytes = postcard::to_allocvec(&d).expect("test setup: postcard encodes DrawText");
            let back: DrawText =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes DrawText");
            assert_eq!(back.font_id, 1);
            assert_eq!(back.text, "hello aether");
            assert_eq!(back.size_pixels, 32.0);
            assert_eq!(back.color, [1.0, 0.5, 0.25, 1.0]);
            assert_eq!(back.origin, [24.0, 48.0]);
            assert_eq!(back.space, QuadSpace::Screen);
        }
    }

    // ADR-0041 I/O kind roundtrips. Request types carry String /
    // Vec<u8>, reply types are Ok/Err enums with the error arm
    // wrapping `FsError`. postcard roundtrip proves the derived
    // Serialize/Deserialize agree on the wire for each shape.
    mod fs_roundtrips {
        use super::*;
        use alloc::string::ToString;
        use alloc::vec;

        #[test]
        fn read_request_roundtrip() {
            let r = Read {
                namespace: "save".to_string(),
                path: "slot1.bin".to_string(),
            };
            let bytes = postcard::to_allocvec(&r).expect("test setup: postcard encodes Read");
            let back: Read =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes Read");
            assert_eq!(back.namespace, r.namespace);
            assert_eq!(back.path, r.path);
        }

        #[test]
        fn read_result_ok_roundtrip_echoes_request() {
            let r = ReadResult::Ok {
                namespace: "save".to_string(),
                path: "slot.bin".to_string(),
                bytes: vec![1, 2, 3, 4],
            };
            let bytes =
                postcard::to_allocvec(&r).expect("test setup: postcard encodes ReadResult::Ok");
            let back: ReadResult =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes ReadResult::Ok");
            match back {
                ReadResult::Ok {
                    namespace,
                    path,
                    bytes,
                } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(path, "slot.bin");
                    assert_eq!(bytes, vec![1, 2, 3, 4]);
                }
                ReadResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn read_result_err_roundtrip_echoes_request_and_io_error() {
            let r = ReadResult::Err {
                namespace: "save".to_string(),
                path: "ghost.bin".to_string(),
                error: FsError::NotFound,
            };
            let bytes =
                postcard::to_allocvec(&r).expect("test setup: postcard encodes ReadResult::Err");
            let back: ReadResult =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes ReadResult::Err");
            match back {
                ReadResult::Err {
                    namespace,
                    path,
                    error,
                } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(path, "ghost.bin");
                    assert_eq!(error, FsError::NotFound);
                }
                ReadResult::Ok { .. } => panic!("expected Err"),
            }
        }

        #[test]
        fn io_error_adapter_carries_payload() {
            let e = FsError::AdapterError("disk full".to_string());
            let bytes = postcard::to_allocvec(&e).expect("test setup: postcard encodes FsError");
            let back: FsError =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes FsError");
            match back {
                FsError::AdapterError(msg) => assert_eq!(msg, "disk full"),
                other => panic!("expected AdapterError, got {other:?}"),
            }
        }

        #[test]
        fn write_request_roundtrip() {
            let w = Write {
                namespace: "save".to_string(),
                path: "state.bin".to_string(),
                bytes: vec![0xde, 0xad, 0xbe, 0xef],
            };
            let bytes = postcard::to_allocvec(&w).expect("test setup: postcard encodes Write");
            let back: Write =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes Write");
            assert_eq!(back.bytes, vec![0xde, 0xad, 0xbe, 0xef]);
        }

        #[test]
        fn list_result_ok_roundtrip_echoes_namespace_and_prefix() {
            let r = ListResult::Ok {
                namespace: "save".to_string(),
                prefix: "slots/".to_string(),
                entries: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            };
            let bytes =
                postcard::to_allocvec(&r).expect("test setup: postcard encodes ListResult::Ok");
            let back: ListResult =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes ListResult::Ok");
            match back {
                ListResult::Ok {
                    namespace,
                    prefix,
                    entries,
                } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(prefix, "slots/");
                    assert_eq!(entries, vec!["a", "b", "c"]);
                }
                ListResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn write_result_ok_roundtrip_echoes_path_without_bytes() {
            // Deliberately exercises the "no bytes in reply" rule:
            // WriteResult::Ok has no `bytes` field — confirming the
            // wire shape excludes the write payload.
            let r = WriteResult::Ok {
                namespace: "save".to_string(),
                path: "state.bin".to_string(),
            };
            let bytes =
                postcard::to_allocvec(&r).expect("test setup: postcard encodes WriteResult::Ok");
            let back: WriteResult =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes WriteResult::Ok");
            match back {
                WriteResult::Ok { namespace, path } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(path, "state.bin");
                }
                WriteResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn delete_result_err_roundtrip_echoes_request_and_io_error() {
            let r = DeleteResult::Err {
                namespace: "save".to_string(),
                path: "ghost.bin".to_string(),
                error: FsError::NotFound,
            };
            let bytes =
                postcard::to_allocvec(&r).expect("test setup: postcard encodes DeleteResult::Err");
            let back: DeleteResult = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes DeleteResult::Err");
            match back {
                DeleteResult::Err {
                    namespace,
                    path,
                    error,
                } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(path, "ghost.bin");
                    assert_eq!(error, FsError::NotFound);
                }
                DeleteResult::Ok { .. } => panic!("expected Err"),
            }
        }
    }

    // iamacoffeepot/aether#1128 cost-table dump roundtrips. `CostTail`
    // carries an optional kind filter; `CostTailResult::Ok` carries one
    // `CostRow` per handler. Both go through the derived
    // Serialize/Deserialize (postcard wire) — pin that the optional
    // filter and the per-row fields survive the round trip.
    mod cost_roundtrips {
        use super::*;
        use alloc::string::ToString;
        use alloc::vec;

        #[test]
        fn cost_tail_request_roundtrips_filter() {
            for kind in [None, Some(aether_data::KindId(0xABCD))] {
                let r = CostTail { kind };
                let bytes =
                    postcard::to_allocvec(&r).expect("test setup: postcard encodes CostTail");
                let back: CostTail =
                    postcard::from_bytes(&bytes).expect("test setup: postcard decodes CostTail");
                assert_eq!(back.kind, kind);
            }
        }

        #[test]
        fn cost_tail_result_ok_roundtrips_rows() {
            let r = CostTailResult::Ok {
                rows: vec![
                    CostRow {
                        kind_id: aether_data::KindId(10),
                        kind_name: Some("test.kind.a".to_string()),
                        mean_nanos: 1_234,
                        mad_nanos: 56,
                        samples: 9,
                    },
                    CostRow {
                        kind_id: aether_data::KindId(20),
                        kind_name: None,
                        mean_nanos: 0,
                        mad_nanos: 0,
                        samples: 0,
                    },
                ],
            };
            let bytes =
                postcard::to_allocvec(&r).expect("test setup: postcard encodes CostTailResult::Ok");
            let back: CostTailResult = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes CostTailResult::Ok");
            let CostTailResult::Ok { rows } = back else {
                panic!("expected Ok");
            };
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].kind_id, aether_data::KindId(10));
            assert_eq!(rows[0].kind_name.as_deref(), Some("test.kind.a"));
            assert_eq!(rows[0].mean_nanos, 1_234);
            assert_eq!(rows[0].samples, 9);
            // Neutral seed row survives with samples = 0 + no name.
            assert_eq!(rows[1].samples, 0);
            assert_eq!(rows[1].kind_name, None);
        }

        #[test]
        fn cost_tail_result_err_roundtrips() {
            let r = CostTailResult::Err {
                error: "no stamped slots".to_string(),
            };
            let bytes = postcard::to_allocvec(&r)
                .expect("test setup: postcard encodes CostTailResult::Err");
            let back: CostTailResult = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes CostTailResult::Err");
            match back {
                CostTailResult::Err { error } => assert_eq!(error, "no stamped slots"),
                CostTailResult::Ok { .. } => panic!("expected Err"),
            }
        }
    }

    // ADR-0043 HTTP kind roundtrips. `Fetch` carries String + typed
    // method + Vec<HttpHeader> + Vec<u8> body + Option<u32>;
    // `FetchResult` mirrors `ReadResult`'s Ok/Err split with a
    // typed error arm wrapping `HttpError`. Tests prove the derived
    // Serialize/Deserialize agree on the wire for each shape, with
    // special attention to the `body`-not-echoed invariant and the
    // payload-carrying `HttpError` variants.
    mod http_roundtrips {
        use super::*;
        use alloc::string::ToString;
        use alloc::vec;
        use alloc::vec::Vec;

        fn sample_headers() -> Vec<HttpHeader> {
            vec![
                HttpHeader {
                    name: "content-type".to_string(),
                    value: "application/json".to_string(),
                },
                HttpHeader {
                    name: "user-agent".to_string(),
                    value: "aether/0.2".to_string(),
                },
            ]
        }

        #[test]
        fn fetch_request_roundtrip() {
            let f = Fetch {
                url: "https://api.example.com/v1/resource".to_string(),
                method: HttpMethod::Post,
                headers: sample_headers(),
                body: vec![b'{', b'}'],
                timeout_ms: Some(5000),
            };
            let bytes = postcard::to_allocvec(&f).expect("test setup: postcard encodes Fetch");
            let back: Fetch =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes Fetch");
            assert_eq!(back.url, f.url);
            assert_eq!(back.method, HttpMethod::Post);
            assert_eq!(back.headers, f.headers);
            assert_eq!(back.body, vec![b'{', b'}']);
            assert_eq!(back.timeout_ms, Some(5000));
        }

        #[test]
        fn fetch_request_roundtrip_no_timeout() {
            let f = Fetch {
                url: "https://api.example.com/".to_string(),
                method: HttpMethod::Get,
                headers: vec![],
                body: vec![],
                timeout_ms: None,
            };
            let bytes =
                postcard::to_allocvec(&f).expect("test setup: postcard encodes Fetch (no timeout)");
            let back: Fetch = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes Fetch (no timeout)");
            assert_eq!(back.timeout_ms, None);
            assert_eq!(back.method, HttpMethod::Get);
        }

        #[test]
        fn fetch_result_ok_roundtrip_echoes_url() {
            let r = FetchResult::Ok {
                url: "https://api.example.com/v1/resource".to_string(),
                status: 200,
                headers: sample_headers(),
                body: vec![0xde, 0xad, 0xbe, 0xef],
            };
            let bytes =
                postcard::to_allocvec(&r).expect("test setup: postcard encodes FetchResult::Ok");
            let back: FetchResult =
                postcard::from_bytes(&bytes).expect("test setup: postcard decodes FetchResult::Ok");
            match back {
                FetchResult::Ok {
                    url,
                    status,
                    headers,
                    body,
                } => {
                    assert_eq!(url, "https://api.example.com/v1/resource");
                    assert_eq!(status, 200);
                    assert_eq!(headers.len(), 2);
                    assert_eq!(body, vec![0xde, 0xad, 0xbe, 0xef]);
                }
                FetchResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn fetch_result_err_roundtrip_echoes_url_and_http_error() {
            let r = FetchResult::Err {
                url: "https://api.example.com/gone".to_string(),
                error: HttpError::Timeout,
            };
            let bytes =
                postcard::to_allocvec(&r).expect("test setup: postcard encodes FetchResult::Err");
            let back: FetchResult = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes FetchResult::Err");
            match back {
                FetchResult::Err { url, error } => {
                    assert_eq!(url, "https://api.example.com/gone");
                    assert_eq!(error, HttpError::Timeout);
                }
                FetchResult::Ok { .. } => panic!("expected Err"),
            }
        }

        #[test]
        fn http_error_invalid_url_carries_payload() {
            let e = HttpError::InvalidUrl("not a url".to_string());
            let bytes = postcard::to_allocvec(&e)
                .expect("test setup: postcard encodes HttpError::InvalidUrl");
            let back: HttpError = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes HttpError::InvalidUrl");
            match back {
                HttpError::InvalidUrl(s) => assert_eq!(s, "not a url"),
                other => panic!("expected InvalidUrl, got {other:?}"),
            }
        }

        #[test]
        fn http_error_adapter_carries_detail() {
            let e = HttpError::AdapterError("dns lookup failed".to_string());
            let bytes = postcard::to_allocvec(&e)
                .expect("test setup: postcard encodes HttpError::AdapterError");
            let back: HttpError = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes HttpError::AdapterError");
            match back {
                HttpError::AdapterError(s) => assert_eq!(s, "dns lookup failed"),
                other => panic!("expected AdapterError, got {other:?}"),
            }
        }

        #[test]
        fn http_error_unit_variants_roundtrip() {
            for e in [
                HttpError::Timeout,
                HttpError::BodyTooLarge,
                HttpError::AllowlistDenied,
                HttpError::Disabled,
            ] {
                let bytes = postcard::to_allocvec(&e)
                    .expect("test setup: postcard encodes HttpError unit variant");
                let back: HttpError = postcard::from_bytes(&bytes)
                    .expect("test setup: postcard decodes HttpError unit variant");
                assert_eq!(back, e);
            }
        }

        #[test]
        fn http_method_roundtrip_all_variants() {
            for m in [
                HttpMethod::Get,
                HttpMethod::Post,
                HttpMethod::Put,
                HttpMethod::Delete,
                HttpMethod::Patch,
                HttpMethod::Head,
                HttpMethod::Options,
            ] {
                let bytes = postcard::to_allocvec(&m)
                    .expect("test setup: postcard encodes HttpMethod variant");
                let back: HttpMethod = postcard::from_bytes(&bytes)
                    .expect("test setup: postcard decodes HttpMethod variant");
                assert_eq!(back, m);
            }
        }

        #[test]
        fn http_server_request_roundtrip() {
            assert_eq!(HttpServerRequest::NAME, "aether.http.server.request");
            let r = HttpServerRequest {
                method: HttpMethod::Post,
                path: "/api/v1/things".to_string(),
                query: "foo=bar&baz=1".to_string(),
                headers: sample_headers(),
                body: vec![0x01, 0x02, 0x03],
            };
            let bytes =
                postcard::to_allocvec(&r).expect("test setup: postcard encodes HttpServerRequest");
            let back: HttpServerRequest = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes HttpServerRequest");
            assert_eq!(back.method, HttpMethod::Post);
            assert_eq!(back.path, "/api/v1/things");
            assert_eq!(back.query, "foo=bar&baz=1");
            assert_eq!(back.headers, r.headers);
            assert_eq!(back.body, vec![0x01, 0x02, 0x03]);
        }

        #[test]
        fn http_server_request_empty_query_roundtrip() {
            let r = HttpServerRequest {
                method: HttpMethod::Get,
                path: "/health".to_string(),
                query: String::new(),
                headers: vec![],
                body: vec![],
            };
            let bytes = postcard::to_allocvec(&r)
                .expect("test setup: postcard encodes HttpServerRequest (empty query)");
            let back: HttpServerRequest = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes HttpServerRequest (empty query)");
            assert_eq!(back.query, "");
            assert_eq!(back.method, HttpMethod::Get);
        }

        #[test]
        fn http_server_response_roundtrip() {
            assert_eq!(HttpServerResponse::NAME, "aether.http.server.response");
            let r = HttpServerResponse {
                status: 200,
                headers: sample_headers(),
                body: vec![0xde, 0xad, 0xbe, 0xef],
            };
            let bytes =
                postcard::to_allocvec(&r).expect("test setup: postcard encodes HttpServerResponse");
            let back: HttpServerResponse = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes HttpServerResponse");
            assert_eq!(back.status, 200);
            assert_eq!(back.headers, r.headers);
            assert_eq!(back.body, vec![0xde, 0xad, 0xbe, 0xef]);
        }

        #[test]
        fn http_server_response_error_status_roundtrip() {
            let r = HttpServerResponse {
                status: 404,
                headers: vec![],
                body: b"not found".to_vec(),
            };
            let bytes = postcard::to_allocvec(&r)
                .expect("test setup: postcard encodes HttpServerResponse (404)");
            let back: HttpServerResponse = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes HttpServerResponse (404)");
            assert_eq!(back.status, 404);
            assert_eq!(back.body, b"not found");
        }
    }

    mod control_plane_roundtrips {
        use super::*;
        use alloc::string::ToString;
        use alloc::vec;

        #[test]
        fn load_component_roundtrips_config_bytes() {
            // ADR-0090 c2: the optional init-config carrier must survive
            // the postcard wire path intact so the substrate hands the
            // exact bytes to the guest's typed `init`.
            let load = LoadComponent {
                wasm: vec![0x00, 0x61, 0x73, 0x6d],
                name: Some("probe_with_config".to_string()),
                config: vec![0xde, 0xad, 0xbe, 0xef],
                export: Some("ui.panel".to_string()),
            };
            let bytes = load.encode_into_bytes();
            let back =
                LoadComponent::decode_from_bytes(&bytes).expect("decode LoadComponent round-trip");
            assert_eq!(back.config, vec![0xde, 0xad, 0xbe, 0xef]);
            assert_eq!(back.wasm, vec![0x00, 0x61, 0x73, 0x6d]);
            assert_eq!(back.name.as_deref(), Some("probe_with_config"));
            assert_eq!(back.export.as_deref(), Some("ui.panel"));
        }

        #[test]
        fn replace_component_roundtrips_config_bytes() {
            let replace = ReplaceComponent {
                mailbox_id: aether_data::MailboxId(7),
                wasm: vec![0x00, 0x61, 0x73, 0x6d],
                drain_timeout_ms: Some(2500),
                config: vec![0x01, 0x02, 0x03],
            };
            let bytes = replace.encode_into_bytes();
            let back = ReplaceComponent::decode_from_bytes(&bytes)
                .expect("decode ReplaceComponent round-trip");
            assert_eq!(back.config, vec![0x01, 0x02, 0x03]);
            assert_eq!(back.mailbox_id, aether_data::MailboxId(7));
        }

        #[test]
        fn schedule_roundtrips_timed_note_batch() {
            // ADR-0104: the timed batch must survive the wire intact —
            // a note-on and its later note-off both carry their offset and
            // payload so the synth can place them on its sample clock.
            let schedule = Schedule {
                events: vec![
                    ScheduledEvent {
                        at_millis: 0,
                        event: ScheduledNote::On {
                            pitch: 60,
                            velocity: 100,
                            instrument_id: 0,
                        },
                    },
                    ScheduledEvent {
                        at_millis: 500,
                        event: ScheduledNote::Off {
                            pitch: 60,
                            instrument_id: 0,
                        },
                    },
                ],
            };
            let bytes = schedule.encode_into_bytes();
            let back = Schedule::decode_from_bytes(&bytes).expect("decode Schedule round-trip");
            assert_eq!(back.events.len(), 2);
            assert_eq!(back.events[0].at_millis, 0);
            assert_eq!(
                back.events[0].event,
                ScheduledNote::On {
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 0,
                },
            );
            assert_eq!(back.events[1].at_millis, 500);
            assert_eq!(
                back.events[1].event,
                ScheduledNote::Off {
                    pitch: 60,
                    instrument_id: 0,
                },
            );
        }

        #[test]
        fn schedule_result_roundtrips_both_arms() {
            let ok = ScheduleResult::Ok { accepted: 7 };
            let back = ScheduleResult::decode_from_bytes(&ok.encode_into_bytes())
                .expect("decode ScheduleResult::Ok round-trip");
            assert!(matches!(back, ScheduleResult::Ok { accepted: 7 }));

            let err = ScheduleResult::Err {
                error: "batch exceeds the 8192-event cap".to_string(),
            };
            let back = ScheduleResult::decode_from_bytes(&err.encode_into_bytes())
                .expect("decode ScheduleResult::Err round-trip");
            match back {
                ScheduleResult::Err { error } => assert!(error.contains("8192-event")),
                ScheduleResult::Ok { .. } => panic!("expected Err"),
            }
        }
    }
}
