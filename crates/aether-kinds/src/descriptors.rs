// Wire descriptors for the substrate's kinds. Consumed by the native
// substrate binary and shipped to the hub at `Hello` per ADR-0007 so
// the hub can encode agent-supplied params for each kind.
//
// ADR-0019 PR 5: every substrate kind, including the control-plane
// vocabulary, ships as `KindEncoding::Schema(T::schema())`. There are
// no `Opaque` kinds left in the substrate's descriptor list — every
// kind is hub-encodable from agent params, and the `payload_bytes`
// escape hatch has been removed from the MCP `send_mail` tool.
//
// Issue #243: the descriptor list used to live as a manual
// `vec![schema::<Tick>(), schema::<Key>(), ...]` here. Adding a
// kind required a second touch to update this list, easy to forget
// — the safety net was a runtime "unknown kind" error at first send.
// Now the `Kind` derive macro emits a `cfg(not(target_arch = "wasm32"))`
// -gated `inventory::submit!` per type (paired with the existing wasm
// `aether.kinds` custom-section path); `all()` materializes the Hub-
// shipped `KindDescriptor` list by iterating the inventory slot.
// Adding a kind is one place — the struct definition with its derives.

// `all()` and its tests are native-only — the function materializes a
// Hub-shipped descriptor list from the inventory slot the Kind derive
// populates on non-wasm builds. wasm guests don't call it (their kind
// discovery rides the `aether.kinds` custom section, ADR-0032), and
// the inventory crate doesn't link on wasm32-unknown-unknown anyway.
#![cfg(not(target_arch = "wasm32"))]

use alloc::string::ToString;
use alloc::vec::Vec;

use aether_hub_protocol::KindDescriptor;

/// Every kind the substrate exposes. Order is unspecified — names are
/// the contract; downstream callers (`Registry::register_kind_with_descriptor`,
/// hub `Hello` handshake) are order-independent.
pub fn all() -> Vec<KindDescriptor> {
    inventory::iter::<aether_mail::__inventory::DescriptorEntry>()
        .map(|e| KindDescriptor {
            name: e.name.to_string(),
            schema: e.schema.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_hub_protocol::{Primitive, SchemaType};
    use aether_mail::Kind;

    use crate::{
        Delete, DeleteResult, DrawTriangle, DropComponent, DropResult, Fetch, FetchResult,
        FrameStats, Key, List, ListResult, LoadComponent, LoadResult, MouseButton, MouseMove,
        NoteOff, NoteOn, Ping, Pong, Read, ReadResult, ReplaceComponent, ReplaceResult,
        SetMasterGain, SubscribeInput, SubscribeInputResult, Tick, UnsubscribeInput, Write,
        WriteResult,
    };

    #[test]
    fn descriptor_list_is_non_empty() {
        // Issue #243 regression guard: the inventory-driven `all()`
        // depends on the linker not stripping the per-kind submission
        // statics. If `--gc-sections` ever decides those are
        // dead, the substrate boots with an empty kind vocabulary
        // and silently wedges. Catch it here instead of in MCP.
        assert!(
            !all().is_empty(),
            "descriptors::all() returned no kinds — inventory entries stripped at link?",
        );
    }

    #[test]
    fn descriptor_list_is_unique() {
        // Inventory submission has no built-in dedup. Two declarations
        // of the same kind name would land here as duplicate entries —
        // probably a bug somewhere upstream of the registry.
        let descs = all();
        let names: Vec<&str> = descs.iter().map(|d| d.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            names.len(),
            sorted.len(),
            "duplicate kind names in descriptors::all(): {names:?}",
        );
    }

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
        assert!(names.contains(&NoteOn::NAME));
        assert!(names.contains(&NoteOff::NAME));
        assert!(names.contains(&SetMasterGain::NAME));
        assert!(names.contains(&Read::NAME));
        assert!(names.contains(&ReadResult::NAME));
        assert!(names.contains(&Write::NAME));
        assert!(names.contains(&WriteResult::NAME));
        assert!(names.contains(&Delete::NAME));
        assert!(names.contains(&DeleteResult::NAME));
        assert!(names.contains(&List::NAME));
        assert!(names.contains(&ListResult::NAME));
        assert!(names.contains(&Fetch::NAME));
        assert!(names.contains(&FetchResult::NAME));
    }

    #[test]
    fn io_requests_are_postcard_schemas() {
        // ADR-0041 §1: request kinds carry `String` namespace + path
        // (and `Vec<u8>` bytes on `Write`), so they must serialize as
        // non-cast structs. Catches an accidental `#[repr(C)]` +
        // `Pod` derive that would silently flip the wire format.
        let descs = all();
        for name in [Read::NAME, Write::NAME, Delete::NAME, List::NAME] {
            let d = descs.iter().find(|d| d.name == name).unwrap();
            let SchemaType::Struct { repr_c, .. } = &d.schema else {
                panic!("{name} should be Struct, got {:?}", d.schema);
            };
            assert!(!*repr_c, "{name} contains String/Vec, must be postcard");
        }
    }

    #[test]
    fn io_results_are_enum_schemas() {
        // Each reply kind is an Ok/Err enum; `Err` wraps `IoError`,
        // `Ok` shape varies per operation.
        let descs = all();
        for name in [
            ReadResult::NAME,
            WriteResult::NAME,
            DeleteResult::NAME,
            ListResult::NAME,
        ] {
            let d = descs.iter().find(|d| d.name == name).unwrap();
            assert!(
                matches!(d.schema, SchemaType::Enum { .. }),
                "{name} should be Enum, got {:?}",
                d.schema
            );
        }
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
            NoteOn::NAME,
            NoteOff::NAME,
            SetMasterGain::NAME,
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
        assert_eq!(nested_fields.len(), 6);
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
