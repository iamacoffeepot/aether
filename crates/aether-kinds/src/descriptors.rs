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

use aether_data::__inventory::DescriptorEntry;
use aether_data::KindDescriptor;

/// Every kind the substrate exposes. Order is unspecified — names are
/// the contract; downstream callers (`Registry::register_kind_with_descriptor`,
/// hub `Hello` handshake) are order-independent.
#[must_use]
pub fn all() -> Vec<KindDescriptor> {
    inventory::iter::<DescriptorEntry>()
        .map(|e| KindDescriptor {
            name: e.name.to_string(),
            schema: e.schema.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;
    use aether_data::{Primitive, SchemaType};

    use crate::{
        Delete, DeleteResult, DrawTriangle, DropComponent, DropResult, FsFetch, FsFetchResult, Key,
        LifecycleUnsubscribeAll, List, ListResult, LoadComponent, LoadResult, Mat4Apply,
        MouseButton, MouseMove, Ping, Pong, ProcessExited, Read, ReadResult, RecordResult,
        ReplaceComponent, ReplaceResult, Spawn, SpawnResult, Terminate, TerminateResult, Tick,
        TrajectoryEnd, TrajectoryLog, TrajectorySample, Write, WriteResult,
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
        assert!(names.contains(&LifecycleUnsubscribeAll::NAME));
        assert!(names.contains(&Read::NAME));
        assert!(names.contains(&ReadResult::NAME));
        assert!(names.contains(&Write::NAME));
        assert!(names.contains(&WriteResult::NAME));
        assert!(names.contains(&Delete::NAME));
        assert!(names.contains(&DeleteResult::NAME));
        assert!(names.contains(&List::NAME));
        assert!(names.contains(&ListResult::NAME));
        assert!(names.contains(&FsFetch::NAME));
        assert!(names.contains(&FsFetchResult::NAME));
        assert!(names.contains(&Spawn::NAME));
        assert!(names.contains(&SpawnResult::NAME));
        assert!(names.contains(&Terminate::NAME));
        assert!(names.contains(&TerminateResult::NAME));
        assert!(names.contains(&ProcessExited::NAME));
    }

    #[test]
    fn io_requests_are_structured_schemas() {
        // ADR-0041 §1: request kinds carry `String` namespace + path
        // (and `Vec<u8>` bytes on `Write`), so they must serialize as
        // non-cast structs. Catches an accidental `#[repr(C)]` +
        // `Pod` derive that would silently flip the wire format.
        let descs = all();
        for name in [Read::NAME, Write::NAME, Delete::NAME, List::NAME] {
            let d = descs
                .iter()
                .find(|d| d.name == name)
                .expect("test setup: io request kind is registered in descriptor inventory");
            let SchemaType::Struct { repr_c, .. } = &d.schema else {
                panic!("{name} should be Struct, got {:?}", d.schema);
            };
            assert!(!*repr_c, "{name} contains String/Vec, must be structured");
        }
    }

    #[test]
    fn io_results_are_enum_schemas() {
        // Each reply kind is an Ok/Err enum; `Err` wraps `FsError`,
        // `Ok` shape varies per operation.
        let descs = all();
        for name in [
            ReadResult::NAME,
            WriteResult::NAME,
            DeleteResult::NAME,
            ListResult::NAME,
        ] {
            let d = descs
                .iter()
                .find(|d| d.name == name)
                .expect("test setup: io result kind is registered in descriptor inventory");
            assert!(
                matches!(d.schema, SchemaType::Enum { .. }),
                "{name} should be Enum, got {:?}",
                d.schema
            );
        }
    }

    #[test]
    fn control_kinds_are_structured_schemas() {
        // ADR-0019: control-plane kinds ship as `Struct{repr_c:false,..}`
        // (LoadComponent, DropComponent, ReplaceComponent) or `Enum{..}`
        // (the *Result variants). Hub builds them from agent params
        // via the wire encoder.
        let descs = all();
        for name in [
            LoadComponent::NAME,
            ReplaceComponent::NAME,
            DropComponent::NAME,
        ] {
            let d = descs
                .iter()
                .find(|d| d.name == name)
                .expect("test setup: control request kind is registered in descriptor inventory");
            let SchemaType::Struct { repr_c, .. } = &d.schema else {
                panic!("{name} should be Struct, got {:?}", d.schema);
            };
            assert!(!*repr_c, "{name} contains String/Vec, must be structured");
        }
        for name in [LoadResult::NAME, DropResult::NAME, ReplaceResult::NAME] {
            let d = descs
                .iter()
                .find(|d| d.name == name)
                .expect("test setup: control result kind is registered in descriptor inventory");
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
            Ping::NAME,
            Pong::NAME,
        ] {
            let d = descs
                .iter()
                .find(|d| d.name == name)
                .expect("test setup: cast kind is registered in descriptor inventory");
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
            let d = descs
                .iter()
                .find(|d| d.name == name)
                .expect("test setup: signal kind is registered in descriptor inventory");
            assert_eq!(d.schema, SchemaType::Unit, "{name} should be Unit");
        }
    }

    #[test]
    fn key_field_layout() {
        let descs = all();
        let key = descs
            .iter()
            .find(|d| d.name == Key::NAME)
            .expect("test setup: Key kind is registered in descriptor inventory");
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
        let mm = descs
            .iter()
            .find(|d| d.name == MouseMove::NAME)
            .expect("test setup: MouseMove kind is registered in descriptor inventory");
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
        let dt = descs
            .iter()
            .find(|d| d.name == DrawTriangle::NAME)
            .expect("test setup: DrawTriangle kind is registered in descriptor inventory");
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
    fn mat4_apply_is_registered_cast_schema() {
        // Issue 1464: the `mat4_apply` transform's input kind. It must
        // register in the inventory (so the DAG validator and the hub
        // can encode it) and ride the cast wire path — it composes the
        // `aether_math` primitives directly (`#[repr(C)]` + `Pod`), so
        // its schema is a cast struct whose fields are the nested `Mat4`
        // and `Vec4` cast structs, not flattened `[f32; N]` arrays. A
        // stray loss of `#[repr(C)]` (flipping it to structured) is a
        // wire-format mismatch this catches.
        let descs = all();
        let d = descs
            .iter()
            .find(|d| d.name == Mat4Apply::NAME)
            .expect("test setup: Mat4Apply kind is registered in descriptor inventory");
        let SchemaType::Struct { fields, repr_c } = &d.schema else {
            panic!("{} should be Struct, got {:?}", Mat4Apply::NAME, d.schema);
        };
        assert!(*repr_c, "Mat4Apply must be cast, not structured");
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "matrix");
        assert_eq!(fields[1].name, "vector");

        // `matrix` is the nested `Mat4` cast struct: one `cols` field
        // that is a four-element array of the `Vec4` cast struct.
        let SchemaType::Struct {
            fields: mat_fields,
            repr_c: mat_repr_c,
        } = &fields[0].ty
        else {
            panic!(
                "matrix should be the nested Mat4 Struct, got {:?}",
                fields[0].ty
            );
        };
        assert!(*mat_repr_c, "Mat4 must be cast");
        assert_eq!(mat_fields.len(), 1);
        assert_eq!(mat_fields[0].name, "cols");
        let SchemaType::Array { element, len } = &mat_fields[0].ty else {
            panic!("Mat4::cols should be Array");
        };
        assert_eq!(*len, 4);
        assert!(
            matches!(**element, SchemaType::Struct { repr_c: true, .. }),
            "Mat4::cols element should be the Vec4 cast Struct, got {element:?}"
        );

        // `vector` is the nested `Vec4` cast struct: four `f32` scalars.
        let SchemaType::Struct {
            fields: vec_fields,
            repr_c: vec_repr_c,
        } = &fields[1].ty
        else {
            panic!(
                "vector should be the nested Vec4 Struct, got {:?}",
                fields[1].ty
            );
        };
        assert!(*vec_repr_c, "Vec4 must be cast");
        assert_eq!(vec_fields.len(), 4);
        for f in vec_fields.iter() {
            assert_eq!(f.ty, SchemaType::Scalar(Primitive::F32));
        }
    }

    /// Regression guard: all four trajectory kinds register in the
    /// inventory-driven descriptor list so they appear in `describe_kinds`
    /// without a manual second touch (ADR-0028, issue #243).
    #[test]
    fn trajectory_kinds_are_in_descriptor_list() {
        let descs = all();
        let names: Vec<&str> = descs.iter().map(|d| d.name.as_str()).collect();
        assert!(
            names.contains(&TrajectorySample::NAME),
            "TrajectorySample missing from descriptor list"
        );
        assert!(
            names.contains(&TrajectoryEnd::NAME),
            "TrajectoryEnd missing from descriptor list"
        );
        assert!(
            names.contains(&TrajectoryLog::NAME),
            "TrajectoryLog missing from descriptor list"
        );
        assert!(
            names.contains(&RecordResult::NAME),
            "RecordResult missing from descriptor list"
        );
    }

    /// Trajectory kinds resolve to distinct ids: no two share the same
    /// `Kind::ID`. A collision would mean two kinds dispatched to the
    /// same handler, silently losing one.
    #[test]
    fn trajectory_kind_ids_are_distinct() {
        let ids = [
            TrajectorySample::ID,
            TrajectoryEnd::ID,
            TrajectoryLog::ID,
            RecordResult::ID,
        ];
        // O(n²) pairwise check — n = 4, tolerable.
        for (i, a) in ids.iter().enumerate() {
            for b in ids.iter().skip(i + 1) {
                assert_ne!(a.0, b.0, "duplicate trajectory kind ids: {a:?} == {b:?}");
            }
        }
    }

    /// `TrajectoryLog` is a structured `Struct` (it carries a
    /// `Vec<TrajectorySampleEntry>` field, so `repr_c` must be false).
    #[test]
    fn trajectory_log_is_structured_struct() {
        let descs = all();
        let d = descs
            .iter()
            .find(|d| d.name == TrajectoryLog::NAME)
            .expect("TrajectoryLog in descriptor list");
        let SchemaType::Struct { repr_c, .. } = &d.schema else {
            panic!("expected Struct for TrajectoryLog, got {:?}", d.schema);
        };
        assert!(
            !*repr_c,
            "TrajectoryLog contains Vec — must be structured, not cast"
        );
    }

    /// `RecordResult` is an `Enum` schema (two variants: `Ok` and `Err`).
    #[test]
    fn record_result_is_enum_schema() {
        let descs = all();
        let d = descs
            .iter()
            .find(|d| d.name == RecordResult::NAME)
            .expect("RecordResult in descriptor list");
        assert!(
            matches!(d.schema, SchemaType::Enum { .. }),
            "RecordResult should be Enum, got {:?}",
            d.schema
        );
    }

    /// Wire round-trip for `TrajectoryLog`: encode and decode through the
    /// `Kind` codec (ADR-0118 `aether_data::wire`), asserting equality of
    /// all fields and samples in order.
    #[test]
    fn trajectory_log_wire_roundtrip() {
        use crate::{TrajectoryEndReason, TrajectorySampleEntry};

        let original = TrajectoryLog {
            seed: 0xDEAD_BEEF_u64,
            samples: alloc::vec![
                TrajectorySampleEntry {
                    tick: 1,
                    x: 10,
                    y: 20,
                    value: 100,
                },
                TrajectorySampleEntry {
                    tick: 2,
                    x: 11,
                    y: 21,
                    value: 200,
                },
            ],
            end_reason: TrajectoryEndReason::Completed,
        };

        let bytes = original.encode_into_bytes();
        let decoded = TrajectoryLog::decode_from_bytes(&bytes)
            .expect("TrajectoryLog round-trips through the wire codec");

        assert_eq!(decoded.seed, original.seed);
        assert_eq!(decoded.samples.len(), 2);
        assert_eq!(decoded.samples[0].tick, 1);
        assert_eq!(decoded.samples[0].x, 10);
        assert_eq!(decoded.samples[0].y, 20);
        assert_eq!(decoded.samples[0].value, 100);
        assert_eq!(decoded.samples[1].tick, 2);
        assert_eq!(decoded.samples[1].value, 200);
        assert_eq!(decoded.end_reason, TrajectoryEndReason::Completed);
    }
}
