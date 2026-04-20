//! aether-kinds: the substrate's own mail vocabulary. Imported by any
//! actor that wants to send mail to the substrate, receive mail the
//! substrate dispatches (tick, input), or consume the substrate's sink
//! kinds (draw_triangle). See ADR-0005.
//!
//! Kind ids are assigned at substrate boot via `Registry::register_kind`
//! and resolved by name at component init via the `resolve_kind` host
//! function. Consumers never depend on the id's numeric value — only on
//! the `NAME` constants on the `Kind` impls below.

#![no_std]

#[cfg(feature = "descriptors")]
extern crate alloc;

#[cfg(feature = "descriptors")]
pub mod descriptors;

use bytemuck::{Pod, Zeroable};

// Every kind below derives `Kind` (always) plus `Schema` (gated on the
// `descriptors` feature so wasm guests stay free of hub-protocol). The
// `Schema` impls feed `descriptors.rs`, which `Hello`-ships the kind
// vocabulary to the hub for agent-side encoding (ADR-0019).

/// Per-frame signal from the substrate's frame loop. Empty payload —
/// elapsed-time is parked until a subscriber actually needs it.
#[derive(aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
#[kind(name = "aether.tick", input)]
pub struct Tick;

/// A single keyboard keypress, identified by `winit::keyboard::KeyCode
/// as u32`. Dispatched on press only (not release, not repeat).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
#[kind(name = "aether.key", input)]
pub struct Key {
    pub code: u32,
}

/// A mouse-button press. No payload today — which button isn't tracked.
#[derive(aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
#[kind(name = "aether.mouse_button", input)]
pub struct MouseButton;

/// Cursor position in window coordinates, as logical pixels cast to f32.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
#[kind(name = "aether.mouse_move", input)]
pub struct MouseMove {
    pub x: f32,
    pub y: f32,
}

/// A single clip-space vertex with per-vertex color. Matches the
/// substrate's `VertexBufferLayout`: `(pos: vec2<f32>, color: vec3<f32>)`,
/// 20 bytes on the wire. Not a kind on its own — only addressable as
/// the element type inside `DrawTriangle.verts`. The `Schema` derive
/// is conditional so DrawTriangle's emitted schema can recurse into
/// it under `descriptors`; without the feature, neither type emits
/// schema or eligibility info.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// A draw-triangle item. One `DrawTriangle` is three vertices; the mail
/// `count` field is the number of triangles in the payload when
/// sent as a slice.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
#[kind(name = "aether.draw_triangle")]
pub struct DrawTriangle {
    pub verts: [Vertex; 3],
}

/// Request addressed to a component that supports the ADR-0013
/// reply-to-sender smoke path. The component answers with `Pong`
/// carrying the same `seq`; the round trip proves that a Claude
/// session → component → session reply actually works end-to-end.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
#[kind(name = "aether.ping")]
pub struct Ping {
    pub seq: u32,
}

/// Reply-to-sender counterpart to `Ping`. The `seq` is the incoming
/// `Ping.seq` echoed back so the caller can match requests against
/// replies when multiple are in flight.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
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
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
#[kind(name = "aether.observation.frame_stats")]
pub struct FrameStats {
    pub frame: u64,
    pub triangles: u64,
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

#[cfg(feature = "descriptors")]
pub use control_plane::*;

#[cfg(feature = "descriptors")]
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

    /// Reply to `LoadComponent`. `Ok` carries the assigned mailbox id
    /// and the resolved name (so callers that omitted `name` learn
    /// the substrate-defaulted one). `Err` carries the failure reason
    /// — kind-descriptor conflict, invalid WASM, name conflict, etc.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.load_result")]
    pub enum LoadResult {
        Ok { mailbox_id: u64, name: String },
        Err { error: String },
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

    /// Reply to `ReplaceComponent`. Same shape as `DropResult` —
    /// success or a free-form error string.
    #[derive(aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.control.replace_result")]
    pub enum ReplaceResult {
        Ok,
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
    /// atomic tool call. The render thread's existing
    /// `queue.wait_idle()` before the capture ensures every
    /// `mails` entry has been fully processed by the time the frame
    /// is read back. Empty vecs mean "just capture the current state"
    /// / "no cleanup".
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
            r: 1.0,
            g: 0.0,
            b: 0.0,
        };
        let tris = [
            DrawTriangle { verts: [v, v, v] },
            DrawTriangle { verts: [v, v, v] },
        ];
        let bytes = encode_slice(&tris);
        assert_eq!(bytes.len(), 2 * 60);
        let back: &[DrawTriangle] = decode_slice(&bytes).unwrap();
        assert_eq!(back, &tris);
    }

    #[test]
    fn kind_names_are_stable() {
        assert_eq!(Tick::NAME, "aether.tick");
        assert_eq!(Key::NAME, "aether.key");
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
    #[cfg(feature = "descriptors")]
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
            assert_eq!(nested_fields.len(), 5);
            assert_eq!(nested_fields[0].name, "x");
            assert_eq!(nested_fields[4].name, "b");
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
}
