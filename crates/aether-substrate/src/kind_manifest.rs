// ADR-0028 / ADR-0032: read a component's embedded kind manifest
// from two wasm custom sections — `aether.kinds` (positional
// canonical bytes, the `Kind::ID` hash input) and
// `aether.kinds.labels` (Rust-nominal sidecar: type paths, field
// names, variant names). Records in the two sections pair by
// declaration order.
//
// Record format (v2):
//   [0x02] [postcard(KindShape)] — in `aether.kinds`
//   [0x02] [postcard(KindLabels)] — in `aether.kinds.labels`
//
// The parser walks each section sequentially; postcard stops decoding
// exactly at the record's end, so the next byte is the next record's
// version tag. Unknown version bytes abort the parse rather than
// silently skip — a kind missing from the caller's build would
// otherwise surface much later as an unrecognized-ID routing failure.
//
// Wasmtime 30 doesn't expose custom sections on `Module`, so we walk
// the raw bytes via `wasmparser` before compilation. The section data
// lives in the binary's original bytes anyway — compilation isn't a
// prerequisite for reading it, and parsing the raw bytes lets us
// fail on an unknown manifest version before spending cycles on
// compile.

use std::borrow::Cow;

use aether_hub_protocol::{
    EnumVariant, KindDescriptor, KindLabels, KindShape, LabelNode, NamedField, SchemaCell,
    SchemaShape, SchemaType, VariantLabel,
};
use wasmparser::{Parser, Payload};

/// Section name the derive writes to for canonical schema bytes.
/// Must match `aether-mail-derive`'s
/// `#[link_section = "aether.kinds"]`.
pub const MANIFEST_SECTION: &str = "aether.kinds";

/// Labels sidecar section — nominal reconstruction data (Rust type
/// paths, field/variant names) that the hub's `describe_kinds` and
/// JSON-param encoder rely on. Optional on the wire but every
/// derive-emitted component ships it; absence degrades schemas to
/// anonymous field names.
pub const LABELS_SECTION: &str = "aether.kinds.labels";

const SUPPORTED_VERSIONS: &[u8] = &[0x02];

/// Decode every kind record in the component's `aether.kinds` and
/// (when present) `aether.kinds.labels` sections, merging each pair
/// into a named `KindDescriptor`. Components without the canonical
/// section return an empty vec — matches the behavior of a
/// `LoadComponent` with empty `kinds` and lets WAT-only tests keep
/// working. Components with canonical bytes but no labels produce
/// anonymous descriptors (empty field names) — the load succeeds at
/// the substrate but hub-side encode-from-JSON is expected to error
/// on such kinds.
pub fn read_from_bytes(wasm: &[u8]) -> Result<Vec<KindDescriptor>, String> {
    let mut shapes: Vec<KindShape> = Vec::new();
    let mut labels: Vec<KindLabels> = Vec::new();

    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|e| format!("wasmparser: {e}"))?;
        let Payload::CustomSection(reader) = payload else {
            continue;
        };
        match reader.name() {
            MANIFEST_SECTION => decode_records(MANIFEST_SECTION, reader.data(), &mut shapes)?,
            LABELS_SECTION => decode_records(LABELS_SECTION, reader.data(), &mut labels)?,
            _ => continue,
        }
    }

    let mut descriptors = Vec::with_capacity(shapes.len());
    for (idx, shape) in shapes.into_iter().enumerate() {
        let label = labels.get(idx);
        descriptors.push(merge(shape, label));
    }
    Ok(descriptors)
}

/// Walk one custom section: `[version][postcard(T)]` records until
/// the section is exhausted. Abort on unknown version or postcard
/// decode error.
fn decode_records<T: serde::de::DeserializeOwned>(
    section_name: &str,
    data: &[u8],
    out: &mut Vec<T>,
) -> Result<(), String> {
    let mut cursor = data;
    while !cursor.is_empty() {
        let version = cursor[0];
        if !SUPPORTED_VERSIONS.contains(&version) {
            return Err(format!(
                "{section_name}: record version {version:#x} not understood by this substrate build"
            ));
        }
        let body = &cursor[1..];
        match postcard::take_from_bytes::<T>(body) {
            Ok((record, rest)) => {
                out.push(record);
                cursor = rest;
            }
            Err(e) => {
                return Err(format!(
                    "{section_name}: postcard decode failed at record {}: {e}",
                    out.len() + 1
                ));
            }
        }
    }
    Ok(())
}

/// Merge a positional `SchemaShape` with its parallel-shape
/// `LabelNode` into a named `SchemaType`. `None` labels produce
/// anonymous field/variant/type names; the shape drives every
/// structural decision. Shape/labels shape mismatches (one's a
/// `Struct` and the other's an `Enum`) fall back to anonymous —
/// structural decisions follow the schema side since that's what
/// the canonical bytes (and `K::ID`) agreed on.
fn merge(shape: KindShape, labels: Option<&KindLabels>) -> KindDescriptor {
    let name = shape.name.into_owned();
    let schema = merge_schema(&shape.schema, labels.map(|l| &l.root));
    KindDescriptor { name, schema }
}

fn merge_schema(shape: &SchemaShape, label: Option<&LabelNode>) -> SchemaType {
    match shape {
        SchemaShape::Unit => SchemaType::Unit,
        SchemaShape::Bool => SchemaType::Bool,
        SchemaShape::Scalar(p) => SchemaType::Scalar(*p),
        SchemaShape::String => SchemaType::String,
        SchemaShape::Bytes => SchemaType::Bytes,
        SchemaShape::Option(inner) => {
            let inner_label = match label {
                Some(LabelNode::Option(cell)) => Some(&**cell),
                _ => None,
            };
            SchemaType::Option(SchemaCell::owned(merge_schema(inner, inner_label)))
        }
        SchemaShape::Vec(inner) => {
            let inner_label = match label {
                Some(LabelNode::Vec(cell)) => Some(&**cell),
                _ => None,
            };
            SchemaType::Vec(SchemaCell::owned(merge_schema(inner, inner_label)))
        }
        SchemaShape::Array { element, len } => {
            let element_label = match label {
                Some(LabelNode::Array(cell)) => Some(&**cell),
                _ => None,
            };
            SchemaType::Array {
                element: SchemaCell::owned(merge_schema(element, element_label)),
                len: *len,
            }
        }
        SchemaShape::Struct { fields, repr_c } => {
            let (field_names, field_labels) = match label {
                Some(LabelNode::Struct {
                    field_names,
                    fields: field_labels,
                    ..
                }) => (Some(&**field_names), Some(&**field_labels)),
                _ => (None, None),
            };
            let named_fields: Vec<NamedField> = fields
                .iter()
                .enumerate()
                .map(|(idx, ft)| {
                    let name = field_names
                        .and_then(|names| names.get(idx))
                        .cloned()
                        .unwrap_or_else(|| Cow::Owned(String::new()));
                    let field_label = field_labels.and_then(|labels| labels.get(idx));
                    NamedField {
                        name,
                        ty: merge_schema(ft, field_label),
                    }
                })
                .collect();
            SchemaType::Struct {
                fields: Cow::Owned(named_fields),
                repr_c: *repr_c,
            }
        }
        SchemaShape::Enum { variants } => {
            let variant_labels = match label {
                Some(LabelNode::Enum { variants: vs, .. }) => Some(&**vs),
                _ => None,
            };
            let merged: Vec<EnumVariant> = variants
                .iter()
                .enumerate()
                .map(|(idx, v)| merge_variant(v, variant_labels.and_then(|vs| vs.get(idx))))
                .collect();
            SchemaType::Enum {
                variants: Cow::Owned(merged),
            }
        }
    }
}

fn merge_variant(
    shape: &aether_hub_protocol::VariantShape,
    label: Option<&VariantLabel>,
) -> EnumVariant {
    match shape {
        aether_hub_protocol::VariantShape::Unit { discriminant } => {
            let name = match label {
                Some(VariantLabel::Unit { name }) => name.clone(),
                _ => Cow::Owned(String::new()),
            };
            EnumVariant::Unit {
                name,
                discriminant: *discriminant,
            }
        }
        aether_hub_protocol::VariantShape::Tuple {
            discriminant,
            fields,
        } => {
            let (name, field_labels) = match label {
                Some(VariantLabel::Tuple { name, fields: fl }) => (name.clone(), Some(&**fl)),
                _ => (Cow::Owned(String::new()), None),
            };
            let merged: Vec<SchemaType> = fields
                .iter()
                .enumerate()
                .map(|(idx, ft)| merge_schema(ft, field_labels.and_then(|fl| fl.get(idx))))
                .collect();
            EnumVariant::Tuple {
                name,
                discriminant: *discriminant,
                fields: Cow::Owned(merged),
            }
        }
        aether_hub_protocol::VariantShape::Struct {
            discriminant,
            fields,
        } => {
            let (name, field_names, field_labels) = match label {
                Some(VariantLabel::Struct {
                    name,
                    field_names: fn_,
                    fields: fl,
                }) => (name.clone(), Some(&**fn_), Some(&**fl)),
                _ => (Cow::Owned(String::new()), None, None),
            };
            let named: Vec<NamedField> = fields
                .iter()
                .enumerate()
                .map(|(idx, ft)| {
                    let field_name = field_names
                        .and_then(|names| names.get(idx))
                        .cloned()
                        .unwrap_or_else(|| Cow::Owned(String::new()));
                    NamedField {
                        name: field_name,
                        ty: merge_schema(ft, field_labels.and_then(|fl| fl.get(idx))),
                    }
                })
                .collect();
            EnumVariant::Struct {
                name,
                discriminant: *discriminant,
                fields: Cow::Owned(named),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_hub_protocol::{LabelCell, LabelNode, Primitive, SchemaShape, VariantShape};

    fn wasm_with_section(section_name: &str, section: &[u8]) -> Vec<u8> {
        let escaped: String = section.iter().map(|b| format!("\\{b:02x}")).collect();
        let wat =
            format!(r#"(module (@custom "{section_name}" "{escaped}") (func (export "noop")))"#);
        wat::parse_str(wat).unwrap()
    }

    fn wasm_with_two_sections(canonical: &[u8], labels: &[u8]) -> Vec<u8> {
        let esc = |bs: &[u8]| -> String { bs.iter().map(|b| format!("\\{b:02x}")).collect() };
        let wat = format!(
            r#"(module
                (@custom "{MANIFEST_SECTION}" "{}")
                (@custom "{LABELS_SECTION}" "{}")
                (func (export "noop")))"#,
            esc(canonical),
            esc(labels),
        );
        wat::parse_str(wat).unwrap()
    }

    #[test]
    fn reads_single_record_with_labels() {
        let shape = KindShape {
            name: Cow::Borrowed("test.kind"),
            schema: SchemaShape::Struct {
                fields: vec![SchemaShape::Scalar(Primitive::U32)],
                repr_c: true,
            },
        };
        let labels = KindLabels {
            kind_label: Cow::Borrowed("my_crate::TestKind"),
            root: LabelNode::Struct {
                type_label: Some(Cow::Borrowed("my_crate::TestKind")),
                field_names: Cow::Owned(vec![Cow::Borrowed("x")]),
                fields: Cow::Owned(vec![LabelNode::Anonymous]),
            },
        };
        let mut canonical = vec![0x02u8];
        canonical.extend(postcard::to_allocvec(&shape).unwrap());
        let mut labels_bytes = vec![0x02u8];
        labels_bytes.extend(postcard::to_allocvec(&labels).unwrap());

        let wasm = wasm_with_two_sections(&canonical, &labels_bytes);
        let descs = read_from_bytes(&wasm).unwrap();

        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].name, "test.kind");
        let SchemaType::Struct { fields, repr_c } = &descs[0].schema else {
            panic!("expected Struct");
        };
        assert!(*repr_c);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "x");
        assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
    }

    #[test]
    fn reads_multiple_records_pair_by_index() {
        let shapes = [
            KindShape {
                name: Cow::Borrowed("a"),
                schema: SchemaShape::Unit,
            },
            KindShape {
                name: Cow::Borrowed("b"),
                schema: SchemaShape::Scalar(Primitive::U8),
            },
        ];
        let labels_a = KindLabels {
            kind_label: Cow::Borrowed("my::A"),
            root: LabelNode::Anonymous,
        };
        let labels_b = KindLabels {
            kind_label: Cow::Borrowed("my::B"),
            root: LabelNode::Anonymous,
        };

        let mut canonical = Vec::new();
        for s in &shapes {
            canonical.push(0x02);
            canonical.extend(postcard::to_allocvec(s).unwrap());
        }
        let mut labels_bytes = Vec::new();
        for l in [&labels_a, &labels_b] {
            labels_bytes.push(0x02);
            labels_bytes.extend(postcard::to_allocvec(l).unwrap());
        }

        let wasm = wasm_with_two_sections(&canonical, &labels_bytes);
        let descs = read_from_bytes(&wasm).unwrap();
        assert_eq!(descs.len(), 2);
        assert_eq!(descs[0].name, "a");
        assert_eq!(descs[0].schema, SchemaType::Unit);
        assert_eq!(descs[1].name, "b");
        assert_eq!(descs[1].schema, SchemaType::Scalar(Primitive::U8));
    }

    #[test]
    fn absent_sections_return_empty() {
        let wasm = wat::parse_str(r#"(module (func (export "noop")))"#).unwrap();
        let descs = read_from_bytes(&wasm).unwrap();
        assert!(descs.is_empty());
    }

    #[test]
    fn canonical_without_labels_produces_anonymous_names() {
        let shape = KindShape {
            name: Cow::Borrowed("t.anon"),
            schema: SchemaShape::Struct {
                fields: vec![SchemaShape::Scalar(Primitive::U32)],
                repr_c: false,
            },
        };
        let mut canonical = vec![0x02u8];
        canonical.extend(postcard::to_allocvec(&shape).unwrap());
        let wasm = wasm_with_section(MANIFEST_SECTION, &canonical);
        let descs = read_from_bytes(&wasm).unwrap();
        assert_eq!(descs.len(), 1);
        let SchemaType::Struct { fields, .. } = &descs[0].schema else {
            panic!("expected Struct");
        };
        // Labels missing → anonymous field name (empty string).
        assert_eq!(fields[0].name, "");
    }

    #[test]
    fn unknown_version_errors() {
        let wasm = wasm_with_section(MANIFEST_SECTION, &[0xff, 0x00]);
        let err = read_from_bytes(&wasm).unwrap_err();
        assert!(err.contains("0xff"), "err was: {err}");
    }

    #[test]
    fn enum_shape_merges_variants_and_field_names() {
        let shape = KindShape {
            name: Cow::Borrowed("test.result"),
            schema: SchemaShape::Enum {
                variants: vec![
                    VariantShape::Unit { discriminant: 0 },
                    VariantShape::Tuple {
                        discriminant: 1,
                        fields: vec![SchemaShape::Scalar(Primitive::U64)],
                    },
                    VariantShape::Struct {
                        discriminant: 2,
                        fields: vec![SchemaShape::String],
                    },
                ],
            },
        };
        let labels = KindLabels {
            kind_label: Cow::Borrowed("my::Outcome"),
            root: LabelNode::Enum {
                type_label: Some(Cow::Borrowed("my::Outcome")),
                variants: Cow::Owned(vec![
                    VariantLabel::Unit {
                        name: Cow::Borrowed("Pending"),
                    },
                    VariantLabel::Tuple {
                        name: Cow::Borrowed("Ok"),
                        fields: Cow::Owned(vec![LabelNode::Anonymous]),
                    },
                    VariantLabel::Struct {
                        name: Cow::Borrowed("Err"),
                        field_names: Cow::Owned(vec![Cow::Borrowed("reason")]),
                        fields: Cow::Owned(vec![LabelNode::Anonymous]),
                    },
                ]),
            },
        };
        let mut canonical = vec![0x02u8];
        canonical.extend(postcard::to_allocvec(&shape).unwrap());
        let mut labels_bytes = vec![0x02u8];
        labels_bytes.extend(postcard::to_allocvec(&labels).unwrap());
        let wasm = wasm_with_two_sections(&canonical, &labels_bytes);
        let descs = read_from_bytes(&wasm).unwrap();
        let SchemaType::Enum { variants } = &descs[0].schema else {
            panic!("expected Enum");
        };
        assert_eq!(variants.len(), 3);
        let EnumVariant::Unit { name, .. } = &variants[0] else {
            panic!("expected Unit");
        };
        assert_eq!(name, "Pending");
        let EnumVariant::Tuple { name, fields, .. } = &variants[1] else {
            panic!("expected Tuple");
        };
        assert_eq!(name, "Ok");
        assert_eq!(fields[0], SchemaType::Scalar(Primitive::U64));
        let EnumVariant::Struct { name, fields, .. } = &variants[2] else {
            panic!("expected Struct");
        };
        assert_eq!(name, "Err");
        assert_eq!(fields[0].name, "reason");
        assert_eq!(fields[0].ty, SchemaType::String);
    }

    #[test]
    fn array_of_struct_merges_nested_labels() {
        // Triangle { verts: [Vertex; 3] } — catches the ADR-0032
        // regression the syntactic walker used to have: nested
        // user-types get their field names back through trait dispatch.
        let shape = KindShape {
            name: Cow::Borrowed("test.triangle"),
            schema: SchemaShape::Struct {
                fields: vec![SchemaShape::Array {
                    element: Box::new(SchemaShape::Struct {
                        fields: vec![
                            SchemaShape::Scalar(Primitive::F32),
                            SchemaShape::Scalar(Primitive::F32),
                        ],
                        repr_c: true,
                    }),
                    len: 3,
                }],
                repr_c: true,
            },
        };
        let vertex_labels = LabelNode::Struct {
            type_label: Some(Cow::Borrowed("my::Vertex")),
            field_names: Cow::Owned(vec![Cow::Borrowed("x"), Cow::Borrowed("y")]),
            fields: Cow::Owned(vec![LabelNode::Anonymous, LabelNode::Anonymous]),
        };
        // Array's child goes through a LabelCell::Owned because we
        // build it at runtime here. Derive-time would use Static.
        let labels = KindLabels {
            kind_label: Cow::Borrowed("my::Triangle"),
            root: LabelNode::Struct {
                type_label: Some(Cow::Borrowed("my::Triangle")),
                field_names: Cow::Owned(vec![Cow::Borrowed("verts")]),
                fields: Cow::Owned(vec![LabelNode::Array(LabelCell::owned(vertex_labels))]),
            },
        };
        let mut canonical = vec![0x02u8];
        canonical.extend(postcard::to_allocvec(&shape).unwrap());
        let mut labels_bytes = vec![0x02u8];
        labels_bytes.extend(postcard::to_allocvec(&labels).unwrap());
        let wasm = wasm_with_two_sections(&canonical, &labels_bytes);
        let descs = read_from_bytes(&wasm).unwrap();
        let SchemaType::Struct { fields, .. } = &descs[0].schema else {
            panic!("expected Struct");
        };
        assert_eq!(fields[0].name, "verts");
        let SchemaType::Array { element, len } = &fields[0].ty else {
            panic!("expected Array");
        };
        assert_eq!(*len, 3);
        let SchemaType::Struct {
            fields: inner_fields,
            ..
        } = &**element
        else {
            panic!("expected nested Struct");
        };
        assert_eq!(inner_fields[0].name, "x");
        assert_eq!(inner_fields[1].name, "y");
    }
}
