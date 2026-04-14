// Wire descriptors for the substrate's kinds. Consumed by the native
// substrate binary and shipped to the hub at `Hello` per ADR-0007 so
// the hub can encode agent-supplied params for each kind.
//
// Adding a kind to `lib.rs` means adding a matching entry here; the
// coupling is deliberate — each descriptor re-states the `#[repr(C)]`
// field order the encoder will walk on the hub side, so drift between
// the struct layout and the descriptor is caught when we next look at
// this file.

use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use aether_hub_protocol::{KindDescriptor, KindEncoding, PodField, PodFieldType, PodPrimitive};
use aether_mail::Kind;

use crate::{
    DrawTriangle, DropComponent, FrameStats, Key, LoadComponent, LoadResult, MouseButton,
    MouseMove, ReplaceComponent, Tick,
};

/// Every kind the substrate exposes, in the order the `Registry` will
/// register them. Caller ignores the order — names are the contract.
pub fn all() -> Vec<KindDescriptor> {
    vec![
        signal(Tick::NAME),
        pod(Key::NAME, vec![scalar("code", PodPrimitive::U32)]),
        signal(MouseButton::NAME),
        pod(
            MouseMove::NAME,
            vec![
                scalar("x", PodPrimitive::F32),
                scalar("y", PodPrimitive::F32),
            ],
        ),
        // DrawTriangle nests Vertex; V0 descriptors don't model nested
        // structs, so this kind stays opaque and clients use the raw
        // payload_bytes path.
        opaque(DrawTriangle::NAME),
        pod(
            FrameStats::NAME,
            vec![
                scalar("frame", PodPrimitive::U64),
                scalar("triangles", PodPrimitive::U64),
            ],
        ),
        // ADR-0010 control-plane kinds. Variable-length payloads that
        // don't fit the Pod model — the substrate handler decodes via
        // postcard against its own wire types. Agents that use the MCP
        // `send_mail` tool supply these as raw `payload_bytes`.
        opaque(LoadComponent::NAME),
        opaque(ReplaceComponent::NAME),
        opaque(DropComponent::NAME),
        opaque(LoadResult::NAME),
    ]
}

fn signal(name: &str) -> KindDescriptor {
    KindDescriptor {
        name: name.to_string(),
        encoding: KindEncoding::Signal,
    }
}

fn pod(name: &str, fields: Vec<PodField>) -> KindDescriptor {
    KindDescriptor {
        name: name.to_string(),
        encoding: KindEncoding::Pod { fields },
    }
}

fn opaque(name: &str) -> KindDescriptor {
    KindDescriptor {
        name: name.to_string(),
        encoding: KindEncoding::Opaque,
    }
}

fn scalar(name: &str, ty: PodPrimitive) -> PodField {
    PodField {
        name: name.to_string(),
        ty: PodFieldType::Scalar(ty),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_every_substrate_kind() {
        let descs = all();
        let names: Vec<&str> = descs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&Tick::NAME));
        assert!(names.contains(&Key::NAME));
        assert!(names.contains(&MouseButton::NAME));
        assert!(names.contains(&MouseMove::NAME));
        assert!(names.contains(&DrawTriangle::NAME));
        assert!(names.contains(&LoadComponent::NAME));
        assert!(names.contains(&ReplaceComponent::NAME));
        assert!(names.contains(&DropComponent::NAME));
        assert!(names.contains(&LoadResult::NAME));
    }

    #[test]
    fn control_kinds_are_opaque() {
        let descs = all();
        for name in [
            LoadComponent::NAME,
            ReplaceComponent::NAME,
            DropComponent::NAME,
            LoadResult::NAME,
        ] {
            let d = descs.iter().find(|d| d.name == name).unwrap();
            assert_eq!(d.encoding, KindEncoding::Opaque, "{name}");
        }
    }

    #[test]
    fn key_fields_match_struct_layout() {
        let descs = all();
        let key = descs.iter().find(|d| d.name == Key::NAME).unwrap();
        let KindEncoding::Pod { fields } = &key.encoding else {
            panic!("expected Pod")
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "code");
        assert_eq!(fields[0].ty, PodFieldType::Scalar(PodPrimitive::U32));
    }

    #[test]
    fn mouse_move_fields_match_struct_layout() {
        let descs = all();
        let mm = descs.iter().find(|d| d.name == MouseMove::NAME).unwrap();
        let KindEncoding::Pod { fields } = &mm.encoding else {
            panic!("expected Pod")
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "x");
        assert_eq!(fields[1].name, "y");
        assert!(matches!(
            fields[0].ty,
            PodFieldType::Scalar(PodPrimitive::F32)
        ));
    }

    #[test]
    fn draw_triangle_is_opaque() {
        let descs = all();
        let dt = descs.iter().find(|d| d.name == DrawTriangle::NAME).unwrap();
        assert_eq!(dt.encoding, KindEncoding::Opaque);
    }

    #[test]
    fn frame_stats_fields_match_struct_layout() {
        let descs = all();
        let fs = descs.iter().find(|d| d.name == FrameStats::NAME).unwrap();
        let KindEncoding::Pod { fields } = &fs.encoding else {
            panic!("expected Pod")
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "frame");
        assert_eq!(fields[1].name, "triangles");
        assert!(matches!(
            fields[0].ty,
            PodFieldType::Scalar(PodPrimitive::U64)
        ));
    }
}
