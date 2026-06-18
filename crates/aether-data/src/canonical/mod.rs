//! Const-fn canonical serializer for `SchemaType`, `KindLabels`, and
//! `InputsRecord` (ADR-0032, ADR-0033). Produces ADR-0118 aether-wire
//! bytes at const-eval time so they can be embedded directly in
//! `#[link_section]` statics and hashed to derive `Kind::ID`.
//!
//! The canonical schema bytes are the bare aether-wire body of
//! `SchemaShape` byte-for-byte: the only difference from the wire body
//! of `SchemaType` is that `NamedField.name` and `EnumVariant`'s `name`
//! field are omitted. Enum selector positions agree between `SchemaType`
//! and `SchemaShape` by construction (same arm order, same field
//! declaration order), so hub-side decode via
//! `wire::from_bytes::<SchemaShape>` reads the canonical bytes
//! cleanly.
//!
//! Only `SchemaCell::Static` / `LabelCell::Static` variants are
//! legal in const context here. Derive-emitted schemas always use
//! `Static`; passing an `Owned` cell (or an `Owned` `Cow`) to these
//! const fns is a compile-time panic. Runtime consumers (the hub)
//! decode the produced bytes back into `Owned` cells via
//! `wire::from_bytes`.
//!
//! Internal submodule layout — module-level re-exports preserve the
//! `canonical::*` surface so no downstream caller needs an edit:
//!   - `primitives`: shared const-fn aether-wire helpers (fixed-LE
//!     integers, str, option, cow-narrowing) the other submodules build on.
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
    inputs_actor_boundary_len, inputs_component_len, inputs_config_len, inputs_fallback_len,
    inputs_handler_len, reply_contract_len, write_inputs_actor_boundary, write_inputs_component,
    write_inputs_config, write_inputs_fallback, write_inputs_handler, write_reply_contract,
};
pub use labels::{canonical_len_labels, canonical_serialize_labels};
pub use schema::{
    canonical_kind_bytes, canonical_len_kind, canonical_len_schema, canonical_serialize_kind,
    canonical_serialize_schema, kind_id_from_parts, kind_id_from_shape,
};

#[cfg(test)]
mod tests {
    //! The contract these tests pin: canonical bytes round-trip through
    //! `wire::from_bytes::<SchemaShape>` / `wire::from_bytes::<KindLabels>`
    //! / `wire::from_bytes::<InputsRecord>`. That's what the hub
    //! relies on after reading the `aether.kinds` /
    //! `aether.kinds.labels` / `aether.kinds.inputs` custom sections.
    //! If these diverge, the hub can't decode what derives produce.
    //!
    //! The `*_const_matches_wire_runtime` tests are the load-bearing
    //! silent-corruption guard ADR-0118 §Consequences calls out: the
    //! const-fn writers must emit the exact bytes the serde-driven
    //! `wire::to_vec` runtime produces for the same value, or a
    //! mis-hashed `KindId` would mis-route mail.
    //!
    //! Each test constructs a schema via `static` so `SchemaCell::Static`
    //! is reachable in const context, runs both passes, and compares
    //! against a hand-built `SchemaShape` that matches the stripped shape.
    use super::*;
    use crate::hash::{KIND_DOMAIN, fnv1a_64_prefixed};
    use crate::ids::KindId;
    use crate::schema::{
        EnumVariant, KindLabels, KindShape, LabelCell, LabelNode, NamedField, Primitive,
        SchemaCell, SchemaShape, SchemaType, VariantLabel, VariantShape,
    };
    use crate::tag_bits::{HASH_MASK, TAG_KIND, TAG_SHIFT};
    use crate::wire;
    use alloc::borrow::Cow;
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;

    static F32: SchemaType = SchemaType::Scalar(Primitive::F32);

    /// `repr(C)` `{ x: f32, y: f32 }` — the canonical vertex fixture
    /// reused by struct, nested-array, and structural-equality tests.
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

    /// Runtime `SchemaShape` that `VERTEX`'s canonical bytes decode to.
    /// Used directly when a test asserts the struct shape, and nested
    /// inside larger shapes (e.g. `TRIANGLE`'s array element).
    fn vertex_shape() -> SchemaShape {
        SchemaShape::Struct {
            fields: vec![
                SchemaShape::Scalar(Primitive::F32),
                SchemaShape::Scalar(Primitive::F32),
            ],
            repr_c: true,
        }
    }

    /// One-line builders for the `Pending` / `Ok(u64)` / `Err{reason}`
    /// `VariantShape`s. Pulled out individually so each construction
    /// site reads as a named call rather than a multi-line struct
    /// literal — the literal shape was what Qodana fingerprinted as
    /// duplicate against `RESULT`'s parallel `EnumVariant` declaration.
    fn pending_shape() -> VariantShape {
        VariantShape::Unit { discriminant: 0 }
    }

    fn ok_u64_shape() -> VariantShape {
        VariantShape::Tuple {
            discriminant: 1,
            fields: vec![SchemaShape::Scalar(Primitive::U64)],
        }
    }

    fn err_reason_shape() -> VariantShape {
        VariantShape::Struct {
            discriminant: 2,
            fields: vec![SchemaShape::String],
        }
    }

    /// `VariantShape` list mirroring `RESULT`'s variant set
    /// (Pending / Ok(u64) / Err{reason}).
    fn result_variant_shapes() -> Vec<VariantShape> {
        vec![pending_shape(), ok_u64_shape(), err_reason_shape()]
    }

    /// Runtime `SchemaShape` that `RESULT`'s canonical bytes decode to —
    /// the three-variant Unit/Tuple/Struct enum exercised by the all-variants
    /// round-trip test.
    fn result_enum_shape() -> SchemaShape {
        SchemaShape::Enum {
            variants: result_variant_shapes(),
        }
    }

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
        let shape: SchemaShape = wire::from_bytes(&BYTES).expect("decode");
        assert_eq!(shape, SchemaShape::Scalar(Primitive::F32));
    }

    #[test]
    fn canonical_schema_struct_round_trips_as_shape() {
        const N: usize = canonical_len_schema(&VERTEX);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&VERTEX);
        let shape: SchemaShape = wire::from_bytes(&BYTES).expect("decode");
        assert_eq!(shape, vertex_shape());
    }

    #[test]
    fn canonical_schema_nested_array_of_struct_round_trips() {
        const N: usize = canonical_len_schema(&TRIANGLE);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&TRIANGLE);
        let shape: SchemaShape = wire::from_bytes(&BYTES).expect("decode");
        let expected = SchemaShape::Struct {
            fields: vec![SchemaShape::Array {
                element: Box::new(vertex_shape()),
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
        let shape: SchemaShape = wire::from_bytes(&BYTES).expect("decode");
        assert_eq!(shape, result_enum_shape());
    }

    #[test]
    fn canonical_kind_round_trips_as_kindshape() {
        const NAME: &str = "test.triangle";
        const N: usize = canonical_len_kind(NAME, &TRIANGLE);
        const BYTES: [u8; N] = canonical_serialize_kind::<N>(NAME, &TRIANGLE);
        let shape: KindShape = wire::from_bytes(&BYTES).expect("decode");
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
        let expected = (u64::from(TAG_KIND) << TAG_SHIFT)
            | (fnv1a_64_prefixed(KIND_DOMAIN, &BYTES) & HASH_MASK);
        assert_eq!(kind_id_from_parts(NAME, &TRIANGLE), expected);
    }

    #[test]
    fn canonical_schema_two_equal_shapes_produce_equal_bytes() {
        // Two schemas with identical wire shape but different field
        // names must produce identical canonical bytes. This pins the
        // structural-not-nominal hashing invariant from ADR-0032.
        // `VERTEX` (fields `x`, `y`) pairs against a sibling with
        // `row`, `col` — same shape, different names.
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
        const N1: usize = canonical_len_schema(&VERTEX);
        const N2: usize = canonical_len_schema(&V2);
        const B1: [u8; N1] = canonical_serialize_schema::<N1>(&VERTEX);
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
        // Selector 10 (SCHEMA_REF) as a `u32` LE — its low byte is at
        // BYTES[0] — followed by the inner Scalar(F32) shape.
        assert_eq!(BYTES[0], 10);
        let shape: SchemaShape = wire::from_bytes(&BYTES).expect("decode");
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
        // The `Ref` wrapper prepends one 4-byte `u32` LE selector.
        assert_eq!(REF_BYTES.len(), INLINE_BYTES.len() + 4);
    }

    // Labels tests — these exercise the full `KindLabels` round-trip.

    static VERTEX_LABELS: LabelNode = LabelNode::Struct {
        type_label: Some(Cow::Borrowed("my_crate::Vertex")),
        field_names: Cow::Borrowed(&[Cow::Borrowed("x"), Cow::Borrowed("y")]),
        fields: Cow::Borrowed(&[LabelNode::Anonymous, LabelNode::Anonymous]),
    };

    static TRIANGLE_LABELS: KindLabels = KindLabels {
        kind_id: KindId(0),
        kind_label: Cow::Borrowed("my_crate::Triangle"),
        root: LabelNode::Struct {
            type_label: Some(Cow::Borrowed("my_crate::Triangle")),
            field_names: Cow::Borrowed(&[Cow::Borrowed("verts")]),
            fields: Cow::Borrowed(&[LabelNode::Array(LabelCell::Static(&VERTEX_LABELS))]),
        },
    };

    #[test]
    fn canonical_labels_round_trip_via_wire() {
        const N: usize = canonical_len_labels(&TRIANGLE_LABELS);
        const BYTES: [u8; N] = canonical_serialize_labels::<N>(&TRIANGLE_LABELS);
        let decoded: KindLabels = wire::from_bytes(&BYTES).expect("decode");
        assert_eq!(decoded, TRIANGLE_LABELS);
    }

    #[test]
    fn canonical_labels_const_matches_wire_runtime() {
        // The const-fn labels writer must emit byte-for-byte what the
        // serde-driven `wire::to_vec(KindLabels)` runtime produces —
        // struct + nested array (TRIANGLE) and enum (RESULT) shapes.
        const TN: usize = canonical_len_labels(&TRIANGLE_LABELS);
        const TRIANGLE_CONST: [u8; TN] = canonical_serialize_labels::<TN>(&TRIANGLE_LABELS);
        const RN: usize = canonical_len_labels(&RESULT_LABELS);
        const RESULT_CONST: [u8; RN] = canonical_serialize_labels::<RN>(&RESULT_LABELS);

        let triangle_runtime = wire::to_vec(&TRIANGLE_LABELS).expect("encode");
        assert_eq!(&TRIANGLE_CONST[..], triangle_runtime.as_slice());
        let result_runtime = wire::to_vec(&RESULT_LABELS).expect("encode");
        assert_eq!(&RESULT_CONST[..], result_runtime.as_slice());
    }

    static RESULT_LABELS: KindLabels = KindLabels {
        kind_id: KindId(0),
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
        let decoded: KindLabels = wire::from_bytes(&BYTES).expect("decode");
        assert_eq!(decoded, RESULT_LABELS);
    }

    // ADR-0045: `LabelNode::Ref` mirrors `SchemaType::Ref` in the
    // labels sidecar tree. The label's own variant tag is 6 (after
    // Enum = 5); the inner cell carries the wrapped kind's labels
    // verbatim. Hub walks both trees in lockstep, so a missing tag
    // on either side breaks `describe_kinds`.

    static REF_VERTEX_LABELS: KindLabels = KindLabels {
        kind_id: KindId(0),
        kind_label: Cow::Borrowed("my_crate::HeldVertex"),
        root: LabelNode::Ref(LabelCell::Static(&VERTEX_LABELS)),
    };

    #[test]
    fn canonical_labels_ref_round_trips() {
        const N: usize = canonical_len_labels(&REF_VERTEX_LABELS);
        const BYTES: [u8; N] = canonical_serialize_labels::<N>(&REF_VERTEX_LABELS);
        let decoded: KindLabels = wire::from_bytes(&BYTES).expect("decode");
        assert_eq!(decoded, REF_VERTEX_LABELS);
    }

    // ADR-0033: handler/fallback/component record encoders. Round-trip
    // through `wire::from_bytes::<InputsRecord>` so the substrate
    // reader sees exactly the enum shapes the macro emits.
    use crate::schema::InputsRecord;

    #[test]
    fn inputs_handler_const_round_trips() {
        use crate::schema::ReplyContract;
        const ID: u64 = 0xdead_beef_cafe_f00d;
        const NAME: &str = "aether.tick";
        const DOC: Option<&str> = Some("Not useful to send manually.");
        // ADR-0112: a single `-> R` handler carries `ReplyContract::One`
        // — `(tag = 1, id)`.
        const REPLY_TAG: u8 = 1;
        const REPLY_ID: u64 = 0x00c0_ffee_0bad_f00d;
        const N: usize = inputs_handler_len(ID, NAME, DOC, REPLY_TAG, REPLY_ID);
        const BYTES: [u8; N] = write_inputs_handler::<N>(ID, NAME, DOC, REPLY_TAG, REPLY_ID);
        let decoded: InputsRecord = wire::from_bytes(&BYTES).expect("decode");
        match decoded {
            InputsRecord::Handler {
                id,
                name,
                doc,
                reply,
            } => {
                assert_eq!(id, KindId(ID));
                assert_eq!(name, NAME);
                assert_eq!(doc.as_deref(), DOC);
                assert_eq!(reply, ReplyContract::One(KindId(REPLY_ID)));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn inputs_handler_without_doc_const_round_trips() {
        use crate::schema::ReplyContract;
        const ID: u64 = 1;
        const NAME: &str = "test.ping";
        const DOC: Option<&str> = None;
        // ADR-0112: a `-> ()` fire-and-forget handler is `ReplyContract::None`
        // — `(tag = 0, id = 0)`.
        const REPLY_TAG: u8 = 0;
        const REPLY_ID: u64 = 0;
        const N: usize = inputs_handler_len(ID, NAME, DOC, REPLY_TAG, REPLY_ID);
        const BYTES: [u8; N] = write_inputs_handler::<N>(ID, NAME, DOC, REPLY_TAG, REPLY_ID);
        let decoded: InputsRecord = wire::from_bytes(&BYTES).expect("decode");
        assert_eq!(
            decoded,
            InputsRecord::Handler {
                id: KindId(ID),
                name: NAME.into(),
                doc: None,
                reply: ReplyContract::None,
            }
        );
    }

    #[test]
    fn reply_contract_wire_roundtrip() {
        use crate::schema::ReplyContract;
        // ADR-0112 / ADR-0118: the const-fn `(tag, id)` encoder matches
        // `wire::to_vec(ReplyContract)` byte-for-byte. The selector is
        // a `u32` LE, so its low byte (buf[0]) is the variant index —
        // `None` = 0, `One` = 1, `Stream` = 2, `Manual` = 3.
        fn check(tag: u8, id: u64, expect: ReplyContract, expect_disc: u8) {
            // Reuse a fixed-cap scratch buffer; `reply_contract_len` <= 12.
            let len = reply_contract_len(tag, id);
            let mut buf = [0u8; 16];
            let written = write_reply_contract(tag, id, &mut buf, 0);
            assert_eq!(written, len, "cursor advance matches reported length");
            assert_eq!(
                buf[0], expect_disc,
                "selector's low byte is the variant index"
            );
            let from_wire = wire::to_vec(&expect).expect("encode");
            assert_eq!(&buf[..len], from_wire.as_slice(), "matches wire runtime");
            let decoded: ReplyContract = wire::from_bytes(&buf[..len]).expect("decode");
            assert_eq!(decoded, expect);
        }
        check(0, 0, ReplyContract::None, 0);
        check(1, 0xabcd, ReplyContract::One(KindId(0xabcd)), 1);
        check(2, 0x1234, ReplyContract::Stream(KindId(0x1234)), 2);
        check(3, 0, ReplyContract::Manual, 3);
    }

    #[test]
    fn inputs_fallback_const_round_trips() {
        const DOC: Option<&str> = Some("Forwards anything unrecognized.");
        const N: usize = inputs_fallback_len(DOC);
        const BYTES: [u8; N] = write_inputs_fallback::<N>(DOC);
        let decoded: InputsRecord = wire::from_bytes(&BYTES).expect("decode");
        assert_eq!(
            decoded,
            InputsRecord::Fallback {
                doc: Some(DOC.expect("test setup: DOC is Some by construction").into()),
            }
        );
    }

    #[test]
    fn inputs_component_const_round_trips() {
        const DOC: &str = "Logs every input event to the broadcast sink.";
        const N: usize = inputs_component_len(DOC);
        const BYTES: [u8; N] = write_inputs_component::<N>(DOC);
        let decoded: InputsRecord = wire::from_bytes(&BYTES).expect("decode");
        assert_eq!(decoded, InputsRecord::Component { doc: DOC.into() });
    }

    #[test]
    fn inputs_config_const_round_trips() {
        // ADR-0090 (issue 1257): the `Config` record's const-eval bytes
        // must decode to `InputsRecord::Config` byte-for-byte so the
        // substrate reader lifts the config kind id + name verbatim.
        const ID: u64 = 0x0123_4567_89ab_cdef;
        const NAME: &str = "aether.test_fixtures.probe_config";
        const N: usize = inputs_config_len(ID, NAME);
        const BYTES: [u8; N] = write_inputs_config::<N>(ID, NAME);
        let decoded: InputsRecord = wire::from_bytes(&BYTES).expect("decode");
        assert_eq!(
            decoded,
            InputsRecord::Config {
                id: KindId(ID),
                name: NAME.into(),
            }
        );
    }

    #[test]
    fn inputs_actor_boundary_const_round_trips() {
        // ADR-0096: `export!(A, B, …)` writes one `ActorBoundary` record
        // per exported type; the const-eval bytes must decode to
        // `InputsRecord::ActorBoundary` byte-for-byte so the substrate
        // reader groups the flat record stream back by namespace.
        const NS: &str = "ui.panel";
        const N: usize = inputs_actor_boundary_len(NS);
        const BYTES: [u8; N] = write_inputs_actor_boundary::<N>(NS);
        let decoded: InputsRecord = wire::from_bytes(&BYTES).expect("decode");
        assert_eq!(
            decoded,
            InputsRecord::ActorBoundary {
                namespace: NS.into()
            }
        );
    }

    #[test]
    fn inputs_handler_const_matches_wire_runtime() {
        use crate::schema::ReplyContract;
        use alloc::borrow::Cow;
        // The strongest inputs guard: the const-fn handler encoder must
        // produce byte-identical output to the serde-driven
        // `wire::to_vec` over the equivalent `InputsRecord::Handler`.
        // It exercises every wire primitive a record uses — `u32` selector,
        // bare `u64` id, length-prefixed name, option-presence doc, and the
        // nested `ReplyContract` enum.
        const ID: u64 = 0xdead_beef_cafe_f00d;
        const NAME: &str = "aether.tick";
        const DOC: Option<&str> = Some("Not useful to send manually.");
        const REPLY_TAG: u8 = 1;
        const REPLY_ID: u64 = 0x00c0_ffee_0bad_f00d;
        const N: usize = inputs_handler_len(ID, NAME, DOC, REPLY_TAG, REPLY_ID);
        const CONST_BYTES: [u8; N] = write_inputs_handler::<N>(ID, NAME, DOC, REPLY_TAG, REPLY_ID);
        let record = InputsRecord::Handler {
            id: KindId(ID),
            name: Cow::Borrowed(NAME),
            doc: DOC.map(Cow::Borrowed),
            reply: ReplyContract::One(KindId(REPLY_ID)),
        };
        let runtime = wire::to_vec(&record).expect("encode");
        assert_eq!(&CONST_BYTES[..], runtime.as_slice());
    }
}
