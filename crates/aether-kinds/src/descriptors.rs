// Wire descriptors for the substrate's kinds. Consumed by the native
// substrate binary and shipped to the hub at `Hello` per ADR-0007 so
// the hub can encode agent-supplied params for each kind.
//
// ADR-0019 PR 5: every substrate kind, including the control-plane
// vocabulary, ships as `KindEncoding::Schema(T::schema())`. There are
// no `Opaque` kinds left in the substrate's descriptor list — every
// kind is hub-encodable from agent params, and the `payload_bytes`
// escape hatch has been removed from the MCP `send_mail` tool.
// Adding or renaming a field on a kind is a one-place change (the
// struct itself); the schema is whatever the derive emits.

use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use aether_hub_protocol::KindDescriptor;
use aether_mail::{Kind, Schema};

use crate::{
    CaptureFrame, CaptureFrameResult, DrawTriangle, DropComponent, DropResult, FrameStats, Key,
    LoadComponent, LoadResult, MouseButton, MouseMove, Ping, PlatformInfo, PlatformInfoResult,
    Pong, ReplaceComponent, ReplaceResult, SetWindowMode, SetWindowModeResult, SetWindowTitle,
    SetWindowTitleResult, SubscribeInput, SubscribeInputResult, Tick, UnresolvedMail,
    UnsubscribeInput, WindowSize,
};

/// Every kind the substrate exposes, in the order the `Registry` will
/// register them. Caller ignores the order — names are the contract.
pub fn all() -> Vec<KindDescriptor> {
    vec![
        schema::<Tick>(),
        schema::<Key>(),
        schema::<MouseButton>(),
        schema::<MouseMove>(),
        schema::<WindowSize>(),
        // DrawTriangle's schema recurses into Vertex; the cast wire
        // format keeps today's bytes (the hub encoder treats the
        // nested `Struct { repr_c: true }` exactly like a flat Pod).
        schema::<DrawTriangle>(),
        schema::<FrameStats>(),
        // Hub → originating-engine diagnostic when a bubbled-up mail
        // doesn't resolve at the hub either (ADR-0037 follow-up,
        // issue #185). Delivered to the engine's `aether.diagnostics`
        // sink, which re-warns locally.
        schema::<UnresolvedMail>(),
        // ADR-0013 smoke-test vocabulary.
        schema::<Ping>(),
        schema::<Pong>(),
        // ADR-0010 control-plane vocabulary — now real schemas. The
        // hub encodes LoadComponent / ReplaceComponent / etc. from
        // agent params; the substrate decodes via postcard. No more
        // `payload_bytes` workaround.
        schema::<LoadComponent>(),
        schema::<ReplaceComponent>(),
        schema::<DropComponent>(),
        schema::<LoadResult>(),
        schema::<DropResult>(),
        schema::<ReplaceResult>(),
        // ADR-0021 publish/subscribe routing for input streams.
        schema::<SubscribeInput>(),
        schema::<UnsubscribeInput>(),
        schema::<SubscribeInputResult>(),
        // Substrate capture path — on-demand PNG readback of the
        // current swapchain, replied-to-sender so an MCP session can
        // see what the engine is rendering.
        schema::<CaptureFrame>(),
        schema::<CaptureFrameResult>(),
        // Read-only snapshot of OS / engine / GPU / monitors / window.
        // Empty request, fat reply — see `PlatformInfoResult`.
        schema::<PlatformInfo>(),
        schema::<PlatformInfoResult>(),
        // Window-mode switch: agents flip between windowed /
        // fullscreen-borderless / fullscreen-exclusive, reply carries
        // the resolved state.
        schema::<SetWindowMode>(),
        schema::<SetWindowModeResult>(),
        // Runtime window-title update. Desktop-only; headless/hub
        // reply with an `unsupported` error.
        schema::<SetWindowTitle>(),
        schema::<SetWindowTitleResult>(),
    ]
}

fn schema<K: Kind + Schema>() -> KindDescriptor {
    KindDescriptor {
        name: K::NAME.to_string(),
        schema: K::SCHEMA.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_hub_protocol::{Primitive, SchemaType};

    #[test]
    fn covers_every_substrate_kind() {
        let descs = all();
        let names: Vec<&str> = descs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&Tick::NAME));
        assert!(names.contains(&Key::NAME));
        assert!(names.contains(&MouseButton::NAME));
        assert!(names.contains(&MouseMove::NAME));
        assert!(names.contains(&DrawTriangle::NAME));
        assert!(names.contains(&Ping::NAME));
        assert!(names.contains(&Pong::NAME));
        assert!(names.contains(&LoadComponent::NAME));
        assert!(names.contains(&ReplaceComponent::NAME));
        assert!(names.contains(&DropComponent::NAME));
        assert!(names.contains(&LoadResult::NAME));
        assert!(names.contains(&DropResult::NAME));
        assert!(names.contains(&ReplaceResult::NAME));
        assert!(names.contains(&SubscribeInput::NAME));
        assert!(names.contains(&UnsubscribeInput::NAME));
        assert!(names.contains(&SubscribeInputResult::NAME));
    }

    #[test]
    fn control_kinds_are_postcard_schemas() {
        // ADR-0019: control-plane kinds ship as `Struct{repr_c:false,..}`
        // (LoadComponent, DropComponent, ReplaceComponent) or `Enum{..}`
        // (the *Result variants). Hub builds them from agent params
        // via the postcard encoder.
        let descs = all();
        for name in [
            LoadComponent::NAME,
            ReplaceComponent::NAME,
            DropComponent::NAME,
        ] {
            let d = descs.iter().find(|d| d.name == name).unwrap();
            let SchemaType::Struct { repr_c, .. } = &d.schema else {
                panic!("{name} should be Struct, got {:?}", d.schema);
            };
            assert!(!*repr_c, "{name} contains String/Vec, must be postcard");
        }
        for name in [LoadResult::NAME, DropResult::NAME, ReplaceResult::NAME] {
            let d = descs.iter().find(|d| d.name == name).unwrap();
            assert!(
                matches!(d.schema, SchemaType::Enum { .. }),
                "{name} should be Enum, got {:?}",
                d.schema
            );
        }
    }

    #[test]
    fn cast_kinds_emit_struct_with_repr_c() {
        let descs = all();
        for name in [
            Key::NAME,
            MouseMove::NAME,
            DrawTriangle::NAME,
            FrameStats::NAME,
            Ping::NAME,
            Pong::NAME,
        ] {
            let d = descs.iter().find(|d| d.name == name).unwrap();
            let SchemaType::Struct { repr_c, .. } = &d.schema else {
                panic!("expected Struct for {name}, got {:?}", d.schema);
            };
            assert!(*repr_c, "{name} should be cast-shaped");
        }
    }

    #[test]
    fn signal_kinds_emit_unit() {
        let descs = all();
        for name in [Tick::NAME, MouseButton::NAME] {
            let d = descs.iter().find(|d| d.name == name).unwrap();
            assert_eq!(d.schema, SchemaType::Unit, "{name} should be Unit");
        }
    }

    #[test]
    fn key_field_layout() {
        let descs = all();
        let key = descs.iter().find(|d| d.name == Key::NAME).unwrap();
        let SchemaType::Struct { fields, .. } = &key.schema else {
            panic!("expected Struct")
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "code");
        assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
    }

    #[test]
    fn mouse_move_field_layout() {
        let descs = all();
        let mm = descs.iter().find(|d| d.name == MouseMove::NAME).unwrap();
        let SchemaType::Struct { fields, .. } = &mm.schema else {
            panic!("expected Struct")
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "x");
        assert_eq!(fields[1].name, "y");
        assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::F32));
        assert_eq!(fields[1].ty, SchemaType::Scalar(Primitive::F32));
    }

    #[test]
    fn draw_triangle_recurses_into_vertex() {
        let descs = all();
        let dt = descs.iter().find(|d| d.name == DrawTriangle::NAME).unwrap();
        let SchemaType::Struct { fields, repr_c } = &dt.schema else {
            panic!("expected Struct")
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
    }

    #[test]
    fn frame_stats_field_layout() {
        let descs = all();
        let fs = descs.iter().find(|d| d.name == FrameStats::NAME).unwrap();
        let SchemaType::Struct { fields, .. } = &fs.schema else {
            panic!("expected Struct")
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "frame");
        assert_eq!(fields[1].name, "triangles");
        assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U64));
    }
}
