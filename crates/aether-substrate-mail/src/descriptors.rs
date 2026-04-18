// Wire descriptors for the substrate's kinds. Consumed by the native
// substrate binary and shipped to the hub at `Hello` per ADR-0007 so
// the hub can encode agent-supplied params for each kind.
//
// ADR-0019 PR 4: cast-shaped kinds are now emitted as
// `KindEncoding::Schema(T::schema())` — the bytes the hub produces are
// identical to what the legacy `Pod`/`Signal` arms produced, but the
// descriptor walks the type definition instead of restating its layout
// here. Adding or renaming a field on a kind is a one-place change
// (the struct itself); the schema is whatever the derive emits.
//
// Control-plane kinds (LoadComponent, ReplaceComponent, DropComponent,
// LoadResult, DropResult, ReplaceResult) stay `Opaque` here. They
// migrate in PR 5 along with the postcard encoder + `payload_bytes`
// removal.

use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use aether_hub_protocol::{KindDescriptor, KindEncoding};
use aether_mail::{Kind, Schema};

use crate::{
    DrawTriangle, DropComponent, DropResult, FrameStats, Key, LoadComponent, LoadResult,
    MouseButton, MouseMove, Ping, Pong, ReplaceComponent, ReplaceResult, Tick,
};

/// Every kind the substrate exposes, in the order the `Registry` will
/// register them. Caller ignores the order — names are the contract.
pub fn all() -> Vec<KindDescriptor> {
    vec![
        schema::<Tick>(),
        schema::<Key>(),
        schema::<MouseButton>(),
        schema::<MouseMove>(),
        // DrawTriangle's schema recurses into Vertex; the cast wire
        // format keeps today's bytes (the hub encoder treats the
        // nested `Struct { repr_c: true }` exactly like a flat Pod).
        schema::<DrawTriangle>(),
        schema::<FrameStats>(),
        // ADR-0013 smoke-test vocabulary.
        schema::<Ping>(),
        schema::<Pong>(),
        // ADR-0010 control-plane kinds — still Opaque; PR 5 turns
        // them into real schemas alongside dropping `payload_bytes`.
        opaque(LoadComponent::NAME),
        opaque(ReplaceComponent::NAME),
        opaque(DropComponent::NAME),
        opaque(LoadResult::NAME),
        opaque(DropResult::NAME),
        opaque(ReplaceResult::NAME),
    ]
}

fn schema<K: Kind + Schema>() -> KindDescriptor {
    KindDescriptor {
        name: K::NAME.to_string(),
        encoding: KindEncoding::Schema(K::schema()),
    }
}

fn opaque(name: &str) -> KindDescriptor {
    KindDescriptor {
        name: name.to_string(),
        encoding: KindEncoding::Opaque,
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
    }

    #[test]
    fn control_kinds_are_opaque() {
        // PR 5 turns these into Schema kinds. Until then, agents must
        // supply `payload_bytes` for the control plane.
        let descs = all();
        for name in [
            LoadComponent::NAME,
            ReplaceComponent::NAME,
            DropComponent::NAME,
            LoadResult::NAME,
            DropResult::NAME,
            ReplaceResult::NAME,
        ] {
            let d = descs.iter().find(|d| d.name == name).unwrap();
            assert_eq!(d.encoding, KindEncoding::Opaque, "{name}");
        }
    }

    #[test]
    fn cast_kinds_emit_schema_struct_with_repr_c() {
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
            let KindEncoding::Schema(SchemaType::Struct { repr_c, .. }) = &d.encoding else {
                panic!("expected Schema(Struct) for {name}, got {:?}", d.encoding);
            };
            assert!(*repr_c, "{name} should be cast-shaped");
        }
    }

    #[test]
    fn signal_kinds_emit_schema_unit() {
        let descs = all();
        for name in [Tick::NAME, MouseButton::NAME] {
            let d = descs.iter().find(|d| d.name == name).unwrap();
            assert_eq!(
                d.encoding,
                KindEncoding::Schema(SchemaType::Unit),
                "{name} should be Schema(Unit)"
            );
        }
    }

    #[test]
    fn key_field_layout() {
        let descs = all();
        let key = descs.iter().find(|d| d.name == Key::NAME).unwrap();
        let KindEncoding::Schema(SchemaType::Struct { fields, .. }) = &key.encoding else {
            panic!("expected Schema(Struct)")
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "code");
        assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
    }

    #[test]
    fn mouse_move_field_layout() {
        let descs = all();
        let mm = descs.iter().find(|d| d.name == MouseMove::NAME).unwrap();
        let KindEncoding::Schema(SchemaType::Struct { fields, .. }) = &mm.encoding else {
            panic!("expected Schema(Struct)")
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "x");
        assert_eq!(fields[1].name, "y");
        assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::F32));
        assert_eq!(fields[1].ty, SchemaType::Scalar(Primitive::F32));
    }

    #[test]
    fn draw_triangle_recurses_into_vertex() {
        // The cast wire format previously couldn't describe nested
        // structs (DrawTriangle was Opaque). The Schema arm fixes that.
        let descs = all();
        let dt = descs.iter().find(|d| d.name == DrawTriangle::NAME).unwrap();
        let KindEncoding::Schema(SchemaType::Struct { fields, repr_c }) = &dt.encoding else {
            panic!("expected Schema(Struct)")
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
        } = element.as_ref()
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
        let KindEncoding::Schema(SchemaType::Struct { fields, .. }) = &fs.encoding else {
            panic!("expected Schema(Struct)")
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "frame");
        assert_eq!(fields[1].name, "triangles");
        assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U64));
    }
}
