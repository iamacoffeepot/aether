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
    }
}
