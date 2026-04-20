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

use aether_hub_protocol::{EnumVariant, NamedField, Primitive, SchemaType};
use aether_mail::{CastEligible, Kind, Schema};

#[derive(aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.tick")]
struct Tick;

#[repr(C)]
#[derive(aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.key")]
struct Key {
    code: u32,
}

#[repr(C)]
#[derive(aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.vertex")]
struct Vertex {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.triangle")]
struct Triangle {
    verts: [Vertex; 3],
}

#[derive(aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.note")]
#[allow(dead_code)]
struct Note {
    body: String,
    tags: Vec<String>,
    optional: Option<u32>,
    blob: Vec<u8>,
}

#[derive(aether_mail::Kind, aether_mail::Schema)]
#[kind(name = "test.tuple")]
#[allow(dead_code)]
struct TupleStruct(u32, bool);

#[derive(aether_mail::Kind, aether_mail::Schema)]
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
    assert!(matches!(<Tick as Schema>::SCHEMA.clone(), SchemaType::Unit));
}

#[test]
fn cast_eligible_struct_picks_repr_c_true() {
    assert_eq!(<Key as Kind>::NAME, "test.key");
    const { assert!(<Key as CastEligible>::ELIGIBLE) };
    let SchemaType::Struct { repr_c, fields } = <Key as Schema>::SCHEMA.clone() else {
        panic!("expected Struct schema");
    };
    assert!(repr_c);
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "code");
    assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
}

#[test]
fn cast_eligible_propagates_through_array_of_substruct() {
    const { assert!(<Vertex as CastEligible>::ELIGIBLE) };
    const { assert!(<Triangle as CastEligible>::ELIGIBLE) };
    let SchemaType::Struct { repr_c, fields } = <Triangle as Schema>::SCHEMA.clone() else {
        panic!("expected Struct schema");
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
    assert_eq!(nested_fields.len(), 2);
}

#[test]
fn postcard_struct_marks_repr_c_false_and_specializes_bytes() {
    const { assert!(!<Note as CastEligible>::ELIGIBLE) };
    let SchemaType::Struct { repr_c, fields } = <Note as Schema>::SCHEMA.clone() else {
        panic!("expected Struct schema");
    };
    assert!(!repr_c);
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
    let SchemaType::Struct { fields, repr_c } = <TupleStruct as Schema>::SCHEMA.clone() else {
        panic!("expected Struct schema");
    };
    // No `#[repr(C)]` on the tuple struct → not cast eligible.
    assert!(!repr_c);
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
    let SchemaType::Enum { variants } = <Outcome as Schema>::SCHEMA.clone() else {
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

#[test]
fn cast_eligible_blocked_by_non_pod_field_even_with_repr_c() {
    // A struct with `#[repr(C)]` but a non-eligible field type must
    // still report `ELIGIBLE = false`. Catching this is what stops
    // the substrate from misclassifying a postcard kind as cast-able.
    #[repr(C)]
    #[derive(aether_mail::Kind, aether_mail::Schema)]
    #[kind(name = "test.repr_c_with_string")]
    #[allow(dead_code)]
    struct ReprCButStrung {
        seq: u32,
        // String isn't `CastEligible` (no impl), so the AND in the
        // derive's emitted `ELIGIBLE` const evaluates to `false`.
        label: String,
    }
    const { assert!(!<ReprCButStrung as CastEligible>::ELIGIBLE) };
    let SchemaType::Struct { repr_c, .. } = <ReprCButStrung as Schema>::SCHEMA.clone() else {
        panic!("expected Struct");
    };
    assert!(!repr_c);
}
