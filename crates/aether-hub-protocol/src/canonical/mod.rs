//! Const-fn canonical serializer for `SchemaType`, `KindLabels`, and
//! `InputsRecord` (ADR-0032, ADR-0033). Produces postcard-compatible
//! bytes at const-eval time so they can be embedded directly in
//! `#[link_section]` statics and hashed to derive `Kind::ID`.
//!
//! The canonical schema format matches postcard of `SchemaShape`
//! byte-for-byte: the only difference from `postcard(SchemaType)` is
//! that `NamedField.name` and `EnumVariant`'s `name` field are
//! omitted. Enum discriminant positions agree between `SchemaType`
//! and `SchemaShape` by construction (same arm order, same field
//! declaration order), so hub-side decode via
//! `postcard::from_bytes::<SchemaShape>` reads the canonical bytes
//! cleanly.
//!
//! Only `SchemaCell::Static` / `LabelCell::Static` variants are
//! legal in const context here. Derive-emitted schemas always use
//! `Static`; passing an `Owned` cell (or an `Owned` `Cow`) to these
//! const fns is a compile-time panic. Runtime consumers (the hub)
//! decode the produced bytes back into `Owned` cells via postcard.
//!
//! Internal submodule layout — module-level re-exports preserve the
//! `canonical::*` surface so no downstream caller needs an edit:
//!   - `primitives`: shared const-fn postcard helpers (varint, str,
//!     option, cow-narrowing) the other submodules build on.
//!   - `schema`: `SchemaType` + `(name, schema)` serializers plus the
//!     runtime `kind_id_from_parts` sibling used by the substrate.
//!   - `labels`: `KindLabels` sidecar serializer.
//!   - `inputs`: `InputsRecord` record encoders (ADR-0033).

// clippy's `ptr_arg` rightly recommends `&[T]` / `&str` over
// `&Cow<[T]>` / `&Cow<str>` in most APIs — deref coercion makes
// `&cow` usable as `&[T]` automatically. But that deref isn't
// `const`, so the helpers below can't accept `&[T]`: they need to
// match on the `Cow` variant to narrow `Cow::Borrowed` to `&[T]`
// by hand. Module-scoped allow documents the single exemption and
// propagates to every child submodule.
#![allow(clippy::ptr_arg)]

mod inputs;
mod labels;
mod primitives;
mod schema;

pub use inputs::{
    inputs_component_len, inputs_fallback_len, inputs_handler_len, write_inputs_component,
    write_inputs_fallback, write_inputs_handler,
};
pub use labels::{canonical_len_labels, canonical_serialize_labels};
pub use primitives::{varint_u32_len, varint_u64_len, varint_usize_len};
pub use schema::{
    canonical_kind_bytes, canonical_len_kind, canonical_len_schema, canonical_serialize_kind,
    canonical_serialize_schema, kind_id_from_parts, kind_id_from_shape,
};

#[cfg(test)]
mod tests {
    //! The contract these tests pin: canonical bytes round-trip through
    //! `postcard::from_bytes::<SchemaShape>` / `postcard::from_bytes::<KindLabels>`
    //! / `postcard::from_bytes::<InputsRecord>`. That's what the hub
    //! relies on after reading the `aether.kinds` /
    //! `aether.kinds.labels` / `aether.kinds.inputs` custom sections.
    //! If these diverge, the hub can't decode what derives produce.
    //!
    //! Each test constructs a schema via `static` so `SchemaCell::Static`
    //! is reachable in const context, runs both passes, and compares
    //! against a hand-built `SchemaShape` that matches the stripped shape.
    use super::*;
    use super::{
        primitives::write_varint_u64,
        schema::{KIND_DOMAIN, fnv1a_64_prefixed},
    };
    use crate::types::{
        EnumVariant, KindLabels, KindShape, LabelCell, LabelNode, NamedField, Primitive,
        SchemaCell, SchemaShape, SchemaType, VariantLabel, VariantShape,
    };
    use aether_id::tag_bits::{HASH_MASK, TAG_KIND, TAG_SHIFT};
    use alloc::borrow::Cow;

    static F32: SchemaType = SchemaType::Scalar(Primitive::F32);

    static VERTEX: SchemaType = SchemaType::Struct {
        fields: Cow::Borrowed(&[
            NamedField {
                name: Cow::Borrowed("x"),
                ty: SchemaType::Scalar(Primitive::F32),
            },
            NamedField {
                name: Cow::Borrowed("y"),
                ty: SchemaType::Scalar(Primitive::F32),
            },
        ]),
        repr_c: true,
    };

    static TRIANGLE: SchemaType = SchemaType::Struct {
        fields: Cow::Borrowed(&[NamedField {
            name: Cow::Borrowed("verts"),
            ty: SchemaType::Array {
                element: SchemaCell::Static(&VERTEX),
                len: 3,
            },
        }]),
        repr_c: true,
    };

    static RESULT: SchemaType = SchemaType::Enum {
        variants: Cow::Borrowed(&[
            EnumVariant::Unit {
                name: Cow::Borrowed("Pending"),
                discriminant: 0,
            },
            EnumVariant::Tuple {
                name: Cow::Borrowed("Ok"),
                discriminant: 1,
                fields: Cow::Borrowed(&[SchemaType::Scalar(Primitive::U64)]),
            },
            EnumVariant::Struct {
                name: Cow::Borrowed("Err"),
                discriminant: 2,
                fields: Cow::Borrowed(&[NamedField {
                    name: Cow::Borrowed("reason"),
                    ty: SchemaType::String,
                }]),
            },
        ]),
    };

    #[test]
    fn canonical_schema_primitive_round_trips_as_shape() {
        const N: usize = canonical_len_schema(&F32);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&F32);
        let shape: SchemaShape = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(shape, SchemaShape::Scalar(Primitive::F32));
    }

    #[test]
    fn canonical_schema_struct_round_trips_as_shape() {
        const N: usize = canonical_len_schema(&VERTEX);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&VERTEX);
        let shape: SchemaShape = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(
            shape,
            SchemaShape::Struct {
                fields: vec![
                    SchemaShape::Scalar(Primitive::F32),
                    SchemaShape::Scalar(Primitive::F32),
                ],
                repr_c: true,
            }
        );
    }

    #[test]
    fn canonical_schema_nested_array_of_struct_round_trips() {
        const N: usize = canonical_len_schema(&TRIANGLE);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&TRIANGLE);
        let shape: SchemaShape = postcard::from_bytes(&BYTES).expect("decode");
        let expected = SchemaShape::Struct {
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
        };
        assert_eq!(shape, expected);
    }

    #[test]
    fn canonical_schema_enum_all_variants_round_trip() {
        const N: usize = canonical_len_schema(&RESULT);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&RESULT);
        let shape: SchemaShape = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(
            shape,
            SchemaShape::Enum {
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
            }
        );
    }

    #[test]
    fn canonical_kind_round_trips_as_kindshape() {
        const NAME: &str = "test.triangle";
        const N: usize = canonical_len_kind(NAME, &TRIANGLE);
        const BYTES: [u8; N] = canonical_serialize_kind::<N>(NAME, &TRIANGLE);
        let shape: KindShape = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(shape.name, "test.triangle");
        let SchemaShape::Struct { fields, repr_c } = &shape.schema else {
            panic!("expected Struct");
        };
        assert!(*repr_c);
        assert_eq!(fields.len(), 1);
    }

    #[test]
    fn canonical_kind_bytes_runtime_matches_const() {
        const NAME: &str = "test.triangle";
        const N: usize = canonical_len_kind(NAME, &TRIANGLE);
        const CONST_BYTES: [u8; N] = canonical_serialize_kind::<N>(NAME, &TRIANGLE);
        let runtime_bytes = canonical_kind_bytes(NAME, &TRIANGLE);
        assert_eq!(&CONST_BYTES[..], runtime_bytes.as_slice());
    }

    #[test]
    fn kind_id_from_parts_matches_hash_of_const_bytes() {
        const NAME: &str = "test.triangle";
        const N: usize = canonical_len_kind(NAME, &TRIANGLE);
        const BYTES: [u8; N] = canonical_serialize_kind::<N>(NAME, &TRIANGLE);
        // Domain-prefixed (issue #186) + ADR-0064 tag-stamped — agrees
        // with the derive macro's compile-time emission.
        let expected =
            ((TAG_KIND as u64) << TAG_SHIFT) | (fnv1a_64_prefixed(KIND_DOMAIN, &BYTES) & HASH_MASK);
        assert_eq!(kind_id_from_parts(NAME, &TRIANGLE), expected);
    }

    #[test]
    fn canonical_schema_two_equal_shapes_produce_equal_bytes() {
        // Two schemas with identical wire shape but different field
        // names must produce identical canonical bytes. This pins the
        // structural-not-nominal hashing invariant from ADR-0032.
        static V1: SchemaType = SchemaType::Struct {
            fields: Cow::Borrowed(&[
                NamedField {
                    name: Cow::Borrowed("x"),
                    ty: SchemaType::Scalar(Primitive::F32),
                },
                NamedField {
                    name: Cow::Borrowed("y"),
                    ty: SchemaType::Scalar(Primitive::F32),
                },
            ]),
            repr_c: true,
        };
        static V2: SchemaType = SchemaType::Struct {
            fields: Cow::Borrowed(&[
                NamedField {
                    name: Cow::Borrowed("row"),
                    ty: SchemaType::Scalar(Primitive::F32),
                },
                NamedField {
                    name: Cow::Borrowed("col"),
                    ty: SchemaType::Scalar(Primitive::F32),
                },
            ]),
            repr_c: true,
        };
        const N1: usize = canonical_len_schema(&V1);
        const N2: usize = canonical_len_schema(&V2);
        const B1: [u8; N1] = canonical_serialize_schema::<N1>(&V1);
        const B2: [u8; N2] = canonical_serialize_schema::<N2>(&V2);
        assert_eq!(&B1[..], &B2[..]);
    }

    // ADR-0045 typed handle reference. The new `SchemaType::Ref`
    // variant gets wire tag 10 (after `SCHEMA_ENUM = 9`); these
    // tests pin both the canonical bytes and the `SchemaShape`
    // round-trip so a future re-numbering of the variants can't
    // silently shift bytes underneath shipped components.

    static REF_F32: SchemaType = SchemaType::Ref(SchemaCell::Static(&F32));

    #[test]
    fn canonical_schema_ref_round_trips_as_shape() {
        const N: usize = canonical_len_schema(&REF_F32);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&REF_F32);
        // Tag 10 (SCHEMA_REF) followed by the inner schema's tag
        // (Scalar = 2, F32 = 8).
        assert_eq!(BYTES[0], 10);
        let shape: SchemaShape = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(
            shape,
            SchemaShape::Ref(Box::new(SchemaShape::Scalar(Primitive::F32)))
        );
    }

    #[test]
    fn canonical_schema_ref_differs_from_inline_kind() {
        // A struct field flipping from `K` to `Ref<K>` MUST change
        // the canonical bytes — that's how kind ids stay distinct
        // for the inline-shaped and ref-shaped variants of an
        // otherwise-equal kind. Pinned so a future bug that drops
        // the SCHEMA_REF tag from the encoding would surface.
        const INLINE_LEN: usize = canonical_len_schema(&F32);
        const REF_LEN: usize = canonical_len_schema(&REF_F32);
        const INLINE_BYTES: [u8; INLINE_LEN] = canonical_serialize_schema::<INLINE_LEN>(&F32);
        const REF_BYTES: [u8; REF_LEN] = canonical_serialize_schema::<REF_LEN>(&REF_F32);
        assert_ne!(&INLINE_BYTES[..], &REF_BYTES[..]);
        assert_eq!(REF_BYTES.len(), INLINE_BYTES.len() + 1);
    }

    // Labels tests — these exercise the full `KindLabels` round-trip.

    static VERTEX_LABELS: LabelNode = LabelNode::Struct {
        type_label: Some(Cow::Borrowed("my_crate::Vertex")),
        field_names: Cow::Borrowed(&[Cow::Borrowed("x"), Cow::Borrowed("y")]),
        fields: Cow::Borrowed(&[LabelNode::Anonymous, LabelNode::Anonymous]),
    };

    static TRIANGLE_LABELS: KindLabels = KindLabels {
        kind_id: aether_id::KindId(0),
        kind_label: Cow::Borrowed("my_crate::Triangle"),
        root: LabelNode::Struct {
            type_label: Some(Cow::Borrowed("my_crate::Triangle")),
            field_names: Cow::Borrowed(&[Cow::Borrowed("verts")]),
            fields: Cow::Borrowed(&[LabelNode::Array(LabelCell::Static(&VERTEX_LABELS))]),
        },
    };

    #[test]
    fn canonical_labels_round_trip_via_postcard() {
        const N: usize = canonical_len_labels(&TRIANGLE_LABELS);
        const BYTES: [u8; N] = canonical_serialize_labels::<N>(&TRIANGLE_LABELS);
        let decoded: KindLabels = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(decoded, TRIANGLE_LABELS);
    }

    static RESULT_LABELS: KindLabels = KindLabels {
        kind_id: aether_id::KindId(0),
        kind_label: Cow::Borrowed("my_crate::Result"),
        root: LabelNode::Enum {
            type_label: Some(Cow::Borrowed("my_crate::Result")),
            variants: Cow::Borrowed(&[
                VariantLabel::Unit {
                    name: Cow::Borrowed("Pending"),
                },
                VariantLabel::Tuple {
                    name: Cow::Borrowed("Ok"),
                    fields: Cow::Borrowed(&[LabelNode::Anonymous]),
                },
                VariantLabel::Struct {
                    name: Cow::Borrowed("Err"),
                    field_names: Cow::Borrowed(&[Cow::Borrowed("reason")]),
                    fields: Cow::Borrowed(&[LabelNode::Anonymous]),
                },
            ]),
        },
    };

    #[test]
    fn canonical_labels_enum_round_trips() {
        const N: usize = canonical_len_labels(&RESULT_LABELS);
        const BYTES: [u8; N] = canonical_serialize_labels::<N>(&RESULT_LABELS);
        let decoded: KindLabels = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(decoded, RESULT_LABELS);
    }

    // ADR-0045: `LabelNode::Ref` mirrors `SchemaType::Ref` in the
    // labels sidecar tree. The label's own variant tag is 6 (after
    // Enum = 5); the inner cell carries the wrapped kind's labels
    // verbatim. Hub walks both trees in lockstep, so a missing tag
    // on either side breaks `describe_kinds`.

    static REF_VERTEX_LABELS: KindLabels = KindLabels {
        kind_id: aether_id::KindId(0),
        kind_label: Cow::Borrowed("my_crate::HeldVertex"),
        root: LabelNode::Ref(LabelCell::Static(&VERTEX_LABELS)),
    };

    #[test]
    fn canonical_labels_ref_round_trips() {
        const N: usize = canonical_len_labels(&REF_VERTEX_LABELS);
        const BYTES: [u8; N] = canonical_serialize_labels::<N>(&REF_VERTEX_LABELS);
        let decoded: KindLabels = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(decoded, REF_VERTEX_LABELS);
    }

    // ADR-0033: handler/fallback/component record encoders. Round-trip
    // through `postcard::from_bytes::<InputsRecord>` so the substrate
    // reader sees exactly the enum shapes the macro emits.
    use crate::types::InputsRecord;

    #[test]
    fn inputs_handler_const_round_trips() {
        const ID: u64 = 0xdead_beef_cafe_f00d;
        const NAME: &str = "aether.tick";
        const DOC: Option<&str> = Some("Not useful to send manually.");
        const N: usize = inputs_handler_len(ID, NAME, DOC);
        const BYTES: [u8; N] = write_inputs_handler::<N>(ID, NAME, DOC);
        let decoded: InputsRecord = postcard::from_bytes(&BYTES).expect("decode");
        match decoded {
            InputsRecord::Handler { id, name, doc } => {
                assert_eq!(id, aether_id::KindId(ID));
                assert_eq!(name, NAME);
                assert_eq!(doc.as_deref(), DOC);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn inputs_handler_without_doc_const_round_trips() {
        const ID: u64 = 1;
        const NAME: &str = "test.ping";
        const DOC: Option<&str> = None;
        const N: usize = inputs_handler_len(ID, NAME, DOC);
        const BYTES: [u8; N] = write_inputs_handler::<N>(ID, NAME, DOC);
        let decoded: InputsRecord = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(
            decoded,
            InputsRecord::Handler {
                id: aether_id::KindId(ID),
                name: NAME.into(),
                doc: None,
            }
        );
    }

    #[test]
    fn inputs_fallback_const_round_trips() {
        const DOC: Option<&str> = Some("Forwards anything unrecognized.");
        const N: usize = inputs_fallback_len(DOC);
        const BYTES: [u8; N] = write_inputs_fallback::<N>(DOC);
        let decoded: InputsRecord = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(
            decoded,
            InputsRecord::Fallback {
                doc: Some(DOC.unwrap().into()),
            }
        );
    }

    #[test]
    fn inputs_component_const_round_trips() {
        const DOC: &str = "Logs every input event to the broadcast sink.";
        const N: usize = inputs_component_len(DOC);
        const BYTES: [u8; N] = write_inputs_component::<N>(DOC);
        let decoded: InputsRecord = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(decoded, InputsRecord::Component { doc: DOC.into() });
    }

    #[test]
    fn varint_u64_matches_postcard_encoding() {
        // Spot-check the new u64 varint against postcard's own encoder.
        // `varint_u64_len` / `write_varint_u64` must agree on every
        // boundary — the macro relies on it for handler ids that are
        // full 64-bit FNV hashes.
        for &v in &[0u64, 1, 0x7f, 0x80, 0xff, 0xffff_ffff, u64::MAX] {
            let mut out = [0u8; 10];
            let used = write_varint_u64(v, &mut out, 0);
            let postcard_bytes = postcard::to_allocvec(&v).unwrap();
            assert_eq!(&out[..used], &postcard_bytes[..], "mismatch for {v:#x}");
            assert_eq!(used, varint_u64_len(v), "len mismatch for {v:#x}");
        }
    }
}
