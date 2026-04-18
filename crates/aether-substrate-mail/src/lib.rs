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

// ADR-0019 PR 3: every cast-shaped kind below moves to
// `#[derive(Kind)]` (always) plus `#[derive(Schema)]` (gated on the
// `descriptors` feature so wasm guests stay free of hub-protocol).
// Wire format is unchanged in this PR — descriptors.rs still emits the
// legacy `Pod`/`Signal`/`Opaque` arms. The `Schema` impls land here so
// the substrate's dispatch path (PR 4) and the hub encoder (PR 5) have
// something to call into without another round of boilerplate.

/// Per-frame signal from the substrate's frame loop. Empty payload for
/// now; milestone 4 will add an elapsed-seconds field.
#[derive(aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
#[kind(name = "aether.tick")]
pub struct Tick;

/// A single keyboard keypress, identified by `winit::keyboard::KeyCode
/// as u32`. Dispatched on press only (not release, not repeat).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
#[kind(name = "aether.key")]
pub struct Key {
    pub code: u32,
}

/// A mouse-button press. No payload today — which button isn't tracked.
#[derive(aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
#[kind(name = "aether.mouse_button")]
pub struct MouseButton;

/// Cursor position in window coordinates, as logical pixels cast to f32.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_mail::Kind)]
#[cfg_attr(feature = "descriptors", derive(aether_mail::Schema))]
#[kind(name = "aether.mouse_move")]
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
            assert!(matches!(<Tick as Schema>::schema(), SchemaType::Unit));
            assert!(matches!(
                <MouseButton as Schema>::schema(),
                SchemaType::Unit
            ));
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
            let SchemaType::Struct { repr_c, fields } = <Key as Schema>::schema() else {
                panic!("expected Struct");
            };
            assert!(repr_c);
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "code");
            assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
        }

        #[test]
        fn draw_triangle_schema_recurses_into_vertex() {
            let SchemaType::Struct { repr_c, fields } = <DrawTriangle as Schema>::schema() else {
                panic!("expected Struct");
            };
            assert!(repr_c);
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "verts");
            let SchemaType::Array { element, len } = &fields[0].ty else {
                panic!("expected Array");
            };
            assert_eq!(*len, 3);
            let SchemaType::Struct {
                repr_c: nested_repr,
                fields: nested_fields,
            } = element.as_ref()
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
            let SchemaType::Struct { repr_c, fields } = <FrameStats as Schema>::schema() else {
                panic!("expected Struct");
            };
            assert!(repr_c);
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "frame");
            assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U64));
            assert_eq!(fields[1].name, "triangles");
            assert_eq!(fields[1].ty, SchemaType::Scalar(Primitive::U64));
        }
    }
}
