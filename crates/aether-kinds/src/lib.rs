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

// Every kind below derives both `Kind` and `Schema`. Pre-ADR-0032
// `Schema` was gated behind a `descriptors` feature so wasm guests
// stayed free of hub-protocol; that gate retired once hub-protocol
// went no_std + alloc. `Schema` drives both the canonical bytes the
// `aether.kinds` section carries and the `LABEL_NODE` sidecar — so
// it's load-bearing on every build, not an optional enrichment.

/// Per-frame signal from the substrate's frame loop. Empty payload —
/// elapsed-time is parked until a subscriber actually needs it.
///
/// ADR-0033 handler dispatch (`#[handlers]` synthesized
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
    aether_mail::Kind,
    aether_mail::Schema,
)]
#[kind(name = "aether.tick", input)]
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
    aether_mail::Kind,
    aether_mail::Schema,
)]
#[kind(name = "aether.key", input)]
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
    aether_mail::Kind,
    aether_mail::Schema,
)]
#[kind(name = "aether.key_release", input)]
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
    aether_mail::Kind,
    aether_mail::Schema,
)]
#[kind(name = "aether.mouse_button", input)]
pub struct MouseButton;

/// Cursor position in window coordinates, as logical pixels cast to f32.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.mouse_move", input)]
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
    aether_mail::Kind,
    aether_mail::Schema,
)]
#[kind(name = "aether.window_size", input)]
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
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Schema)]
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
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.draw_triangle")]
pub struct DrawTriangle {
    pub verts: [Vertex; 3],
}

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
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.camera")]
pub struct Camera {
    pub view_proj: [f32; 16],
}

/// Set the orbit camera's distance from its target (eye radius).
/// Typical values 1.0–100.0; 0.0 collapses to the target point and
/// produces a degenerate view. Applied on the next tick.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.camera.orbit.set_distance")]
pub struct OrbitSetDistance {
    pub distance: f32,
}

/// Set the orbit camera's pitch (radians). Positive tilts the eye
/// upward so the camera looks down; negative tilts down so it looks
/// up. Not clamped — `±π/2` are degenerate.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.camera.orbit.set_pitch")]
pub struct OrbitSetPitch {
    pub pitch: f32,
}

/// Set the orbit camera's absolute yaw (radians). Auto-advance still
/// ticks from this value on the next frame; pair with
/// `OrbitSetSpeed { rad_per_tick: 0.0 }` to pin a specific yaw.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.camera.orbit.set_yaw")]
pub struct OrbitSetYaw {
    pub yaw: f32,
}

/// Set the orbit camera's auto-rotation rate (radians per tick).
/// `0.0` freezes the camera at its current yaw. Negative reverses
/// direction.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.camera.orbit.set_speed")]
pub struct OrbitSetSpeed {
    pub rad_per_tick: f32,
}

/// Set the orbit camera's vertical field of view (radians). Typical
/// values `π/4` (45°) to `π/2` (90°).
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.camera.orbit.set_fov")]
pub struct OrbitSetFov {
    pub fov_y_rad: f32,
}

/// Set the world-space point the orbit camera orbits around (default
/// `(0, 0, 0)`). Useful for following an object by re-targeting the
/// camera each frame.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.camera.orbit.set_target")]
pub struct OrbitSetTarget {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Pan the top-down camera's centerpoint in world xy. Z is implicit —
/// the camera always looks down the `-Z` axis.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.camera.topdown.set_center")]
pub struct TopdownSetCenter {
    pub x: f32,
    pub y: f32,
}

/// Set the top-down camera's orthographic extent — the half-height of
/// the frustum in world units. The visible width is
/// `extent * aspect`. Larger values zoom out. Must be positive; zero
/// or negative degenerates the projection.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.camera.topdown.set_extent")]
pub struct TopdownSetExtent {
    pub extent: f32,
}

/// Teleport the player to a world-space position. Also zeroes velocity
/// isn't implied — send `PlayerSetVelocity { 0, 0 }` explicitly if you
/// want to stop motion at the new point.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.player.set_position")]
pub struct PlayerSetPosition {
    pub x: f32,
    pub y: f32,
}

/// Set the player's per-tick velocity in world units. `(0, 0)` stops
/// motion; values compose per-axis so `(1, 0)` drifts +x, `(0, -1)`
/// drifts -y.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.player.set_velocity")]
pub struct PlayerSetVelocity {
    pub vx: f32,
    pub vy: f32,
}

/// Switch the player between continuous and tile-step motion modes.
/// `0 = continuous` (WASD / `PlayerSetVelocity` drive per-tick
/// velocity — the original model). `1 = tile_step` (each WASD press
/// emits a `PlayerRequestStep` to the world authority; the player's
/// position changes only when it receives a `PlayerStepResult` back).
/// Other values are ignored.
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
    aether_mail::Kind,
    aether_mail::Schema,
)]
#[kind(name = "aether.player.set_mode")]
pub struct PlayerSetMode {
    pub mode: u32,
}

/// Player → world-authority request: "I want to step by `(dx, dy)`
/// world units — what actually happens?". Authority addressed by the
/// mailbox name `"world"` (resolved as a sink during player init).
/// Deltas are integer cell offsets in tile-step mode: `(+1, 0)` east,
/// `(0, +1)` north (matching the engine's +Y-up world). The player
/// does not apply the motion itself in tile-step mode — it waits for
/// `PlayerStepResult` before updating its position.
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
    aether_mail::Kind,
    aether_mail::Schema,
)]
#[kind(name = "aether.player.request_step")]
pub struct PlayerRequestStep {
    pub dx: i32,
    pub dy: i32,
}

/// World-authority → player reply to `PlayerRequestStep`. `accepted`
/// is `1` when the requested motion was applied (with any side
/// effects like a box push already committed on the world side) and
/// `0` when the authority refused (wall, out of bounds, unpushable
/// box, etc.). `new_x` / `new_y` are the player's post-resolution
/// world position either way — authoritative. The player overwrites
/// its own position from these fields, so a rejected step leaves the
/// player where the world says it is, not where it tried to go.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
)]
#[kind(name = "aether.player.step_result")]
pub struct PlayerStepResult {
    pub accepted: u32,
    pub new_x: f32,
    pub new_y: f32,
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
    aether_mail::Kind,
    aether_mail::Schema,
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
    aether_mail::Kind,
    aether_mail::Schema,
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
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema,
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
    aether_mail::Kind,
    aether_mail::Schema,
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
    aether_mail::Kind,
    aether_mail::Schema,
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
    aether_mail::Kind,
    aether_mail::Schema,
)]
#[kind(name = "aether.observation.frame_stats")]
pub struct FrameStats {
    pub frame: u64,
    pub triangles: u64,
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
    aether_mail::Kind,
    aether_mail::Schema,
)]
#[kind(name = "aether.mail.unresolved")]
pub struct UnresolvedMail {
    pub recipient_mailbox_id: u64,
    pub kind_id: u64,
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

mod control_plane {
    use alloc::string::String;
    use alloc::vec::Vec;

    use serde::{Deserialize, Serialize};

    /// `aether.control.load_component` — request the substrate load a
    /// WASM component into a freshly allocated mailbox. Carries the
    /// raw WASM bytes and an optional human-readable name. The
    /// component's kind vocabulary ships embedded in the wasm's
    /// `aether.kinds` custom section (ADR-0028) — the substrate
    /// reads it directly and the loader doesn't need to declare
    /// anything. Substrate replies with `LoadResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.load_component")]
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
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.load_result")]
    pub enum LoadResult {
        Ok {
            mailbox_id: u64,
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
    /// without the `#[handlers]` macro (ADR-0027 shape) — the hub can
    /// tell those apart from a truly empty receive surface.
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone, Default)]
    pub struct ComponentCapabilities {
        pub handlers: Vec<HandlerCapability>,
        pub fallback: Option<FallbackCapability>,
        pub doc: Option<String>,
    }

    /// One `#[handler]` method's advertised capability. `id` is the
    /// compile-time `<K as Kind>::ID` (ADR-0030); `name` is `K::NAME`;
    /// `doc` carries the author's rustdoc filtered through the
    /// `# Agent` section convention when present, else the full doc.
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct HandlerCapability {
        pub id: u64,
        pub name: String,
        pub doc: Option<String>,
    }

    /// A `#[fallback]` method's advertised presence + optional doc.
    /// Components without a fallback are strict receivers; absence of
    /// this field on `ComponentCapabilities` means "no catchall — mail
    /// for unhandled kinds will land as `DISPATCH_UNKNOWN_KIND`".
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct FallbackCapability {
        pub doc: Option<String>,
    }

    /// `aether.control.drop_component` — remove a component from the
    /// substrate and invalidate its mailbox id. Reply: `DropResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.drop_component")]
    pub struct DropComponent {
        pub mailbox_id: u64,
    }

    /// Reply to `DropComponent`. `Ok` on success; `Err` if the
    /// mailbox was unknown, wasn't a component, or already dropped.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.drop_result")]
    pub enum DropResult {
        Ok,
        Err { error: String },
    }

    /// `aether.control.replace_component` — atomically rebind a target
    /// mailbox id to a freshly instantiated component. ADR-0022: the
    /// substrate freezes the target, drains in-flight mail through
    /// the old instance, then swaps. If the drain exceeds
    /// `drain_timeout_ms` (default 5000) the replace fails with
    /// `ReplaceResult::Err` and the old instance stays bound. Kind
    /// vocabulary rides in the wasm's `aether.kinds` custom section
    /// (ADR-0028). Reply: `ReplaceResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.replace_component")]
    pub struct ReplaceComponent {
        pub mailbox_id: u64,
        pub wasm: Vec<u8>,
        pub drain_timeout_ms: Option<u32>,
    }

    /// Reply to `ReplaceComponent`. Carries the new component's
    /// advertised capabilities on `Ok` so the hub's cached state
    /// reflects the swapped binary; `Err` carries a free-form reason.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.replace_result")]
    pub enum ReplaceResult {
        Ok { capabilities: ComponentCapabilities },
        Err { error: String },
    }

    // ADR-0021 publish/subscribe routing for substrate input streams.
    // Closed enum over streams the platform layer publishes; a
    // SubscribeInput names one and a mailbox to receive it. Reserved
    // kind names `aether.control.subscribe_input` /
    // `aether.control.unsubscribe_input` / `aether.control.subscribe_input_result`
    // match the namespace used for load/drop/replace; the substrate
    // handles them inline and replies via reply-to-sender.

    /// A substrate-published input stream (ADR-0021). Closed set —
    /// adding a platform event (e.g. `Resize`) is an additive variant
    /// plus a publisher change on the substrate side.
    #[derive(
        aether_mail::Schema,
        Serialize,
        Deserialize,
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        Hash,
        PartialOrd,
        Ord,
    )]
    pub enum InputStream {
        Tick,
        Key,
        MouseMove,
        MouseButton,
        WindowSize,
        KeyRelease,
    }

    /// `aether.control.subscribe_input` — add `mailbox` to the
    /// subscriber set for `stream`. Idempotent: subscribing a mailbox
    /// already in the set is still `Ok` (subscriptions are a set, not
    /// a counter). Reply: `SubscribeInputResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.subscribe_input")]
    pub struct SubscribeInput {
        pub stream: InputStream,
        pub mailbox: u64,
    }

    /// `aether.control.unsubscribe_input` — remove `mailbox` from the
    /// subscriber set for `stream`. Idempotent: unsubscribing a mailbox
    /// that isn't subscribed is still `Ok`. Reply:
    /// `SubscribeInputResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.unsubscribe_input")]
    pub struct UnsubscribeInput {
        pub stream: InputStream,
        pub mailbox: u64,
    }

    /// Reply to both subscribe and unsubscribe (ADR-0021 §2). Only
    /// failure mode: the target mailbox id doesn't name a live
    /// component (unknown, a sink, or already dropped).
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.subscribe_input_result")]
    pub enum SubscribeInputResult {
        Ok,
        Err { error: String },
    }

    /// `aether.control.capture_frame` — request the substrate grab the
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
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.capture_frame")]
    pub struct CaptureFrame {
        pub mails: Vec<MailEnvelope>,
        pub after_mails: Vec<MailEnvelope>,
    }

    /// One mail in a `CaptureFrame.mails` bundle. Structurally mirrors
    /// `aether_hub_protocol::MailFrame` — a pre-encoded payload plus
    /// the name-level addressing the substrate uses to resolve it.
    /// The hub encodes each envelope's `payload` via the kind's
    /// descriptor before wrapping it into the bundle, so the
    /// substrate side just pushes `Mail::new(mailbox, kind_id,
    /// payload, count)` directly.
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
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
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.capture_frame_result")]
    pub enum CaptureFrameResult {
        Ok { png: Vec<u8> },
        Err { error: String },
    }

    /// `aether.control.platform_info` — request a one-shot snapshot of
    /// the host environment the substrate is running on: OS + engine
    /// build + GPU adapter + monitors with video modes + current
    /// window state. Empty payload; reply is `PlatformInfoResult`.
    ///
    /// Fat-query design: static environment (OS / GPU) and live state
    /// (window mode / size) ride together in one snapshot. Callers
    /// that mutate state (`set_window_mode`) get the new state in the
    /// mutation's reply, so polling `platform_info` after every
    /// change isn't necessary.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.platform_info")]
    pub struct PlatformInfo;

    /// Reply to `PlatformInfo`. `Err` is reserved for snapshot
    /// failures that the substrate can articulate (e.g. monitor
    /// enumeration failed) — today the happy path is essentially
    /// infallible, but keeping the variant leaves room to surface
    /// platform-specific issues without widening the kind later.
    ///
    /// `Ok` holds far more data than `Err`; the clippy lint is
    /// accurate but the value is constructed once per request,
    /// serialized, and dropped, so the in-memory enum-tag cost is
    /// not a concern.
    #[allow(clippy::large_enum_variant)]
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.platform_info_result")]
    pub enum PlatformInfoResult {
        Ok {
            os: OsInfo,
            engine: EngineInfo,
            gpu: GpuInfo,
            monitors: Vec<MonitorInfo>,
            /// `None` before winit's `resumed` callback fires — there's
            /// no window yet. After first resume this is populated for
            /// the life of the process.
            window: Option<WindowInfo>,
        },
        Err {
            error: String,
        },
    }

    /// Host OS identification. `name` / `arch` come from
    /// `std::env::consts` (lowercase short names — `"macos"`,
    /// `"linux"`, `"windows"`; `"aarch64"` / `"x86_64"`); `version`
    /// is sourced from the `os_info` crate and is platform-formatted
    /// (e.g. `"14.5"`, `"22.04"`).
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct OsInfo {
        pub name: String,
        pub version: String,
        pub arch: String,
    }

    /// Engine-side build identification. `version` is the substrate
    /// crate's `CARGO_PKG_VERSION`; `workers` is the scheduler's
    /// configured worker count; `kinds_count` is the number of kinds
    /// registered at boot (ADR-0010 load-time additions aren't
    /// included — this is a static boot-time fingerprint).
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct EngineInfo {
        pub version: String,
        pub workers: u32,
        pub kinds_count: u32,
    }

    /// wgpu adapter identification plus the limits most agents reach
    /// for when planning work. Values are the ones wgpu reports; ids
    /// are the raw `AdapterInfo::vendor` / `device` integers (PCI
    /// ids on desktop GPUs, zero on software adapters).
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct GpuInfo {
        pub name: String,
        pub vendor_id: u32,
        pub device_id: u32,
        pub device_type: GpuDeviceType,
        pub backend: GpuBackend,
        pub driver: String,
        pub driver_info: String,
        pub max_texture_dim_2d: u32,
        pub max_buffer_size: u64,
        pub max_bind_groups: u32,
    }

    /// Mirror of `wgpu::DeviceType`. Kept as its own enum so the
    /// kind's schema doesn't depend on wgpu version churn and so
    /// agents see the same variant names on every platform.
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GpuDeviceType {
        Other,
        IntegratedGpu,
        DiscreteGpu,
        VirtualGpu,
        Cpu,
    }

    /// Mirror of `wgpu::Backend`. Like `GpuDeviceType`, independent
    /// of wgpu's enum so the wire shape is stable.
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GpuBackend {
        Noop,
        Vulkan,
        Metal,
        Dx12,
        Gl,
        BrowserWebGpu,
    }

    /// One monitor attached to the host. `position_x` / `position_y`
    /// are the monitor's top-left in desktop coordinates; `width` /
    /// `height` are the monitor's current resolution in physical
    /// pixels. `current_mode` is `None` if winit couldn't determine
    /// the active mode. `modes` is the full list winit reported —
    /// callers pick one for `FullscreenExclusive`.
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct MonitorInfo {
        pub name: Option<String>,
        pub is_primary: bool,
        pub position_x: i32,
        pub position_y: i32,
        pub width: u32,
        pub height: u32,
        pub scale_factor: f64,
        pub current_mode: Option<VideoMode>,
        pub modes: Vec<VideoMode>,
    }

    /// A single video mode a monitor supports. `refresh_mhz` is
    /// winit's millihertz unit (exact rational — divide by 1000 for
    /// Hz). `bit_depth` is the per-channel count winit reports.
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VideoMode {
        pub width: u32,
        pub height: u32,
        pub refresh_mhz: u32,
        pub bit_depth: u16,
    }

    /// Current window state. `monitor_index` points into the
    /// `monitors` vec on the same reply; `None` if winit couldn't
    /// resolve a current monitor (rare).
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct WindowInfo {
        pub mode: WindowMode,
        pub width: u32,
        pub height: u32,
        pub scale_factor: f64,
        pub monitor_index: Option<u32>,
    }

    /// The three window presentation modes. `Windowed` has no fields —
    /// the current size lives on `WindowInfo` / `SetWindowModeResult`.
    /// `FullscreenExclusive` carries the specific video mode; the
    /// substrate matches against the active monitor's supported modes
    /// and fails the request if none matches (loud rather than
    /// silently falling back).
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum WindowMode {
        Windowed,
        FullscreenBorderless,
        FullscreenExclusive {
            width: u32,
            height: u32,
            refresh_mhz: u32,
        },
    }

    /// `aether.control.set_window_mode` — switch the substrate's
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
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.set_window_mode")]
    pub struct SetWindowMode {
        pub mode: WindowMode,
        pub width: Option<u32>,
        pub height: Option<u32>,
    }

    /// Reply to `SetWindowMode`. `Ok` carries the resolved state
    /// after the mode change applied; `Err` carries the reason the
    /// request was rejected (unknown video mode, window not ready,
    /// etc.) with no state change.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.set_window_mode_result")]
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

    /// `aether.control.set_window_title` — update the substrate
    /// window's title at runtime. `winit::Window::set_title` is
    /// infallible on every supported platform, so the desktop reply
    /// always echoes the applied title back on `Ok`. Headless and hub
    /// chassis reply `Err { error: "unsupported on headless..." }`.
    /// Boot-time default comes from `AETHER_WINDOW_TITLE`; unset falls
    /// back to the substrate's name.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.set_window_title")]
    pub struct SetWindowTitle {
        pub title: String,
    }

    /// Reply to `SetWindowTitle`. `Ok` echoes the applied title — same
    /// value the caller sent, returned so MCP logs and agent memory
    /// see the resulting state in one place. `Err` is reserved for
    /// chassis that don't own a window (headless, hub) or for a
    /// pre-window-ready request.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.set_window_title_result")]
    pub enum SetWindowTitleResult {
        Ok { title: String },
        Err { error: String },
    }

    /// Reply to `SetMasterGain` (ADR-0039). `Ok` echoes the gain the
    /// substrate actually applied — values above `1.0` are clamped, so
    /// callers that sent `1.5` learn they got `1.0`. `Err` fires on
    /// chassis without an audio device (headless, hub) or when audio
    /// was disabled at boot via `AETHER_AUDIO_DISABLE`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.audio.set_master_gain_result")]
    pub enum SetMasterGainResult {
        Ok { applied_gain: f32 },
        Err { error: String },
    }

    // ADR-0041 substrate file I/O. Four request kinds on the `"io"`
    // sink (read / write / delete / list), paired 1:1 with reply kinds
    // that carry a structured `IoError` on failure. All postcard-
    // shaped because every request carries String namespace/path
    // fields and writes carry `Vec<u8>` bytes.
    //
    // `namespace` is the logical prefix without the `://`: mail
    // carries `"save"`, not `"save://"`. Paths are relative to the
    // namespace root; `..` and absolute prefixes are rejected at the
    // adapter boundary as `IoError::Forbidden`.

    /// Structured failure reason for an I/O request (ADR-0041 §1).
    /// Components can pattern-match on the variant to decide whether
    /// to retry (`AdapterError`), prompt the user (`NotFound`), or
    /// surface a bug (`Forbidden` / `UnknownNamespace`). `AdapterError`
    /// preserves backend-specific detail as free-form text — e.g.
    /// permission-denied text from the OS, an HTTP status from a
    /// future cloud adapter — without locking the enum shape to any
    /// one backend.
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum IoError {
        NotFound,
        Forbidden,
        UnknownNamespace,
        AdapterError(String),
    }

    /// `aether.io.read` — request the substrate read a file and reply
    /// with its bytes. Mailed to the `"io"` sink; reply lands via
    /// `reply_mail` as `ReadResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.io.read")]
    pub struct Read {
        pub namespace: String,
        pub path: String,
    }

    /// Reply to `Read`. Both arms echo the `namespace` + `path` from
    /// the originating `Read` so the caller can correlate the reply
    /// to its source request without threading a pending-op queue or
    /// allocating correlation ids — operation identity comes from the
    /// reply kind itself (`aether.io.read_result`), target identity
    /// from the echoed fields. `Ok` carries the full file contents;
    /// `Err` carries an `IoError` variant.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.io.read_result")]
    pub enum ReadResult {
        Ok {
            namespace: String,
            path: String,
            bytes: Vec<u8>,
        },
        Err {
            namespace: String,
            path: String,
            error: IoError,
        },
    }

    /// `aether.io.write` — request the substrate write `bytes` to
    /// `namespace://path`. v1's local-file adapter stages to a
    /// temporary sibling and `rename`s on success so a crash
    /// mid-write leaves either the old contents or the new, never a
    /// torn file. Reply: `WriteResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.io.write")]
    pub struct Write {
        pub namespace: String,
        pub path: String,
        pub bytes: Vec<u8>,
    }

    /// Reply to `Write`. Both arms echo `namespace` + `path` for
    /// correlation; the request's `bytes` field is *not* echoed so the
    /// reply payload stays small even when the write was megabytes
    /// (correlation needs the identity of the write, not its contents).
    /// `Err` carries an `IoError` — `Forbidden` for read-only
    /// namespaces (e.g. `assets://`), `AdapterError` for disk-full /
    /// permission / rename failures.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.io.write_result")]
    pub enum WriteResult {
        Ok {
            namespace: String,
            path: String,
        },
        Err {
            namespace: String,
            path: String,
            error: IoError,
        },
    }

    /// `aether.io.delete` — request the substrate remove a file.
    /// Missing files surface as `NotFound` (not silent success) so
    /// callers that care about the distinction can tell; callers
    /// that don't ignore it. Reply: `DeleteResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.io.delete")]
    pub struct Delete {
        pub namespace: String,
        pub path: String,
    }

    /// Reply to `Delete`. Both arms echo `namespace` + `path` for
    /// correlation. `Ok` on successful removal; `Err` on any
    /// adapter-reported failure, including `NotFound` for a file that
    /// wasn't there to delete.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.io.delete_result")]
    pub enum DeleteResult {
        Ok {
            namespace: String,
            path: String,
        },
        Err {
            namespace: String,
            path: String,
            error: IoError,
        },
    }

    /// `aether.io.list` — enumerate entries under `prefix` in
    /// `namespace`. Shallow (no recursion) and prefix-filtered —
    /// callers that want a tree walk paginate themselves. Empty
    /// `prefix` lists the namespace root. Reply: `ListResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.io.list")]
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
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.io.list_result")]
    pub enum ListResult {
        Ok {
            namespace: String,
            prefix: String,
            entries: Vec<String>,
        },
        Err {
            namespace: String,
            prefix: String,
            error: IoError,
        },
    }

    // ADR-0043 substrate HTTP egress. One request kind + one reply
    // kind on the `"net"` sink, plus supporting `HttpMethod`,
    // `HttpHeader`, and `NetError` shapes. All postcard-shaped
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
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
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
    /// `aether_mail::Schema` has no blanket impl for tuples — if
    /// that lands later the wire shape here is source-compatible
    /// (same two fields in the same order).
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct HttpHeader {
        pub name: String,
        pub value: String,
    }

    /// Structured failure reason for a net request (ADR-0043 §1).
    /// Typed variants cover the branches agents routinely need to
    /// match on — `Timeout` → retry, `AllowlistDenied` → config
    /// issue, `BodyTooLarge` → chunk the response, `Disabled` →
    /// surface to the operator. `InvalidUrl` carries the offending
    /// URL text; `AdapterError` is the catchall preserving backend-
    /// specific detail (DNS failure, TLS handshake, connection
    /// refused, etc.) as free-form text.
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum NetError {
        InvalidUrl(String),
        Timeout,
        BodyTooLarge,
        AllowlistDenied,
        Disabled,
        AdapterError(String),
    }

    /// `aether.net.fetch` — request the substrate perform an HTTP
    /// request and reply with the response. Mailed to the `"net"`
    /// sink; reply lands via `reply_mail` as `FetchResult`.
    /// `timeout_ms` overrides the chassis default
    /// (`AETHER_NET_TIMEOUT_MS`, default 30000) when set; `None`
    /// uses the default.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.net.fetch")]
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
    /// kind itself (`aether.net.fetch_result`). Request `body` is
    /// deliberately not echoed: correlation needs the identity of
    /// the request, not its contents, and a multi-MB upload should
    /// not round-trip. `Ok` carries the HTTP status, response
    /// headers, and response body (bounded by
    /// `AETHER_NET_MAX_BODY_BYTES`, default 16MB); `Err` carries a
    /// `NetError` variant.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.net.fetch_result")]
    pub enum FetchResult {
        Ok {
            url: String,
            status: u16,
            headers: Vec<HttpHeader>,
            body: Vec<u8>,
        },
        Err {
            url: String,
            error: NetError,
        },
    }

    // ADR-0045 typed-handle store. Four request kinds on the
    // `"handle"` sink (`publish` / `release` / `pin` / `unpin`),
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
    /// `IoError` / `NetError`'s tagged-enum shape so guests can
    /// pattern-match on the variant rather than parsing strings.
    #[derive(aether_mail::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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
        /// the same reasons `IoError::AdapterError` is.
        AdapterError(String),
    }

    /// `aether.handle.publish` — request the substrate stash
    /// `bytes` in the handle store under `kind_id` and reply with
    /// a fresh ephemeral id. Mailed to the `"handle"` sink; reply
    /// lands as `HandlePublishResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.publish")]
    pub struct HandlePublish {
        pub kind_id: u64,
        pub bytes: Vec<u8>,
    }

    /// Reply to `HandlePublish`. Both arms echo the originating
    /// `kind_id` for correlation; `Ok` carries the minted `id`.
    /// The request's `bytes` aren't echoed — correlation needs the
    /// identity of the publish, not its contents.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.publish_result")]
    pub enum HandlePublishResult {
        Ok { kind_id: u64, id: u64 },
        Err { kind_id: u64, error: HandleError },
    }

    /// `aether.handle.release` — drop one reference on `id`. Reply:
    /// `HandleReleaseResult`. The substrate's `dec_ref` saturates
    /// at zero, so calling release on an already-released handle
    /// is a no-op success rather than `UnknownHandle`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.release")]
    pub struct HandleRelease {
        pub id: u64,
    }

    /// Reply to `HandleRelease`. Both arms echo the originating
    /// `id`. `Err` only fires when no entry exists at that id.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.release_result")]
    pub enum HandleReleaseResult {
        Ok { id: u64 },
        Err { id: u64, error: HandleError },
    }

    /// `aether.handle.pin` — protect `id` from LRU eviction even
    /// when its refcount drops to zero. Reply: `HandlePinResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.pin")]
    pub struct HandlePin {
        pub id: u64,
    }

    /// Reply to `HandlePin`. Both arms echo the originating `id`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.pin_result")]
    pub enum HandlePinResult {
        Ok { id: u64 },
        Err { id: u64, error: HandleError },
    }

    /// `aether.handle.unpin` — clear the pinned flag on `id`.
    /// Doesn't drop the entry; only makes it eligible for LRU
    /// eviction once `refcount == 0`. Reply: `HandleUnpinResult`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.unpin")]
    pub struct HandleUnpin {
        pub id: u64,
    }

    /// Reply to `HandleUnpin`. Both arms echo the originating `id`.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.handle.unpin_result")]
    pub enum HandleUnpinResult {
        Ok { id: u64 },
        Err { id: u64, error: HandleError },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_mail::{Kind, decode, decode_slice, encode, encode_slice};

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
        assert_eq!(LoadComponent::NAME, "aether.control.load_component");
        assert_eq!(ReplaceComponent::NAME, "aether.control.replace_component");
        assert_eq!(DropComponent::NAME, "aether.control.drop_component");
        assert_eq!(LoadResult::NAME, "aether.control.load_result");
        assert_eq!(DropResult::NAME, "aether.control.drop_result");
        assert_eq!(ReplaceResult::NAME, "aether.control.replace_result");
        assert_eq!(SubscribeInput::NAME, "aether.control.subscribe_input");
        assert_eq!(UnsubscribeInput::NAME, "aether.control.unsubscribe_input");
        assert_eq!(
            SubscribeInputResult::NAME,
            "aether.control.subscribe_input_result"
        );
        assert_eq!(CaptureFrame::NAME, "aether.control.capture_frame");
        assert_eq!(
            CaptureFrameResult::NAME,
            "aether.control.capture_frame_result"
        );
        assert_eq!(PlatformInfo::NAME, "aether.control.platform_info");
        assert_eq!(
            PlatformInfoResult::NAME,
            "aether.control.platform_info_result"
        );
        assert_eq!(SetWindowMode::NAME, "aether.control.set_window_mode");
        assert_eq!(
            SetWindowModeResult::NAME,
            "aether.control.set_window_mode_result"
        );
        assert_eq!(SetWindowTitle::NAME, "aether.control.set_window_title");
        assert_eq!(
            SetWindowTitleResult::NAME,
            "aether.control.set_window_title_result"
        );
        assert_eq!(Camera::NAME, "aether.camera");
        assert_eq!(OrbitSetDistance::NAME, "aether.camera.orbit.set_distance");
        assert_eq!(OrbitSetPitch::NAME, "aether.camera.orbit.set_pitch");
        assert_eq!(OrbitSetYaw::NAME, "aether.camera.orbit.set_yaw");
        assert_eq!(OrbitSetSpeed::NAME, "aether.camera.orbit.set_speed");
        assert_eq!(OrbitSetFov::NAME, "aether.camera.orbit.set_fov");
        assert_eq!(OrbitSetTarget::NAME, "aether.camera.orbit.set_target");
        assert_eq!(TopdownSetCenter::NAME, "aether.camera.topdown.set_center");
        assert_eq!(TopdownSetExtent::NAME, "aether.camera.topdown.set_extent");
        assert_eq!(PlayerSetPosition::NAME, "aether.player.set_position");
        assert_eq!(PlayerSetVelocity::NAME, "aether.player.set_velocity");
        assert_eq!(PlayerSetMode::NAME, "aether.player.set_mode");
        assert_eq!(PlayerRequestStep::NAME, "aether.player.request_step");
        assert_eq!(PlayerStepResult::NAME, "aether.player.step_result");
        assert_eq!(NoteOn::NAME, "aether.audio.note_on");
        assert_eq!(NoteOff::NAME, "aether.audio.note_off");
        assert_eq!(SetMasterGain::NAME, "aether.audio.set_master_gain");
        assert_eq!(
            SetMasterGainResult::NAME,
            "aether.audio.set_master_gain_result"
        );
        assert_eq!(Read::NAME, "aether.io.read");
        assert_eq!(ReadResult::NAME, "aether.io.read_result");
        assert_eq!(Write::NAME, "aether.io.write");
        assert_eq!(WriteResult::NAME, "aether.io.write_result");
        assert_eq!(Delete::NAME, "aether.io.delete");
        assert_eq!(DeleteResult::NAME, "aether.io.delete_result");
        assert_eq!(List::NAME, "aether.io.list");
        assert_eq!(ListResult::NAME, "aether.io.list_result");
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
        use aether_hub_protocol::{Primitive, SchemaType};
        use aether_mail::{CastEligible, Schema};

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
    // wrapping `IoError`. postcard roundtrip proves the derived
    // Serialize/Deserialize agree on the wire for each shape.
    mod io_roundtrips {
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
                error: IoError::NotFound,
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
                    assert_eq!(error, IoError::NotFound);
                }
                ReadResult::Ok { .. } => panic!("expected Err"),
            }
        }

        #[test]
        fn io_error_adapter_carries_payload() {
            let e = IoError::AdapterError("disk full".to_string());
            let bytes = postcard::to_allocvec(&e).unwrap();
            let back: IoError = postcard::from_bytes(&bytes).unwrap();
            match back {
                IoError::AdapterError(msg) => assert_eq!(msg, "disk full"),
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
                error: IoError::NotFound,
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
                    assert_eq!(error, IoError::NotFound);
                }
                DeleteResult::Ok { .. } => panic!("expected Err"),
            }
        }
    }

    // ADR-0043 net kind roundtrips. `Fetch` carries String + typed
    // method + Vec<HttpHeader> + Vec<u8> body + Option<u32>;
    // `FetchResult` mirrors `ReadResult`'s Ok/Err split with a
    // typed error arm wrapping `NetError`. Tests prove the derived
    // Serialize/Deserialize agree on the wire for each shape, with
    // special attention to the `body`-not-echoed invariant and the
    // payload-carrying `NetError` variants.
    mod net_roundtrips {
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
        fn fetch_result_err_roundtrip_echoes_url_and_net_error() {
            let r = FetchResult::Err {
                url: "https://api.example.com/gone".to_string(),
                error: NetError::Timeout,
            };
            let bytes = postcard::to_allocvec(&r).unwrap();
            let back: FetchResult = postcard::from_bytes(&bytes).unwrap();
            match back {
                FetchResult::Err { url, error } => {
                    assert_eq!(url, "https://api.example.com/gone");
                    assert_eq!(error, NetError::Timeout);
                }
                FetchResult::Ok { .. } => panic!("expected Err"),
            }
        }

        #[test]
        fn net_error_invalid_url_carries_payload() {
            let e = NetError::InvalidUrl("not a url".to_string());
            let bytes = postcard::to_allocvec(&e).unwrap();
            let back: NetError = postcard::from_bytes(&bytes).unwrap();
            match back {
                NetError::InvalidUrl(s) => assert_eq!(s, "not a url"),
                other => panic!("expected InvalidUrl, got {other:?}"),
            }
        }

        #[test]
        fn net_error_adapter_carries_detail() {
            let e = NetError::AdapterError("dns lookup failed".to_string());
            let bytes = postcard::to_allocvec(&e).unwrap();
            let back: NetError = postcard::from_bytes(&bytes).unwrap();
            match back {
                NetError::AdapterError(s) => assert_eq!(s, "dns lookup failed"),
                other => panic!("expected AdapterError, got {other:?}"),
            }
        }

        #[test]
        fn net_error_unit_variants_roundtrip() {
            for e in [
                NetError::Timeout,
                NetError::BodyTooLarge,
                NetError::AllowlistDenied,
                NetError::Disabled,
            ] {
                let bytes = postcard::to_allocvec(&e).unwrap();
                let back: NetError = postcard::from_bytes(&bytes).unwrap();
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
