// ADR-0028 / ADR-0032: read a component's embedded kind manifest
// from two wasm custom sections — `aether.kinds` (canonical bytes,
// the `Kind::ID` hash input) and `aether.kinds.labels` (Rust-nominal
// sidecar: type paths, field names, variant names).
//
// Record formats:
//   `aether.kinds`         — [0x03] [postcard(KindShape)] [is_stream_byte]
//   `aether.kinds.labels`  — [0x03] [postcard(KindLabels)]
//
// `aether.kinds` records are identified by computing
// `kind_id_from_parts(&shape.name, &shape.schema)`. `aether.kinds.labels`
// records carry their `kind_id` inline (v0x03 field), so the reader
// indexes labels by id and looks up per shape. Pairing is robust
// against emit order, duplicates, and mixed emitters (the Kind derive,
// `#[handlers]` retention for kinds defined in rlib dependencies, and
// future external sources of kind metadata).
//
// Pre-v0x03 labels lacked `kind_id` and were paired by declaration
// order. That was fragile once any second emitter wrote to only one
// of the two sections (issue tracked in the
// "demo.sokoban.load_level.id" empty-field-name regression); v0x03
// rejects old-format bytes loudly so a rebuild-required boundary is
// explicit rather than "single-field cast kinds have empty fields
// and encode-from-JSON silently fails."
//
// The parser walks each section sequentially; postcard stops decoding
// exactly at the record's end, so the next byte is the next record's
// version tag. Unknown version bytes abort the parse rather than
// silently skip.
//
// Wasmtime 30 doesn't expose custom sections on `Module`, so we walk
// the raw bytes via `wasmparser` before compilation. The section data
// lives in the binary's original bytes anyway — compilation isn't a
// prerequisite for reading it, and parsing the raw bytes lets us
// fail on an unknown manifest version before spending cycles on
// compile.

use std::borrow::Cow;
use std::collections::HashMap;

use aether_data::{
    EnumVariant, INPUTS_SECTION, INPUTS_SECTION_VERSION, InputsRecord, KindDescriptor, KindLabels,
    KindShape, LabelNode, NamedField, SchemaCell, SchemaShape, SchemaType, VariantLabel,
    canonical::kind_id_from_shape,
};
use aether_kinds::{ComponentCapabilities, FallbackCapability, HandlerCapability};
use wasmparser::{Parser, Payload};

/// Section name the derive writes to for canonical schema bytes.
/// Must match `aether-data-derive`'s
/// `#[link_section = "aether.kinds"]`.
pub const MANIFEST_SECTION: &str = "aether.kinds";

/// Labels sidecar section — nominal reconstruction data (Rust type
/// paths, field/variant names) that the hub's `describe_kinds` and
/// JSON-param encoder rely on. Optional on the wire but every
/// derive-emitted component ships it; absence degrades schemas to
/// anonymous field names.
pub const LABELS_SECTION: &str = "aether.kinds.labels";

/// Default-mailbox-name section. Issue 525 Phase 1B: each component
/// declares `Component::NAMESPACE` and `export!()` pins the bytes
/// here; substrate's `load_component` reads the section as the default
/// recipient name when the load payload omits an explicit `name`. The
/// payload is the raw UTF-8 bytes — no version prefix, no postcard
/// wrapper, since it's a single fixed-shape string with no anticipated
/// evolution.
pub const NAMESPACE_SECTION: &str = "aether.namespace";

/// Wire versions accepted in `aether.kinds`. The shape record's bytes
/// are `Kind::ID` hash input, so the format is pinned indefinitely at
/// 0x03's canonical-bytes layout — any change there would shift every
/// id. v0x03 added a trailing `is_stream` byte AFTER the canonical
/// bytes (ADR-0068); the canonical bytes themselves are unchanged
/// from v0x02 so the hash is stable, but the v-byte signals "this
/// record carries the stream metadata flag." v0x02 is no longer
/// accepted — a loud rebuild-required boundary, same as the v0x02→
/// v0x03 bump on `aether.kinds.labels`.
const KINDS_VERSION: u8 = 0x03;

/// Wire versions accepted in `aether.kinds.labels`. v0x03 added
/// `kind_id` to `KindLabels`, making records self-identifying so the
/// reader pairs by id rather than by declaration order. v0x02 is no
/// longer accepted — a loud rebuild-required boundary.
const LABELS_SUPPORTED_VERSIONS: &[u8] = &[0x03];

/// Decode every kind record in the component's `aether.kinds` and
/// (when present) `aether.kinds.labels` sections, merging labels into
/// each shape by `Kind::ID`. Components without the canonical section
/// return an empty vec — matches the behavior of a `LoadComponent`
/// with empty `kinds` and lets WAT-only tests keep working. Shapes
/// without a matching labels record produce anonymous descriptors
/// (empty field names) — the load succeeds at the substrate but
/// hub-side encode-from-JSON is expected to error on such kinds.
/// Orphan labels (a labels record whose id has no shape) are
/// silently ignored so third-party emitters can add labels for kinds
/// not present in this particular binary without breaking loads.
pub fn read_from_bytes(wasm: &[u8]) -> Result<Vec<KindDescriptor>, String> {
    let mut kinds: Vec<(KindShape, bool)> = Vec::new();
    let mut labels_list: Vec<KindLabels> = Vec::new();

    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|e| format!("wasmparser: {e}"))?;
        let Payload::CustomSection(reader) = payload else {
            continue;
        };
        match reader.name() {
            MANIFEST_SECTION => decode_kinds_records(reader.data(), &mut kinds)?,
            LABELS_SECTION => decode_records(
                LABELS_SECTION,
                LABELS_SUPPORTED_VERSIONS,
                reader.data(),
                &mut labels_list,
            )?,
            _ => continue,
        }
    }

    let labels_by_id: HashMap<aether_data::KindId, KindLabels> =
        labels_list.into_iter().map(|l| (l.kind_id, l)).collect();

    let mut descriptors = Vec::with_capacity(kinds.len());
    for (shape, is_stream) in kinds {
        let id = aether_data::KindId(kind_id_from_shape(&shape));
        let label = labels_by_id.get(&id);
        descriptors.push(merge(shape, label, is_stream));
    }
    Ok(descriptors)
}

/// Walk the `aether.kinds` v0x03 section: each record is
/// `[0x03][postcard(KindShape)][is_stream_byte]`. Postcard stops
/// decoding exactly at the shape record's end, so the next byte is
/// the trailing `is_stream` flag, and the byte after that is the
/// next record's version tag.
fn decode_kinds_records(data: &[u8], out: &mut Vec<(KindShape, bool)>) -> Result<(), String> {
    let mut cursor = data;
    while !cursor.is_empty() {
        let version = cursor[0];
        if version != KINDS_VERSION {
            return Err(format!(
                "{MANIFEST_SECTION}: record version {version:#x} not understood by this substrate build"
            ));
        }
        let body = &cursor[1..];
        match postcard::take_from_bytes::<KindShape>(body) {
            Ok((shape, rest)) => {
                if rest.is_empty() {
                    return Err(format!(
                        "{MANIFEST_SECTION}: record {} truncated — missing trailing is_stream byte",
                        out.len() + 1
                    ));
                }
                let is_stream = rest[0] != 0;
                out.push((shape, is_stream));
                cursor = &rest[1..];
            }
            Err(e) => {
                return Err(format!(
                    "{MANIFEST_SECTION}: postcard decode failed at record {}: {e}",
                    out.len() + 1
                ));
            }
        }
    }
    Ok(())
}

/// Read the component's [`NAMESPACE_SECTION`] payload (issue 525
/// Phase 1B) as a UTF-8 string. Returns `None` when the section is
/// absent (component built against a pre-Phase-1B SDK, or built with a
/// hand-rolled `export!` shim) so callers fall back to a derived
/// name. Returns an `Err` only on malformed UTF-8 — the substrate
/// surfaces that as a load failure rather than silently using a
/// different name.
pub fn read_namespace_from_bytes(wasm: &[u8]) -> Result<Option<String>, String> {
    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|e| format!("wasmparser: {e}"))?;
        let Payload::CustomSection(reader) = payload else {
            continue;
        };
        if reader.name() == NAMESPACE_SECTION {
            return std::str::from_utf8(reader.data())
                .map(|s| Some(s.to_owned()))
                .map_err(|e| format!("{NAMESPACE_SECTION}: invalid UTF-8: {e}"));
        }
    }
    Ok(None)
}

/// Decode the component's `aether.kinds.inputs` section (ADR-0033)
/// into a structured `ComponentCapabilities`. Components without the
/// section return `ComponentCapabilities::default()` — valid only for
/// components built against the pre-ADR-0033 SDK, which are no longer
/// produceable after phase 3 retired the `type Kinds` / `fn receive`
/// surface.
///
/// Record shape: `[0x01][postcard(InputsRecord)]` back-to-back. The
/// classifier walks the records in declaration order: every Handler
/// enters `handlers`, at most one Fallback populates `fallback`, and
/// at most one Component populates `doc`. Duplicate Fallback /
/// Component records are a substrate-rejected load error — the macro
/// emits at most one of each so seeing two means the component
/// binary was built against a different manifest contract.
pub fn read_inputs_from_bytes(wasm: &[u8]) -> Result<ComponentCapabilities, String> {
    let mut records: Vec<InputsRecord> = Vec::new();

    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|e| format!("wasmparser: {e}"))?;
        let Payload::CustomSection(reader) = payload else {
            continue;
        };
        if reader.name() != INPUTS_SECTION {
            continue;
        }
        decode_inputs_records(reader.data(), &mut records)?;
    }

    let mut caps = ComponentCapabilities::default();
    for record in records {
        match record {
            InputsRecord::Handler { id, name, doc } => {
                caps.handlers.push(HandlerCapability {
                    id,
                    name: name.into_owned(),
                    doc: doc.map(|d| d.into_owned()),
                });
            }
            InputsRecord::Fallback { doc } => {
                if caps.fallback.is_some() {
                    return Err(format!(
                        "{INPUTS_SECTION}: duplicate Fallback record — macro emits at most one"
                    ));
                }
                caps.fallback = Some(FallbackCapability {
                    doc: doc.map(|d| d.into_owned()),
                });
            }
            InputsRecord::Component { doc } => {
                if caps.doc.is_some() {
                    return Err(format!(
                        "{INPUTS_SECTION}: duplicate Component record — macro emits at most one"
                    ));
                }
                caps.doc = Some(doc.into_owned());
            }
        }
    }
    Ok(caps)
}

fn decode_inputs_records(data: &[u8], out: &mut Vec<InputsRecord>) -> Result<(), String> {
    let mut cursor = data;
    while !cursor.is_empty() {
        let version = cursor[0];
        if version != INPUTS_SECTION_VERSION {
            return Err(format!(
                "{INPUTS_SECTION}: record version {version:#x} not understood by this substrate build"
            ));
        }
        let body = &cursor[1..];
        match postcard::take_from_bytes::<InputsRecord>(body) {
            Ok((record, rest)) => {
                out.push(record);
                cursor = rest;
            }
            Err(e) => {
                return Err(format!(
                    "{INPUTS_SECTION}: postcard decode failed at record {}: {e}",
                    out.len() + 1
                ));
            }
        }
    }
    Ok(())
}

/// Walk one custom section: `[version][postcard(T)]` records until
/// the section is exhausted. Abort on unknown version or postcard
/// decode error. Per-section version allowlists are passed in so the
/// shape and labels sections can evolve independently.
fn decode_records<T: serde::de::DeserializeOwned>(
    section_name: &str,
    supported_versions: &[u8],
    data: &[u8],
    out: &mut Vec<T>,
) -> Result<(), String> {
    let mut cursor = data;
    while !cursor.is_empty() {
        let version = cursor[0];
        if !supported_versions.contains(&version) {
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
fn merge(shape: KindShape, labels: Option<&KindLabels>, is_stream: bool) -> KindDescriptor {
    let name = shape.name.into_owned();
    let schema = merge_schema(&shape.schema, labels.map(|l| &l.root));
    KindDescriptor {
        name,
        schema,
        is_stream,
    }
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
        SchemaShape::Ref(inner) => {
            let inner_label = match label {
                Some(LabelNode::Ref(cell)) => Some(&**cell),
                _ => None,
            };
            SchemaType::Ref(SchemaCell::owned(merge_schema(inner, inner_label)))
        }
        SchemaShape::Map { key, value } => {
            // Issue #232: parallel-walk the labels Map arm so any
            // nominal info inside key/value types (struct field
            // names etc.) survives the shape→type rebuild. Mismatched
            // labels (or no labels at all) collapse to anonymous on
            // each side independently — the schema arm always wins.
            let (key_label, value_label) = match label {
                Some(LabelNode::Map { key: kc, value: vc }) => (Some(&**kc), Some(&**vc)),
                _ => (None, None),
            };
            SchemaType::Map {
                key: SchemaCell::owned(merge_schema(key, key_label)),
                value: SchemaCell::owned(merge_schema(value, value_label)),
            }
        }
        SchemaShape::TypeId(id) => SchemaType::TypeId(*id),
    }
}

fn merge_variant(shape: &aether_data::VariantShape, label: Option<&VariantLabel>) -> EnumVariant {
    match shape {
        aether_data::VariantShape::Unit { discriminant } => {
            let name = match label {
                Some(VariantLabel::Unit { name }) => name.clone(),
                _ => Cow::Owned(String::new()),
            };
            EnumVariant::Unit {
                name,
                discriminant: *discriminant,
            }
        }
        aether_data::VariantShape::Tuple {
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
        aether_data::VariantShape::Struct {
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
    use aether_data::{LabelCell, LabelNode, Primitive, SchemaShape, VariantShape};
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

    /// Append `[0x03][postcard(shape)][is_stream_byte]` to `canonical`.
    /// Matches what the Kind derive emits into the `aether.kinds`
    /// section after ADR-0068 bumped the version. Tests pass `false`
    /// for `is_stream` since the synthetic `KindShape`s here are
    /// non-stream demo kinds.
    fn push_shape(canonical: &mut Vec<u8>, shape: &KindShape) {
        push_shape_with_stream(canonical, shape, false);
    }

    fn push_shape_with_stream(canonical: &mut Vec<u8>, shape: &KindShape, is_stream: bool) {
        canonical.push(0x03);
        canonical.extend(postcard::to_allocvec(shape).unwrap());
        canonical.push(if is_stream { 1 } else { 0 });
    }

    /// Append `[0x03][postcard(labels)]` to `labels_bytes`, and stamp
    /// `labels.kind_id` from the paired shape so the reader's by-id
    /// merge finds it. Matches what the Kind derive emits into
    /// `aether.kinds.labels` (v0x03 adds `kind_id`).
    fn push_labels(labels_bytes: &mut Vec<u8>, shape: &KindShape, labels: &mut KindLabels) {
        labels.kind_id = aether_data::KindId(kind_id_from_shape(shape));
        labels_bytes.push(0x03);
        labels_bytes.extend(postcard::to_allocvec(labels).unwrap());
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
        let mut labels = KindLabels {
            kind_id: aether_data::KindId(0),
            kind_label: Cow::Borrowed("my_crate::TestKind"),
            root: LabelNode::Struct {
                type_label: Some(Cow::Borrowed("my_crate::TestKind")),
                field_names: Cow::Owned(vec![Cow::Borrowed("x")]),
                fields: Cow::Owned(vec![LabelNode::Anonymous]),
            },
        };
        let mut canonical = Vec::new();
        push_shape(&mut canonical, &shape);
        let mut labels_bytes = Vec::new();
        push_labels(&mut labels_bytes, &shape, &mut labels);

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
    fn reads_multiple_records_pair_by_id() {
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
        let mut labels_a = KindLabels {
            kind_id: aether_data::KindId(0),
            kind_label: Cow::Borrowed("my::A"),
            root: LabelNode::Anonymous,
        };
        let mut labels_b = KindLabels {
            kind_id: aether_data::KindId(0),
            kind_label: Cow::Borrowed("my::B"),
            root: LabelNode::Anonymous,
        };

        let mut canonical = Vec::new();
        for s in &shapes {
            push_shape(&mut canonical, s);
        }
        // Emit labels in REVERSE order relative to shapes, to prove
        // the reader's by-id pairing doesn't rely on declaration order.
        let mut labels_bytes = Vec::new();
        push_labels(&mut labels_bytes, &shapes[1], &mut labels_b);
        push_labels(&mut labels_bytes, &shapes[0], &mut labels_a);

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
        let mut canonical = vec![0x03u8];
        canonical.extend(postcard::to_allocvec(&shape).unwrap());
        canonical.push(0u8); // is_stream=false trailing byte (ADR-0068)
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
    fn labels_v0x02_rejected_loudly() {
        // Pre-id-pairing labels records lacked `kind_id`; a substrate
        // running this build against an old wasm build would silently
        // fail-merge everything and surface empty field names only at
        // hub encode time. Reject with a clear version-mismatch error
        // instead.
        let old_labels_payload = [0x02, 0x00];
        let wasm = wasm_with_section(LABELS_SECTION, &old_labels_payload);
        let err = read_from_bytes(&wasm).unwrap_err();
        assert!(err.contains("0x2"), "err was: {err}");
        assert!(err.contains(LABELS_SECTION), "err was: {err}");
    }

    #[test]
    fn duplicate_kinds_records_tolerated_under_by_id_pairing() {
        // `#[handlers]` retention emits both an `aether.kinds` and an
        // `aether.kinds.labels` record per handler kind; when the
        // defining crate also emits via `Kind` derive, a kind ends up
        // with duplicate records in both sections. The by-id merge
        // tolerates duplicates because records with the same id are
        // byte-identical by construction (name + schema → canonical
        // bytes → hash).
        let shape = KindShape {
            name: Cow::Borrowed("test.dup"),
            schema: SchemaShape::Struct {
                fields: vec![SchemaShape::Scalar(Primitive::U32)],
                repr_c: true,
            },
        };
        let mut labels = KindLabels {
            kind_id: aether_data::KindId(0),
            kind_label: Cow::Borrowed("my::Dup"),
            root: LabelNode::Struct {
                type_label: Some(Cow::Borrowed("my::Dup")),
                field_names: Cow::Owned(vec![Cow::Borrowed("n")]),
                fields: Cow::Owned(vec![LabelNode::Anonymous]),
            },
        };
        let mut canonical = Vec::new();
        push_shape(&mut canonical, &shape);
        push_shape(&mut canonical, &shape);
        let mut labels_bytes = Vec::new();
        push_labels(&mut labels_bytes, &shape, &mut labels);
        push_labels(&mut labels_bytes, &shape, &mut labels);
        let wasm = wasm_with_two_sections(&canonical, &labels_bytes);
        let descs = read_from_bytes(&wasm).unwrap();
        // Two shape records surface as two descriptors; merging each
        // with the same labels record by id is the correct behavior
        // (the substrate's `register_kind_with_descriptor` then
        // dedupes by id on register).
        assert_eq!(descs.len(), 2);
        for desc in &descs {
            let SchemaType::Struct { fields, .. } = &desc.schema else {
                panic!("expected Struct");
            };
            assert_eq!(fields[0].name, "n");
        }
    }

    #[test]
    fn orphan_labels_record_ignored() {
        // A labels record whose `kind_id` doesn't match any shape is
        // harmlessly ignored. Future-proofs the reader against mixed
        // manifests where a third-party emitter contributes labels
        // for kinds not in this particular binary.
        let shape = KindShape {
            name: Cow::Borrowed("present"),
            schema: SchemaShape::Unit,
        };
        let mut orphan = KindLabels {
            // Deliberately a id that won't match `shape`.
            kind_id: aether_data::KindId(0xDEADBEEF_DEADBEEF),
            kind_label: Cow::Borrowed("my::Missing"),
            root: LabelNode::Anonymous,
        };
        let mut canonical = Vec::new();
        push_shape(&mut canonical, &shape);
        let mut labels_bytes = Vec::new();
        labels_bytes.push(0x03);
        labels_bytes.extend(postcard::to_allocvec(&orphan).unwrap());
        let wasm = wasm_with_two_sections(&canonical, &labels_bytes);
        let descs = read_from_bytes(&wasm).unwrap();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].name, "present");
        let _ = &mut orphan;
    }

    #[test]
    fn sokoban_load_level_single_field_has_name() {
        // Regression: `#[handlers]` retention historically wrote kinds
        // records into `aether.kinds` without parallel labels records
        // into `aether.kinds.labels`, desyncing the old by-index
        // pairing. `demo.sokoban.load_level`'s single `id` field came
        // back with an empty name, blocking hub encode-from-params
        // for that kind. With by-id pairing + parallel labels
        // retention the field name survives all emitter
        // configurations. Skipped when the wasm isn't built.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/wasm32-unknown-unknown/release/aether_demo_sokoban.wasm"
        );
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping: sokoban wasm not built at {path}");
            return;
        };
        let descs = read_from_bytes(&bytes).expect("decode");
        let load_level = descs
            .iter()
            .find(|d| d.name == "demo.sokoban.load_level")
            .expect("load_level descriptor present");
        let SchemaType::Struct { fields, repr_c } = &load_level.schema else {
            panic!("expected Struct, got {:?}", load_level.schema);
        };
        assert!(*repr_c);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "id");
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
        let mut labels = KindLabels {
            kind_id: aether_data::KindId(0),
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
        let mut canonical = Vec::new();
        push_shape(&mut canonical, &shape);
        let mut labels_bytes = Vec::new();
        push_labels(&mut labels_bytes, &shape, &mut labels);
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
        let mut labels = KindLabels {
            kind_id: aether_data::KindId(0),
            kind_label: Cow::Borrowed("my::Triangle"),
            root: LabelNode::Struct {
                type_label: Some(Cow::Borrowed("my::Triangle")),
                field_names: Cow::Owned(vec![Cow::Borrowed("verts")]),
                fields: Cow::Owned(vec![LabelNode::Array(LabelCell::owned(vertex_labels))]),
            },
        };
        let mut canonical = Vec::new();
        push_shape(&mut canonical, &shape);
        let mut labels_bytes = Vec::new();
        push_labels(&mut labels_bytes, &shape, &mut labels);
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

    // ADR-0033: `aether.kinds.inputs` reader. The macro emits
    // `[INPUTS_SECTION_VERSION][postcard(InputsRecord)]` back to back;
    // these tests pin the classifier that turns those records into
    // `ComponentCapabilities`.

    fn inputs_section(records: &[InputsRecord]) -> Vec<u8> {
        let mut out = Vec::new();
        for rec in records {
            out.push(INPUTS_SECTION_VERSION);
            out.extend(postcard::to_allocvec(rec).unwrap());
        }
        out
    }

    #[test]
    fn reads_handlers_plus_component_doc() {
        let section = inputs_section(&[
            InputsRecord::Component {
                doc: "Draws triangles on tick.".into(),
            },
            InputsRecord::Handler {
                id: aether_data::KindId(42),
                name: "aether.tick".into(),
                doc: Some("substrate drives this".into()),
            },
            InputsRecord::Handler {
                id: aether_data::KindId(0xff),
                name: "aether.ping".into(),
                doc: None,
            },
        ]);
        let wasm = wasm_with_section(INPUTS_SECTION, &section);
        let caps = read_inputs_from_bytes(&wasm).unwrap();
        assert_eq!(caps.doc.as_deref(), Some("Draws triangles on tick."));
        assert_eq!(caps.handlers.len(), 2);
        assert_eq!(caps.handlers[0].id, aether_data::KindId(42));
        assert_eq!(caps.handlers[0].name, "aether.tick");
        assert_eq!(
            caps.handlers[0].doc.as_deref(),
            Some("substrate drives this")
        );
        assert_eq!(caps.handlers[1].id, aether_data::KindId(0xff));
        assert_eq!(caps.handlers[1].name, "aether.ping");
        assert!(caps.handlers[1].doc.is_none());
        assert!(caps.fallback.is_none());
    }

    #[test]
    fn reads_fallback_record() {
        let section = inputs_section(&[InputsRecord::Fallback {
            doc: Some("catchall".into()),
        }]);
        let wasm = wasm_with_section(INPUTS_SECTION, &section);
        let caps = read_inputs_from_bytes(&wasm).unwrap();
        assert!(caps.handlers.is_empty());
        let fallback = caps.fallback.expect("fallback present");
        assert_eq!(fallback.doc.as_deref(), Some("catchall"));
    }

    #[test]
    fn absent_section_returns_default_capabilities() {
        let wasm = wat::parse_str(r#"(module (func (export "noop")))"#).unwrap();
        let caps = read_inputs_from_bytes(&wasm).unwrap();
        assert!(caps.handlers.is_empty());
        assert!(caps.fallback.is_none());
        assert!(caps.doc.is_none());
    }

    #[test]
    fn duplicate_fallback_is_rejected() {
        let section = inputs_section(&[
            InputsRecord::Fallback { doc: None },
            InputsRecord::Fallback {
                doc: Some("two".into()),
            },
        ]);
        let wasm = wasm_with_section(INPUTS_SECTION, &section);
        let err = read_inputs_from_bytes(&wasm).unwrap_err();
        assert!(err.contains("duplicate Fallback"), "err: {err}");
    }

    #[test]
    fn unknown_inputs_version_rejected() {
        let wasm = wasm_with_section(INPUTS_SECTION, &[0xff, 0x00]);
        let err = read_inputs_from_bytes(&wasm).unwrap_err();
        assert!(err.contains("0xff"), "err: {err}");
    }

    #[test]
    fn reads_hello_component_inputs_section() {
        // End-to-end sanity: the real hello example's section decodes
        // into the expected shape without wiring every byte by hand.
        // Skips if the wasm isn't built — the cargo-test harness builds
        // workspace members lazily, and example wasms are an opt-in
        // `--examples` build.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/wasm32-unknown-unknown/release/examples/hello.wasm"
        );
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping: hello example wasm not built at {path}");
            return;
        };
        let caps = read_inputs_from_bytes(&bytes).expect("decode");
        assert!(caps.doc.is_some(), "component-level doc present");
        assert_eq!(caps.handlers.len(), 2, "tick + ping handlers");
        let names: Vec<&str> = caps.handlers.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"aether.tick"));
        assert!(names.contains(&"aether.ping"));
        assert!(caps.fallback.is_none(), "strict receiver");
    }
}
