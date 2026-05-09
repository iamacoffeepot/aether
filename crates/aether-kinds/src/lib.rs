//! aether-kinds: the substrate's own mail vocabulary. Imported by any
//! actor that wants to send mail to the substrate, receive mail the
//! substrate dispatches (tick, input), or consume the substrate's sink
//! kinds (draw_triangle). See ADR-0005 / ADR-0030.
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

use bytemuck::{Pod, Zeroable};

/// Hub broadcast mailbox name (ADR-0008 observation path). The
/// `BroadcastCapability` (in `aether-capabilities`) reuses this string
/// as its `Actor::NAMESPACE`; substrate-internal pushes (frame_loop's
/// frame-stats emission, the scheduler-death announce) read the same
/// const so name and id stay in lockstep without depending on
/// `aether-capabilities`. Issue #613 retired the `mailboxes` module
/// the const used to live in; this single string is the residue.
pub const HUB_BROADCAST_MAILBOX_NAME: &str = "hub.claude.broadcast";

// Every kind below derives both `Kind` and `Schema`. Pre-ADR-0032
// `Schema` was gated behind a `descriptors` feature so wasm guests
// stayed free of hub-protocol; that gate retired once hub-protocol
// went no_std + alloc. `Schema` drives both the canonical bytes the
// `aether.kinds` section carries and the `LABEL_NODE` sidecar — so
// it's load-bearing on every build, not an optional enrichment.

/// Per-frame signal from the substrate's frame loop. Empty payload —
/// elapsed-time is parked until a subscriber actually needs it.
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
#[kind(name = "aether.tick", stream)]
pub struct Tick;

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
#[kind(name = "aether.key", stream)]
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
#[kind(name = "aether.key_release", stream)]
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
#[kind(name = "aether.mouse_button", stream)]
pub struct MouseButton;

/// Cursor position in window coordinates, as logical pixels cast to f32.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.mouse_move", stream)]
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
#[kind(name = "aether.window_size", stream)]
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
pub const DRAW_TRIANGLE_BYTES: usize = core::mem::size_of::<DrawTriangle>();

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

/// Periodic observation emitted by the substrate's frame loop when a
/// hub is attached (ADR-0008). The substrate pushes one of these at
/// `LOG_EVERY_FRAMES` cadence to the `hub.claude.broadcast` sink, so
/// every attached Claude session learns how the engine is running
/// without having to poll the engine directly.
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
#[kind(name = "aether.observation.frame_stats")]
pub struct FrameStats {
    pub frame: u64,
    pub triangles: u64,
}

/// Substrate broadcast on actor death (issue 321 Phase 2). The
/// dispatcher emits one of these to `hub.claude.broadcast` when a
/// component's actor thread is marked dead — either because the guest
/// trapped during `deliver` or a host-side panic was caught around the
/// loop body. External monitor components (or a Claude session in MCP)
/// observe this kind via `receive_mail` and decide what to do —
/// `replace_component` for hot-recovery, page a human, or just leave
/// the mailbox dead. The substrate itself takes no recovery action;
/// policy lives outside.
///
/// `last_kind` carries the kind name being delivered when the actor
/// died. `reason` is a human-readable string describing the failure
/// (panic payload, trap message). String fields make this postcard-
/// shaped on the wire (cast eligibility is false for non-`Pod` types).
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "aether.observation.component_died")]
pub struct ComponentDied {
    pub mailbox_id: aether_data::MailboxId,
    pub mailbox_name: alloc::string::String,
    pub last_kind: alloc::string::String,
    pub reason: alloc::string::String,
}

/// Final broadcast emitted by the substrate before `lifecycle::
/// fatal_abort` calls `std::process::exit` (ADR-0063). Tells attached
/// hub sessions that the substrate is going down on purpose, with a
/// human-readable reason. Distinct from `ComponentDied`: the latter
/// fires per dying component while the substrate keeps running;
/// `SubstrateDying` fires once, immediately before exit, regardless of
/// whether the cause was a component death or a wedged dispatcher.
///
/// `reason` is the same string that lands in `engine_logs` (e.g.
/// `"component died: <kind> ..."` or `"dispatcher wedged: mailbox=...
/// waited=5s"`). Receivers should treat this as the engine's last
/// word — the TCP connection drops moments later.
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "aether.observation.substrate_dying")]
pub struct SubstrateDying {
    pub reason: alloc::string::String,
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
#[kind(name = "aether.observation.monitor_notice")]
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

mod control_plane {
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

    /// Reply to subscribe / unsubscribe / unsubscribe_all (ADR-0021 §2).
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

    /// `aether.log.configure_drain` — chassis-pushed mail every actor
    /// handles to pick up the configured log drain mailbox (issue
    /// #601). The chassis dispatches one of these to every
    /// freshly-instantiated actor before any other inbound mail; the
    /// SDK's `#[actor]` / `#[handlers]` derive auto-emits a handler
    /// that installs `mailbox` into the per-actor `LogDrainSlot` so
    /// `aether-actor::log::drain_buffer` knows where to ship the
    /// `LogBatch`. Cast-shape — one `MailboxId`, fixed size.
    ///
    /// Fire-and-forget; no reply. If the chassis didn't declare a
    /// drain via `Builder::with_log_drain<T>()` no `ConfigureLogDrain`
    /// is dispatched and the slot stays at its default (`MailboxId(0)`,
    /// drain disabled).
    use bytemuck::{Pod, Zeroable};

    #[repr(C)]
    #[derive(
        Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema,
    )]
    #[kind(name = "aether.log.configure_drain")]
    pub struct ConfigureLogDrain {
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
    /// Reply: `CaptureFrameResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.capture_frame")]
    pub struct CaptureFrame {
        pub mails: Vec<MailEnvelope>,
        pub after_mails: Vec<MailEnvelope>,
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
    /// captured frame; `Err` carries a free-form reason — capture not
    /// supported on this surface, map failed, encode failed, or a
    /// bundle-resolution failure (unknown kind / mailbox) aborting
    /// before any mail was dispatched.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.capture_frame_result")]
    pub enum CaptureFrameResult {
        Ok { png: Vec<u8> },
        Err { error: String },
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
    // per-ReplyTo correlation ids via `prev_correlation_p32` rather
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

    // Issue #581 unified actor-aware logging. `LogEvent` is the shape
    // the actor-aware tracing subscriber buffers per-actor; the wire
    // kind is `LogBatch` (one mail per drain rather than one mail per
    // event). `LogEvent` itself is intentionally *not* a `Kind` — it
    // can't be addressed as top-level mail, so the only authors are
    // the in-crate subscriber and the `aether::log_*!` macros. The
    // mailbox `"aether.log"` (kind id namespace `aether.log.*`) is
    // owned by `LogCapability` (`aether-capabilities::log`) which
    // forwards entries to the hub.

    /// One captured tracing event, ready to be batched into a
    /// `LogBatch`. `level` maps `0 = trace .. 4 = error`. `target` is
    /// a module-style string the chassis's `EnvFilter` matches against.
    /// `message` is pre-formatted with structured fields collapsed
    /// into the message body (e.g. `"error=<Display> count=3 parse
    /// failed"`), capped by the subscriber with a `" [truncated]"`
    /// suffix on overflow.
    ///
    /// Pre-#581 this carried `#[derive(aether_data::Kind,
    /// aether_data::Schema)]` + `#[kind(name = "aether.log")]` and was
    /// mailed one event at a time. The `Kind` derive was dropped to
    /// make `LogEvent` unmailable on its own — every emission flows
    /// through `LogBatch`. `Schema` stays so the derive on `LogBatch`
    /// can describe its `Vec<LogEvent>` field's wire shape.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct LogEvent {
        pub level: u8,
        pub target: String,
        pub message: String,
    }

    /// `aether.log.batch` — a drained per-actor log buffer or a single
    /// host-emitted event. Mailed to the `"aether.log"` mailbox by the
    /// actor-aware subscriber (issue #581); fire-and-forget. The
    /// originating `MailboxId` rides on the mail envelope (cap reads
    /// `ctx.sender()`); empty `entries` is legal but produced by no
    /// path the SDK exposes.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.log.batch")]
    pub struct LogBatch {
        pub entries: Vec<LogEvent>,
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
        let back: Key = decode(&bytes).unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn mouse_move_roundtrip() {
        let m = MouseMove { x: 1.5, y: -3.25 };
        let bytes = encode(&m);
        assert_eq!(bytes.len(), 8);
        let back: MouseMove = decode(&bytes).unwrap();
        assert_eq!(back, m);
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
        let back: &[DrawTriangle] = decode_slice(&bytes).unwrap();
        assert_eq!(back, &tris);
    }

    #[test]
    fn kind_names_are_stable() {
        assert_eq!(Tick::NAME, "aether.tick");
        assert_eq!(Key::NAME, "aether.key");
        assert_eq!(KeyRelease::NAME, "aether.key_release");
        assert_eq!(MouseButton::NAME, "aether.mouse_button");
        assert_eq!(MouseMove::NAME, "aether.mouse_move");
        assert_eq!(DrawTriangle::NAME, "aether.draw_triangle");
        assert_eq!(FrameStats::NAME, "aether.observation.frame_stats");
        assert_eq!(Ping::NAME, "aether.ping");
        assert_eq!(Pong::NAME, "aether.pong");
        assert_eq!(LoadComponent::NAME, "aether.component.load");
        assert_eq!(ReplaceComponent::NAME, "aether.component.replace");
        assert_eq!(DropComponent::NAME, "aether.component.drop");
        assert_eq!(LoadResult::NAME, "aether.component.load_result");
        assert_eq!(DropResult::NAME, "aether.component.drop_result");
        assert_eq!(ReplaceResult::NAME, "aether.component.replace_result");
        assert_eq!(SubscribeInput::NAME, "aether.input.subscribe");
        assert_eq!(UnsubscribeInput::NAME, "aether.input.unsubscribe");
        assert_eq!(SubscribeInputResult::NAME, "aether.input.subscribe_result");
        assert_eq!(CaptureFrame::NAME, "aether.render.capture_frame");
        assert_eq!(
            CaptureFrameResult::NAME,
            "aether.render.capture_frame_result"
        );
        assert_eq!(SetWindowMode::NAME, "aether.window.set_mode");
        assert_eq!(SetWindowModeResult::NAME, "aether.window.set_mode_result");
        assert_eq!(SetWindowTitle::NAME, "aether.window.set_title");
        assert_eq!(SetWindowTitleResult::NAME, "aether.window.set_title_result");
        assert_eq!(Camera::NAME, "aether.camera");
        // ADR-0066: aether.camera.{create,destroy,set_active,set_mode,
        // orbit.set,topdown.set} kind-name asserts live in
        // `aether-camera`'s tests; aether.mesh.load lives in
        // `aether-mesh-viewer`'s tests. The view-proj sink contract
        // (`aether.camera`) stays here as a chassis primitive.
        assert_eq!(NoteOn::NAME, "aether.audio.note_on");
        assert_eq!(NoteOff::NAME, "aether.audio.note_off");
        assert_eq!(SetMasterGain::NAME, "aether.audio.set_master_gain");
        assert_eq!(
            SetMasterGainResult::NAME,
            "aether.audio.set_master_gain_result"
        );
        assert_eq!(Read::NAME, "aether.fs.read");
        assert_eq!(ReadResult::NAME, "aether.fs.read_result");
        assert_eq!(Write::NAME, "aether.fs.write");
        assert_eq!(WriteResult::NAME, "aether.fs.write_result");
        assert_eq!(Delete::NAME, "aether.fs.delete");
        assert_eq!(DeleteResult::NAME, "aether.fs.delete_result");
        assert_eq!(List::NAME, "aether.fs.list");
        assert_eq!(ListResult::NAME, "aether.fs.list_result");
    }

    #[test]
    fn frame_stats_roundtrip() {
        let s = FrameStats {
            frame: 120,
            triangles: 240,
        };
        let bytes = encode(&s);
        assert_eq!(bytes.len(), 16);
        let back: FrameStats = decode(&bytes).unwrap();
        assert_eq!(back, s);
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
            const { assert!(<FrameStats as CastEligible>::ELIGIBLE) };
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

        #[test]
        fn frame_stats_schema_is_two_u64_fields() {
            let SchemaType::Struct { repr_c, fields } = &<FrameStats as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert!(*repr_c);
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "frame");
            assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U64));
            assert_eq!(fields[1].name, "triangles");
            assert_eq!(fields[1].ty, SchemaType::Scalar(Primitive::U64));
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
            let bytes = postcard::to_allocvec(&r).unwrap();
            let back: Read = postcard::from_bytes(&bytes).unwrap();
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
            let bytes = postcard::to_allocvec(&r).unwrap();
            let back: ReadResult = postcard::from_bytes(&bytes).unwrap();
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
            let bytes = postcard::to_allocvec(&r).unwrap();
            let back: ReadResult = postcard::from_bytes(&bytes).unwrap();
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
            let bytes = postcard::to_allocvec(&e).unwrap();
            let back: FsError = postcard::from_bytes(&bytes).unwrap();
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
            let bytes = postcard::to_allocvec(&w).unwrap();
            let back: Write = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(back.bytes, vec![0xde, 0xad, 0xbe, 0xef]);
        }

        #[test]
        fn list_result_ok_roundtrip_echoes_namespace_and_prefix() {
            let r = ListResult::Ok {
                namespace: "save".to_string(),
                prefix: "slots/".to_string(),
                entries: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            };
            let bytes = postcard::to_allocvec(&r).unwrap();
            let back: ListResult = postcard::from_bytes(&bytes).unwrap();
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
            let bytes = postcard::to_allocvec(&r).unwrap();
            let back: WriteResult = postcard::from_bytes(&bytes).unwrap();
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
            let bytes = postcard::to_allocvec(&r).unwrap();
            let back: DeleteResult = postcard::from_bytes(&bytes).unwrap();
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
            let bytes = postcard::to_allocvec(&f).unwrap();
            let back: Fetch = postcard::from_bytes(&bytes).unwrap();
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
            let bytes = postcard::to_allocvec(&f).unwrap();
            let back: Fetch = postcard::from_bytes(&bytes).unwrap();
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
            let bytes = postcard::to_allocvec(&r).unwrap();
            let back: FetchResult = postcard::from_bytes(&bytes).unwrap();
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
            let bytes = postcard::to_allocvec(&r).unwrap();
            let back: FetchResult = postcard::from_bytes(&bytes).unwrap();
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
            let bytes = postcard::to_allocvec(&e).unwrap();
            let back: HttpError = postcard::from_bytes(&bytes).unwrap();
            match back {
                HttpError::InvalidUrl(s) => assert_eq!(s, "not a url"),
                other => panic!("expected InvalidUrl, got {other:?}"),
            }
        }

        #[test]
        fn http_error_adapter_carries_detail() {
            let e = HttpError::AdapterError("dns lookup failed".to_string());
            let bytes = postcard::to_allocvec(&e).unwrap();
            let back: HttpError = postcard::from_bytes(&bytes).unwrap();
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
                let bytes = postcard::to_allocvec(&e).unwrap();
                let back: HttpError = postcard::from_bytes(&bytes).unwrap();
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
                let bytes = postcard::to_allocvec(&m).unwrap();
                let back: HttpMethod = postcard::from_bytes(&bytes).unwrap();
                assert_eq!(back, m);
            }
        }
    }
}
