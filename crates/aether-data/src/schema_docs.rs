//! Doc trees: the `///` docs of a type's fields and variants, carried beside
//! its schema.
//!
//! [`DocNode`] mirrors [`SchemaType`] the way [`crate::LabelNode`] does, and
//! like the labels it never enters the canonical schema bytes a kind id is
//! hashed from, so editing a comment never changes a kind id.
//! `#[derive(Schema)]`, `#[derive(Storage)]`, and `#[kind]` emit a type's tree
//! as `Schema::DOC_NODE`; a hand-written `Schema` impl keeps the
//! [`DocNode::Opaque`] default.
//!
//! [`require_documented`] is the const check a program input passes: every
//! named field and every variant reachable from it carries a doc, so a model
//! offered the program as a tool is told what each parameter means.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::ops::Deref;

use serde::ser::Error as SerError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Schema;
use crate::schema::{EnumVariant, NamedField, SchemaCell, SchemaType};

/// How deep [`require_documented`] walks before it refuses the type as too
/// deeply nested. Type nesting bounds the walk; the cap turns a pathological
/// nesting into a compile error instead of exhausting const evaluation.
pub const MAX_DOC_DEPTH: usize = 128;

/// One field's or variant's doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Doc {
    /// The joined `///` text.
    Written(Cow<'static, str>),
    /// No doc was written. Holds the compile-error message the derive
    /// pre-wrote, naming the field or variant, because a const panic cannot
    /// format one.
    Missing(&'static str),
}

/// The doc of one field of a struct or struct-shaped variant, or of one
/// positional field of a tuple variant, beside the doc tree of its type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDoc {
    /// The field's own doc. A tuple variant's positional field is written
    /// empty: it has no description of its own.
    pub doc: Doc,
    /// The doc tree of the field's type.
    pub node: DocCell,
    /// The compile-error message when `node` is [`DocNode::Opaque`] over a
    /// struct or enum schema, naming this field. Empty after a decode.
    pub opaque: &'static str,
}

/// The doc of one enum variant, beside the docs of its fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantDoc {
    /// The variant's own doc.
    pub doc: Doc,
    /// One entry per field: named for a struct variant, positional for a
    /// tuple variant, none for a unit variant.
    pub fields: Cow<'static, [FieldDoc]>,
}

/// The parallel doc tree of a [`SchemaType`]. Arms mirror the schema so a
/// walker steps both in lockstep; `Box<T>` and validated newtypes share
/// their inner type's tree, as they share its schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocNode {
    /// A leaf: scalar, `bool`, `String`, bytes, typed id, or a type whose
    /// schema holds no struct or enum.
    Leaf,
    /// A hand-written `Schema` that supplies no tree. Fine over a leaf
    /// schema; refused by [`require_documented`] over a struct or enum.
    /// Encoded as [`DocNode::Leaf`], so a decoded tree never holds it.
    Opaque,
    Option(DocCell),
    Vec(DocCell),
    Array(DocCell),
    Map {
        key: DocCell,
        value: DocCell,
    },
    Struct {
        fields: Cow<'static, [FieldDoc]>,
    },
    Enum {
        variants: Cow<'static, [VariantDoc]>,
    },
}

/// Recursion-breaking cell for a nested [`DocNode`], twin of
/// [`crate::LabelCell`]: `Static` for the derive's const literals, `Owned`
/// for a decoded tree.
#[derive(Debug)]
pub enum DocCell {
    Static(&'static DocNode),
    Owned(Box<DocNode>),
}

impl DocCell {
    #[must_use]
    pub fn owned(node: DocNode) -> Self {
        Self::Owned(Box::new(node))
    }
}

impl Deref for DocCell {
    type Target = DocNode;
    fn deref(&self) -> &DocNode {
        match self {
            Self::Static(r) => r,
            Self::Owned(b) => b,
        }
    }
}

impl Clone for DocCell {
    fn clone(&self) -> Self {
        Self::Owned(Box::new((**self).clone()))
    }
}

impl PartialEq for DocCell {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for DocCell {}

/// `'static` borrows of `T`'s schema and doc tree, for a const check over a
/// type named only through a generic parameter or a trait projection, where
/// a borrowed temporary would need a destructor at compile time.
pub struct StaticSchema<T: ?Sized>(PhantomData<T>);

impl<T: Schema + ?Sized + 'static> StaticSchema<T> {
    /// `&T::SCHEMA`.
    pub const SCHEMA: &'static SchemaType = &T::SCHEMA;
    /// `&T::DOC_NODE`.
    pub const DOC_NODE: &'static DocNode = &T::DOC_NODE;
}

/// Refuse, at compile time, a type that cannot be described as a tool's
/// parameters: its schema must be a struct, and every named field and every
/// variant reachable from it must carry a `///` doc.
///
/// The walk goes into `Option`, `Vec`, `[T; N]`, and `BTreeMap` keys and
/// values, and into the types of tuple-variant fields. It stops at leaves.
///
/// # Panics
///
/// With `root` when `schema` is not a struct or `docs` supplies no tree; with
/// the derive's message naming the first undocumented field or variant; with
/// a field's opaque message when a hand-written `Schema` exposes a struct or
/// enum without a doc tree; and when the nesting passes [`MAX_DOC_DEPTH`].
pub const fn require_documented(schema: &SchemaType, docs: &DocNode, root: &'static str) {
    assert!(matches!(schema, SchemaType::Struct { .. }) && matches!(docs, DocNode::Struct { .. }), "{}", root);
    check_node(schema, docs, root, 0);
}

const fn check_node(schema: &SchemaType, docs: &DocNode, opaque: &'static str, depth: usize) {
    assert!(depth <= MAX_DOC_DEPTH, "a program input nests deeper than MAX_DOC_DEPTH");
    match (schema, docs) {
        (SchemaType::Option(inner), DocNode::Option(cell))
        | (SchemaType::Vec(inner), DocNode::Vec(cell))
        | (SchemaType::Array { element: inner, .. }, DocNode::Array(cell)) => {
            check_node(schema_cell(inner), doc_cell(cell), opaque, depth + 1);
        }
        (SchemaType::Map { key, value }, DocNode::Map { key: key_docs, value: value_docs }) => {
            check_node(schema_cell(key), doc_cell(key_docs), opaque, depth + 1);
            check_node(schema_cell(value), doc_cell(value_docs), opaque, depth + 1);
        }
        (SchemaType::Struct { fields, .. }, DocNode::Struct { fields: field_docs }) => {
            check_named_fields(named_fields(fields), doc_fields(field_docs), opaque, depth);
        }
        (SchemaType::Enum { variants }, DocNode::Enum { variants: variant_docs }) => {
            let variants = enum_variants(variants);
            let variant_docs = doc_variants(variant_docs);
            assert!(variants.len() == variant_docs.len(), "{}", opaque);
            let mut index = 0;
            while index < variants.len() {
                let variant_doc = &variant_docs[index];
                require_doc(&variant_doc.doc);
                let field_docs = doc_fields(&variant_doc.fields);
                match &variants[index] {
                    EnumVariant::Unit { .. } => {}
                    EnumVariant::Tuple { fields, .. } => {
                        let fields = schema_types(fields);
                        assert!(fields.len() == field_docs.len(), "{}", opaque);
                        let mut field = 0;
                        while field < fields.len() {
                            let field_doc = &field_docs[field];
                            check_node(&fields[field], doc_cell(&field_doc.node), field_doc.opaque, depth + 1);
                            field += 1;
                        }
                    }
                    EnumVariant::Struct { fields, .. } => {
                        check_named_fields(named_fields(fields), field_docs, opaque, depth);
                    }
                }
                index += 1;
            }
        }
        // A doc tree that does not mirror the schema supplies nothing, which
        // is fine only while the schema reaches no struct or enum.
        _ => {
            assert!(!reaches_documented_shape(schema, depth), "{}", opaque);
        }
    }
}

const fn check_named_fields(fields: &[NamedField], field_docs: &[FieldDoc], opaque: &'static str, depth: usize) {
    assert!(fields.len() == field_docs.len(), "{}", opaque);
    let mut index = 0;
    while index < fields.len() {
        let field_doc = &field_docs[index];
        require_doc(&field_doc.doc);
        check_node(&fields[index].ty, doc_cell(&field_doc.node), field_doc.opaque, depth + 1);
        index += 1;
    }
}

const fn require_doc(doc: &Doc) {
    if let Doc::Missing(message) = doc {
        panic!("{}", *message);
    }
}

/// Whether `schema` holds a struct or enum, whose fields or variants a
/// rendered schema would describe, anywhere under its containers.
const fn reaches_documented_shape(schema: &SchemaType, depth: usize) -> bool {
    assert!(depth <= MAX_DOC_DEPTH, "a program input nests deeper than MAX_DOC_DEPTH");
    match schema {
        SchemaType::Struct { .. } | SchemaType::Enum { .. } => true,
        SchemaType::Option(inner) | SchemaType::Vec(inner) | SchemaType::Array { element: inner, .. } => {
            reaches_documented_shape(schema_cell(inner), depth + 1)
        }
        SchemaType::Map { key, value } => {
            reaches_documented_shape(schema_cell(key), depth + 1)
                || reaches_documented_shape(schema_cell(value), depth + 1)
        }
        _ => false,
    }
}

// `Deref` is not const, so each `Cow` / cell is narrowed by hand. Only the
// derive's `Borrowed` / `Static` forms are reachable at compile time.

const fn schema_cell(cell: &SchemaCell) -> &SchemaType {
    match cell {
        SchemaCell::Static(r) => r,
        SchemaCell::Owned(_) => panic!("require_documented: an Owned SchemaCell is not supported in const"),
    }
}

const fn doc_cell(cell: &DocCell) -> &DocNode {
    match cell {
        DocCell::Static(r) => r,
        DocCell::Owned(_) => panic!("require_documented: an Owned DocCell is not supported in const"),
    }
}

#[allow(clippy::ptr_arg)]
const fn named_fields<'a>(fields: &'a Cow<'static, [NamedField]>) -> &'a [NamedField] {
    match fields {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("require_documented: an Owned field list is not supported in const"),
    }
}

#[allow(clippy::ptr_arg)]
const fn enum_variants<'a>(variants: &'a Cow<'static, [EnumVariant]>) -> &'a [EnumVariant] {
    match variants {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("require_documented: an Owned variant list is not supported in const"),
    }
}

#[allow(clippy::ptr_arg)]
const fn schema_types<'a>(types: &'a Cow<'static, [SchemaType]>) -> &'a [SchemaType] {
    match types {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("require_documented: an Owned tuple field list is not supported in const"),
    }
}

#[allow(clippy::ptr_arg)]
pub(crate) const fn doc_fields<'a>(fields: &'a Cow<'static, [FieldDoc]>) -> &'a [FieldDoc] {
    match fields {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("doc tree: an Owned field doc list is not supported in const"),
    }
}

#[allow(clippy::ptr_arg)]
pub(crate) const fn doc_variants<'a>(variants: &'a Cow<'static, [VariantDoc]>) -> &'a [VariantDoc] {
    match variants {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("doc tree: an Owned variant doc list is not supported in const"),
    }
}

// The serde form is the one `canonical::docs` writes at compile time: a doc is
// its text, `Opaque` is written as `Leaf`, and a field's opaque message is
// not carried.

impl Serialize for Doc {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Written(text) => serializer.serialize_str(text),
            Self::Missing(message) => Err(SerError::custom(message)),
        }
    }
}

impl<'de> Deserialize<'de> for Doc {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|text| Self::Written(Cow::Owned(text)))
    }
}

impl Serialize for DocCell {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (**self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DocCell {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        DocNode::deserialize(deserializer).map(Self::owned)
    }
}

impl Serialize for FieldDoc {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("FieldDoc", 2)?;
        s.serialize_field("doc", &self.doc)?;
        s.serialize_field("node", &self.node)?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for FieldDoc {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename = "FieldDoc")]
        struct FieldDocDe {
            doc: Doc,
            node: DocCell,
        }
        let FieldDocDe { doc, node } = FieldDocDe::deserialize(deserializer)?;
        Ok(Self { doc, node, opaque: "" })
    }
}

impl Serialize for VariantDoc {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("VariantDoc", 2)?;
        s.serialize_field("doc", &self.doc)?;
        s.serialize_field("fields", &*self.fields)?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for VariantDoc {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename = "VariantDoc")]
        struct VariantDocDe {
            doc: Doc,
            fields: Vec<FieldDoc>,
        }
        let VariantDocDe { doc, fields } = VariantDocDe::deserialize(deserializer)?;
        Ok(Self { doc, fields: Cow::Owned(fields) })
    }
}

impl Serialize for DocNode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeStructVariant, SerializeTupleVariant};
        match self {
            Self::Leaf | Self::Opaque => serializer.serialize_unit_variant("DocNode", 0, "Leaf"),
            Self::Option(cell) => {
                let mut s = serializer.serialize_tuple_variant("DocNode", 1, "Option", 1)?;
                s.serialize_field(cell)?;
                s.end()
            }
            Self::Vec(cell) => {
                let mut s = serializer.serialize_tuple_variant("DocNode", 2, "Vec", 1)?;
                s.serialize_field(cell)?;
                s.end()
            }
            Self::Array(cell) => {
                let mut s = serializer.serialize_tuple_variant("DocNode", 3, "Array", 1)?;
                s.serialize_field(cell)?;
                s.end()
            }
            Self::Map { key, value } => {
                let mut s = serializer.serialize_struct_variant("DocNode", 4, "Map", 2)?;
                s.serialize_field("key", key)?;
                s.serialize_field("value", value)?;
                s.end()
            }
            Self::Struct { fields } => {
                let mut s = serializer.serialize_struct_variant("DocNode", 5, "Struct", 1)?;
                s.serialize_field("fields", &**fields)?;
                s.end()
            }
            Self::Enum { variants } => {
                let mut s = serializer.serialize_struct_variant("DocNode", 6, "Enum", 1)?;
                s.serialize_field("variants", &**variants)?;
                s.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for DocNode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename = "DocNode")]
        enum DocNodeDe {
            Leaf,
            Option(DocCell),
            Vec(DocCell),
            Array(DocCell),
            Map { key: DocCell, value: DocCell },
            Struct { fields: Vec<FieldDoc> },
            Enum { variants: Vec<VariantDoc> },
        }
        Ok(match DocNodeDe::deserialize(deserializer)? {
            DocNodeDe::Leaf => Self::Leaf,
            DocNodeDe::Option(cell) => Self::Option(cell),
            DocNodeDe::Vec(cell) => Self::Vec(cell),
            DocNodeDe::Array(cell) => Self::Array(cell),
            DocNodeDe::Map { key, value } => Self::Map { key, value },
            DocNodeDe::Struct { fields } => Self::Struct { fields: Cow::Owned(fields) },
            DocNodeDe::Enum { variants } => Self::Enum { variants: Cow::Owned(variants) },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{canonical_len_docs, canonical_serialize_docs};
    use crate::schema::Primitive;
    use crate::wire;

    static LEAF: DocNode = DocNode::Leaf;

    static INNER_SCHEMA: SchemaType = SchemaType::Struct {
        fields: Cow::Borrowed(&[NamedField { name: Cow::Borrowed("depth"), ty: SchemaType::Scalar(Primitive::U32) }]),
        repr_c: false,
    };
    static INNER_DOCUMENTED: DocNode = DocNode::Struct {
        fields: Cow::Borrowed(&[FieldDoc {
            doc: Doc::Written(Cow::Borrowed("How deep.")),
            node: DocCell::Static(&LEAF),
            opaque: "Inner.depth is opaque",
        }]),
    };
    static INNER_MISSING: DocNode = DocNode::Struct {
        fields: Cow::Borrowed(&[FieldDoc {
            doc: Doc::Missing("Inner.depth has no doc"),
            node: DocCell::Static(&LEAF),
            opaque: "Inner.depth is opaque",
        }]),
    };

    static MODE_SCHEMA: SchemaType = SchemaType::Enum {
        variants: Cow::Borrowed(&[
            EnumVariant::Unit { name: Cow::Borrowed("Fast"), discriminant: 0 },
            EnumVariant::Tuple {
                name: Cow::Borrowed("Nested"),
                discriminant: 1,
                fields: Cow::Borrowed(&[SchemaType::Vec(SchemaCell::Static(&INNER_SCHEMA))]),
            },
        ]),
    };

    /// A mode enum whose tuple variant holds `Vec<Inner>`, with `inner` the
    /// doc tree of `Inner`.
    macro_rules! mode_docs {
        ($inner:expr) => {
            DocNode::Enum {
                variants: Cow::Borrowed(&[
                    VariantDoc { doc: Doc::Written(Cow::Borrowed("Go fast.")), fields: Cow::Borrowed(&[]) },
                    VariantDoc {
                        doc: Doc::Written(Cow::Borrowed("Go nested.")),
                        fields: Cow::Borrowed(&[FieldDoc {
                            doc: Doc::Written(Cow::Borrowed("")),
                            node: DocCell::Static(&DocNode::Vec(DocCell::Static($inner))),
                            opaque: "Mode::Nested.0 is opaque",
                        }]),
                    },
                ]),
            }
        };
    }

    static MODE_DOCUMENTED: DocNode = mode_docs!(&INNER_DOCUMENTED);
    static MODE_MISSING: DocNode = mode_docs!(&INNER_MISSING);
    static MODE_OPAQUE: DocNode = mode_docs!(&DocNode::Opaque);

    static INPUT_SCHEMA: SchemaType = SchemaType::Struct {
        fields: Cow::Borrowed(&[
            NamedField { name: Cow::Borrowed("count"), ty: SchemaType::Scalar(Primitive::U32) },
            NamedField { name: Cow::Borrowed("mode"), ty: SchemaType::Option(SchemaCell::Static(&MODE_SCHEMA)) },
        ]),
        repr_c: false,
    };

    macro_rules! input_docs {
        ($mode:expr) => {
            DocNode::Struct {
                fields: Cow::Borrowed(&[
                    FieldDoc {
                        doc: Doc::Written(Cow::Borrowed("How many.")),
                        node: DocCell::Static(&LEAF),
                        opaque: "Input.count is opaque",
                    },
                    FieldDoc {
                        doc: Doc::Written(Cow::Borrowed("Which mode.")),
                        node: DocCell::Static(&DocNode::Option(DocCell::Static($mode))),
                        opaque: "Input.mode is opaque",
                    },
                ]),
            }
        };
    }

    static INPUT_DOCUMENTED: DocNode = input_docs!(&MODE_DOCUMENTED);
    static INPUT_MISSING: DocNode = input_docs!(&MODE_MISSING);
    static INPUT_OPAQUE: DocNode = input_docs!(&MODE_OPAQUE);

    #[test]
    fn const_serialized_docs_match_the_serde_form() {
        // Catches drift between the const serializer and the serde form the record decoder reads.
        const N: usize = canonical_len_docs(&INPUT_DOCUMENTED);
        const BYTES: [u8; N] = canonical_serialize_docs::<N>(&INPUT_DOCUMENTED);
        assert_eq!(&BYTES[..], wire::to_vec(&INPUT_DOCUMENTED).expect("encode").as_slice());

        let decoded: DocNode = wire::from_bytes(&BYTES).expect("docs decode");
        let DocNode::Struct { fields } = &decoded else {
            panic!("a struct tree, got {decoded:?}")
        };
        assert_eq!(fields[1].doc, Doc::Written(Cow::Borrowed("Which mode.")));
        let DocNode::Option(mode) = &*fields[1].node else {
            panic!("an option tree")
        };
        let DocNode::Enum { variants } = &**mode else {
            panic!("an enum tree")
        };
        assert_eq!(variants[1].doc, Doc::Written(Cow::Borrowed("Go nested.")));
        let DocNode::Vec(inner) = &*variants[1].fields[0].node else {
            panic!("a vec tree")
        };
        let DocNode::Struct { fields: inner } = &**inner else {
            panic!("a struct tree")
        };
        assert_eq!(inner[0].doc, Doc::Written(Cow::Borrowed("How deep.")));
    }

    #[test]
    fn a_documented_tree_passes() {
        // Catches a check that refuses a fully documented input, so no program would compile.
        const _: () = require_documented(&INPUT_SCHEMA, &INPUT_DOCUMENTED, "root");
    }

    #[test]
    #[should_panic = "Inner.depth has no doc"]
    fn the_check_walks_through_option_enum_tuple_fields_and_vec() {
        // Catches a walk that stops at a container: the missing doc sits under an Option, an enum's tuple
        // variant, and a Vec.
        require_documented(&INPUT_SCHEMA, &INPUT_MISSING, "root");
    }

    #[test]
    #[should_panic = "Mode::Nested.0 is opaque"]
    fn an_opaque_tree_over_a_struct_is_refused() {
        // Catches a hand-written Schema exposing undocumented fields through the Opaque default.
        require_documented(&INPUT_SCHEMA, &INPUT_OPAQUE, "root");
    }

    #[test]
    #[should_panic = "root"]
    fn a_non_struct_input_is_refused() {
        // Catches an input a responses-API `parameters` object cannot describe.
        require_documented(&MODE_SCHEMA, &MODE_DOCUMENTED, "root");
    }
}
