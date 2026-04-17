// aether-substrate-mail: the substrate's own mail vocabulary. Imported
// by any actor that wants to send mail to the substrate, receive mail
// the substrate dispatches (tick, input), or consume the substrate's
// sink kinds (draw_triangle). See ADR-0005.
//
// Kind ids are assigned at substrate boot via `Registry::register_kind`
// and resolved by name at component init via the `resolve_kind` host
// function. Consumers never depend on the id's numeric value — only on
// the `NAME` constants on the `Kind` impls below.

#![no_std]

#[cfg(feature = "descriptors")]
extern crate alloc;

#[cfg(feature = "descriptors")]
pub mod descriptors;

use aether_mail::Kind;
use bytemuck::{Pod, Zeroable};

/// Per-frame signal from the substrate's frame loop. Empty payload for
/// now; milestone 4 will add an elapsed-seconds field.
pub struct Tick;
impl Kind for Tick {
    const NAME: &'static str = "aether.tick";
}

/// A single keyboard keypress, identified by `winit::keyboard::KeyCode
/// as u32`. Dispatched on press only (not release, not repeat).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Key {
    pub code: u32,
}
impl Kind for Key {
    const NAME: &'static str = "aether.key";
}

/// A mouse-button press. No payload today — which button isn't tracked.
pub struct MouseButton;
impl Kind for MouseButton {
    const NAME: &'static str = "aether.mouse_button";
}

/// Cursor position in window coordinates, as logical pixels cast to f32.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct MouseMove {
    pub x: f32,
    pub y: f32,
}
impl Kind for MouseMove {
    const NAME: &'static str = "aether.mouse_move";
}

/// A single clip-space vertex with per-vertex color. Matches the
/// substrate's `VertexBufferLayout`: `(pos: vec2<f32>, color: vec3<f32>)`,
/// 20 bytes on the wire.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable)]
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
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct DrawTriangle {
    pub verts: [Vertex; 3],
}
impl Kind for DrawTriangle {
    const NAME: &'static str = "aether.draw_triangle";
}

/// Request addressed to a component that supports the ADR-0013
/// reply-to-sender smoke path. The component answers with `Pong`
/// carrying the same `seq`; the round trip proves that a Claude
/// session → component → session reply actually works end-to-end.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Ping {
    pub seq: u32,
}
impl Kind for Ping {
    const NAME: &'static str = "aether.ping";
}

/// Reply-to-sender counterpart to `Ping`. The `seq` is the incoming
/// `Ping.seq` echoed back so the caller can match requests against
/// replies when multiple are in flight.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Pong {
    pub seq: u32,
}
impl Kind for Pong {
    const NAME: &'static str = "aether.pong";
}

/// Periodic observation emitted by the substrate's frame loop when a
/// hub is attached (ADR-0008). The substrate pushes one of these at
/// `LOG_EVERY_FRAMES` cadence to the `hub.claude.broadcast` sink, so
/// every attached Claude session learns how the engine is running
/// without having to poll the engine directly.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct FrameStats {
    pub frame: u64,
    pub triangles: u64,
}
impl Kind for FrameStats {
    const NAME: &'static str = "aether.observation.frame_stats";
}

// Reserved control-plane vocabulary (ADR-0010). The substrate handles
// these kinds inline rather than dispatching to a component — the
// namespace itself is the routing discriminator. Payloads are not Pod,
// so each is `Opaque`: the substrate-side handler decodes via postcard
// against substrate-internal types. Keeping them here (alongside the
// engine-facing kinds) means the hub's `describe_kinds` surfaces them
// uniformly and the init path registers them exactly once at boot.

/// `aether.control.load_component` — request the substrate load a WASM
/// component into a freshly allocated mailbox. Payload carries the WASM
/// bytes, any new kinds the component intends to use, and an optional
/// human-readable name. The substrate replies with `load_result`.
pub struct LoadComponent;
impl Kind for LoadComponent {
    const NAME: &'static str = "aether.control.load_component";
}

/// `aether.control.replace_component` — atomically rebind a target
/// mailbox id to a freshly instantiated component. Any mail queued on
/// the old instance at the moment of swap is dropped (V0 policy; drain
/// is an additive follow-up).
pub struct ReplaceComponent;
impl Kind for ReplaceComponent {
    const NAME: &'static str = "aether.control.replace_component";
}

/// `aether.control.drop_component` — remove a component from the
/// substrate and invalidate its mailbox id.
pub struct DropComponent;
impl Kind for DropComponent {
    const NAME: &'static str = "aether.control.drop_component";
}

/// `aether.control.load_result` — reply-to-sender emitted by the
/// substrate after handling `load_component`. Carries the assigned
/// mailbox id on success or an error describing why the load failed
/// (kind-descriptor conflict, invalid WASM, etc.).
pub struct LoadResult;
impl Kind for LoadResult {
    const NAME: &'static str = "aether.control.load_result";
}

/// `aether.control.drop_result` — reply-to-sender for `drop_component`.
/// Carries `Ok` on success or an error describing why the drop failed
/// (unknown mailbox, mailbox wasn't a component, already dropped).
pub struct DropResult;
impl Kind for DropResult {
    const NAME: &'static str = "aether.control.drop_result";
}

/// `aether.control.replace_result` — reply-to-sender for
/// `replace_component`. Carries `Ok` on success or an error if the
/// target mailbox was invalid, the new module failed to compile, or
/// instantiation failed.
pub struct ReplaceResult;
impl Kind for ReplaceResult {
    const NAME: &'static str = "aether.control.replace_result";
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_mail::{decode, decode_slice, encode, encode_slice};

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
}
