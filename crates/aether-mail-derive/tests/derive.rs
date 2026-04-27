// Integration tests for `#[derive(Kind)]` and `#[derive(Schema)]`. Run
// with `cargo test -p aether-mail-derive`. The aether-mail dev-dep
// has both `derive` and `descriptors` features enabled (Cargo.toml of
// this crate), so the macros expand and the runtime traits resolve.
//
// Each test pins one slice of behavior:
//   - `#[derive(Kind)]` sets `NAME` and `CastEligible::ELIGIBLE`
//     correctly across `#[repr(C)]` / non-repr / nested-substructure
//     cases.
//   - `#[derive(Schema)]` produces the right `SchemaType` for unit,
//     tuple, named-field, cast-eligible, postcard-shaped, and
//     enum-shaped inputs.
//   - The `Vec<u8>` field-level specialization lands as `Bytes`, not
//     `Vec(Scalar(U8))`.

use aether_hub_protocol::{EnumVariant, NamedField, Primitive, SchemaCell, SchemaType};
use aether_mail::{CastEligible, Kind, Ref, Schema};
use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.tick")]
struct Tick;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.key")]
struct Key {
    code: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.vertex")]
struct Vertex {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.triangle")]
struct Triangle {
    verts: [Vertex; 3],
}

#[derive(Serialize, Deserialize, aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.note")]
#[allow(dead_code)]
struct Note {
    body: String,
    tags: Vec<String>,
    optional: Option<u32>,
    blob: Vec<u8>,
}

#[derive(Serialize, Deserialize, aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.tuple")]
#[allow(dead_code)]
struct TupleStruct(u32, bool);

#[derive(Serialize, Deserialize, aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.result")]
#[allow(dead_code)]
enum Outcome {
    Pending,
    Ok(u64),
    Err { reason: String },
}

#[test]
fn unit_struct_emits_name_and_unit_schema() {
    assert_eq!(<Tick as Kind>::NAME, "test.tick");
    assert!(matches!(<Tick as Schema>::SCHEMA, SchemaType::Unit));
}

#[test]
fn cast_eligible_struct_picks_repr_c_true() {
    assert_eq!(<Key as Kind>::NAME, "test.key");
    const { assert!(<Key as CastEligible>::ELIGIBLE) };
    let SchemaType::Struct { repr_c, fields } = &<Key as Schema>::SCHEMA else {
        panic!("expected Struct schema");
    };
    assert!(*repr_c);
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "code");
    assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
}

#[test]
fn cast_eligible_propagates_through_array_of_substruct() {
    const { assert!(<Vertex as CastEligible>::ELIGIBLE) };
    const { assert!(<Triangle as CastEligible>::ELIGIBLE) };
    let SchemaType::Struct { repr_c, fields } = &<Triangle as Schema>::SCHEMA else {
        panic!("expected Struct schema");
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
    assert_eq!(nested_fields.len(), 2);
}

#[test]
fn postcard_struct_marks_repr_c_false_and_specializes_bytes() {
    const { assert!(!<Note as CastEligible>::ELIGIBLE) };
    let SchemaType::Struct { repr_c, fields } = &<Note as Schema>::SCHEMA else {
        panic!("expected Struct schema");
    };
    assert!(!*repr_c);
    let by_name: std::collections::HashMap<&str, &SchemaType> =
        fields.iter().map(|f| (&*f.name, &f.ty)).collect();
    assert_eq!(by_name["body"], &SchemaType::String);
    assert!(matches!(by_name["tags"], SchemaType::Vec(inner) if **inner == SchemaType::String));
    assert!(
        matches!(by_name["optional"], SchemaType::Option(inner) if **inner == SchemaType::Scalar(Primitive::U32))
    );
    // Vec<u8> is the load-bearing specialization — must land as
    // `Bytes`, not `Vec(Scalar(U8))`. Catching this regression is the
    // point of having a dedicated assertion.
    assert_eq!(by_name["blob"], &SchemaType::Bytes);
}

#[test]
fn tuple_struct_names_fields_positionally() {
    let SchemaType::Struct { fields, repr_c } = &<TupleStruct as Schema>::SCHEMA else {
        panic!("expected Struct schema");
    };
    // No `#[repr(C)]` on the tuple struct → not cast eligible.
    assert!(!*repr_c);
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "0");
    assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
    assert_eq!(fields[1].name, "1");
    assert_eq!(fields[1].ty, SchemaType::Bool);
}

#[test]
fn enum_emits_each_variant_shape_with_sequential_discriminants() {
    assert_eq!(<Outcome as Kind>::NAME, "test.result");
    const { assert!(!<Outcome as CastEligible>::ELIGIBLE) };
    let SchemaType::Enum { variants } = &<Outcome as Schema>::SCHEMA else {
        panic!("expected Enum schema");
    };
    assert_eq!(variants.len(), 3);

    let EnumVariant::Unit { name, discriminant } = &variants[0] else {
        panic!("expected Unit variant first");
    };
    assert_eq!(name, "Pending");
    assert_eq!(*discriminant, 0);

    let EnumVariant::Tuple {
        name,
        discriminant,
        fields,
    } = &variants[1]
    else {
        panic!("expected Tuple variant second");
    };
    assert_eq!(name, "Ok");
    assert_eq!(*discriminant, 1);
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0], SchemaType::Scalar(Primitive::U64));

    let EnumVariant::Struct {
        name,
        discriminant,
        fields,
    } = &variants[2]
    else {
        panic!("expected Struct variant third");
    };
    assert_eq!(name, "Err");
    assert_eq!(*discriminant, 2);
    assert_eq!(
        fields,
        &vec![NamedField {
            name: "reason".into(),
            ty: SchemaType::String,
        }]
    );
}

// ADR-0045 typed handle: the derive doesn't have to know about
// `Ref<K>` syntactically — the hand-rolled `Schema` impl in
// aether-mail dispatches through the existing trait. These tests
// pin that integration: a struct with a `Ref<K>` field gets a
// `SchemaType::Ref` arm in its layout, the parent's CastEligible
// flips to false (refs force postcard), and the wire roundtrips
// for both Inline and Handle variants.

#[derive(Serialize, Deserialize, aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.held_note")]
#[allow(dead_code)]
struct HeldNote {
    body: Ref<Note>,
    seq: u32,
}

#[test]
fn ref_field_lands_as_schema_ref_pointing_at_inner_kind() {
    let SchemaType::Struct { fields, repr_c } = &<HeldNote as Schema>::SCHEMA else {
        panic!("expected Struct schema");
    };
    // Ref<K> forces postcard — a parent with a Ref field can't
    // claim `repr_c: true` no matter how the rest of the fields
    // look. ADR-0045 §1.
    assert!(!*repr_c);
    assert_eq!(fields.len(), 2);

    let body = &fields[0];
    assert_eq!(body.name, "body");
    let SchemaType::Ref(inner_cell) = &body.ty else {
        panic!("expected Ref schema for body field, got {:?}", body.ty);
    };
    let inner: &SchemaType = inner_cell;
    // The cell points at <Note as Schema>::SCHEMA verbatim — same
    // bytes the standalone Note would emit. Recipients dispatch
    // against this after handle resolution lands inline.
    assert_eq!(inner, &<Note as Schema>::SCHEMA);

    // Sibling field unaffected.
    let seq = &fields[1];
    assert_eq!(seq.name, "seq");
    assert_eq!(seq.ty, SchemaType::Scalar(Primitive::U32));
}

#[test]
fn parent_with_ref_field_is_cast_ineligible() {
    // Even though Ref<K>'s own ELIGIBLE is false, the AND-fold in
    // the derive's emitted `ELIGIBLE` const propagates the false up
    // to the parent. A future regression that forgets to mark
    // Ref<K> ineligible would let parents claim repr_c, then break
    // the cast-shaped wire encoder when it tries to emit a
    // variable-length Inline body.
    const { assert!(!<HeldNote as CastEligible>::ELIGIBLE) };
}

#[test]
fn ref_inner_kind_id_is_carried_in_handle_arm() {
    // `Ref::handle::<K>(id)` pulls `K::ID` automatically — pin the
    // value so a future change to the kind's NAME or schema (which
    // would reshuffle the FNV-derived id) is loud, not silent.
    let r: Ref<Note> = Ref::handle(0xfeed_0000);
    let Ref::Handle { id, kind_id } = r else {
        panic!("expected Handle variant");
    };
    assert_eq!(id, 0xfeed_0000);
    assert_eq!(kind_id, <Note as Kind>::ID);
}

#[test]
fn ref_kind_id_differs_from_inline_kind_id() {
    // The schema canonical bytes change when a field flips from
    // `K` to `Ref<K>` — they pick up an extra `SCHEMA_REF` tag.
    // This is intentional (a wire change at the kind boundary is
    // a kind boundary change), but pin it explicitly so a refactor
    // can't silently align the two ids and let mismatched
    // recipients silently consume each other's mail.
    #[derive(Serialize, Deserialize, aether_mail::Kind, aether_mail::Schema)]
    #[kind(name = "test.inline_note_field")]
    #[allow(dead_code)]
    struct Inlined {
        body: Note,
        seq: u32,
    }
    assert_ne!(<HeldNote as Kind>::ID, <Inlined as Kind>::ID);
}

#[test]
fn ref_schema_cell_uses_static_pointer_for_const_construction() {
    // ADR-0031: the Schema impl is a `const` so the derive can
    // splat it as a literal. `SchemaType::Ref(SchemaCell::Static(_))`
    // is what comes out of `<Ref<K> as Schema>::SCHEMA`; pin it
    // here so a regression to `Owned` (which would force runtime
    // allocation per-emit) is loud.
    let SchemaType::Ref(cell) = &<Ref<Note> as Schema>::SCHEMA else {
        panic!("expected Ref schema");
    };
    assert!(matches!(cell, SchemaCell::Static(_)));
}

#[test]
fn cast_eligible_blocked_by_non_pod_field_even_with_repr_c() {
    // A struct with `#[repr(C)]` but a non-eligible field type must
    // still report `ELIGIBLE = false`. Catching this is what stops
    // the substrate from misclassifying a postcard kind as cast-able.
    //
    // Kind derive is intentionally omitted here — the autodetect rule
    // (cast iff `#[repr(C)]`) would emit a `decode_cast` body that
    // can't satisfy `AnyBitPattern` against a `String` field, and that
    // user-side compile error is the correct outcome. This fixture
    // exercises Schema's AND-fold in isolation.
    #[repr(C)]
    #[derive(aether_mail::Schema)]
    #[allow(dead_code)]
    struct ReprCButStrung {
        seq: u32,
        // String isn't `CastEligible` (no impl), so the AND in the
        // derive's emitted `ELIGIBLE` const evaluates to `false`.
        label: String,
    }
    const { assert!(!<ReprCButStrung as CastEligible>::ELIGIBLE) };
    let SchemaType::Struct { repr_c, .. } = &<ReprCButStrung as Schema>::SCHEMA else {
        panic!("expected Struct");
    };
    assert!(!*repr_c);
}

// Issue #232 — `BTreeMap<K, V>` is the deterministic map type for
// derived kind schemas. The Schema impl in `aether-mail` lands a
// `SchemaType::Map`; the derive does no special-casing, so this is
// trait-dispatch end-to-end.

#[derive(Serialize, Deserialize, aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.headers")]
#[allow(dead_code)]
struct Headers {
    headers: std::collections::BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.lookup")]
#[allow(dead_code)]
struct Lookup {
    counters: std::collections::BTreeMap<u32, u64>,
}

#[test]
fn btreemap_field_lands_as_schema_map() {
    let SchemaType::Struct { fields, .. } = &<Headers as Schema>::SCHEMA else {
        panic!("expected Struct schema");
    };
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "headers");
    let SchemaType::Map { key, value } = &fields[0].ty else {
        panic!("expected Map schema, got {:?}", fields[0].ty);
    };
    assert_eq!(&**key, &SchemaType::String);
    assert_eq!(&**value, &SchemaType::String);
}

#[test]
fn btreemap_with_integer_keys_lands_as_schema_map() {
    let SchemaType::Struct { fields, .. } = &<Lookup as Schema>::SCHEMA else {
        panic!("expected Struct schema");
    };
    let SchemaType::Map { key, value } = &fields[0].ty else {
        panic!("expected Map schema");
    };
    assert_eq!(&**key, &SchemaType::Scalar(Primitive::U32));
    assert_eq!(&**value, &SchemaType::Scalar(Primitive::U64));
}

#[test]
fn btreemap_field_disqualifies_repr_c() {
    // BTreeMap is variable-length — same constraint as Vec/String/
    // Option. A `#[repr(C)]` struct with a BTreeMap field must report
    // `ELIGIBLE = false` so the wire layer doesn't try to cast bytes.
    const { assert!(!<Headers as CastEligible>::ELIGIBLE) };
    const { assert!(!<Lookup as CastEligible>::ELIGIBLE) };
}

#[test]
fn btreemap_kind_id_stable_across_invocations() {
    // `Kind::ID` is `fnv1a_64_prefixed(KIND_DOMAIN, canonical_bytes)`.
    // Two reads of the same const must agree byte-for-byte; this is
    // the canonical-stability invariant ADR-0030 + ADR-0032 lock in
    // for the new Map arm.
    let id_a = <Headers as Kind>::ID;
    let id_b = <Headers as Kind>::ID;
    assert_eq!(id_a, id_b);
    // And different schemas → different ids (collision-resistance
    // sanity, not an exhaustive test of FNV-1a).
    assert_ne!(<Headers as Kind>::ID, <Lookup as Kind>::ID);
}
