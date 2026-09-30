//! Const-fn serializer for a [`DocNode`] tree. Produces ADR-0118 aether-wire
//! bytes at const-eval time, matching the runtime decode via
//! `wire::from_bytes::<DocNode>`: a doc is its text, [`DocNode::Opaque`] is
//! written as [`DocNode::Leaf`], and a field's opaque message is not carried.
//! A [`Doc::Missing`] panics with the message it holds, so a tree with an
//! undocumented field cannot be serialized.

use crate::schema_docs::{Doc, DocCell, DocNode, FieldDoc, VariantDoc, doc_fields, doc_variants};

use super::primitives::{U32_WIDTH, cow_str_as_str, str_len, write_count, write_str, write_u32_le};

const DOC_LEAF: u32 = 0;
const DOC_OPTION: u32 = 1;
const DOC_VEC: u32 = 2;
const DOC_ARRAY: u32 = 3;
const DOC_MAP: u32 = 4;
const DOC_STRUCT: u32 = 5;
const DOC_ENUM: u32 = 6;

/// Byte length of `node`'s aether-wire encoding.
///
/// # Panics
/// Panics with its message on a [`Doc::Missing`], or on an `Owned` cell or
/// `Cow`, which only a decoded tree holds.
#[must_use]
pub const fn canonical_len_docs(node: &DocNode) -> usize {
    match node {
        DocNode::Leaf | DocNode::Opaque => U32_WIDTH,
        DocNode::Option(cell) | DocNode::Vec(cell) | DocNode::Array(cell) => U32_WIDTH + cell_len(cell),
        DocNode::Map { key, value } => U32_WIDTH + cell_len(key) + cell_len(value),
        DocNode::Struct { fields } => U32_WIDTH + fields_len(doc_fields(fields)),
        DocNode::Enum { variants } => {
            let variants = doc_variants(variants);
            let mut total = U32_WIDTH + U32_WIDTH;
            let mut index = 0;
            while index < variants.len() {
                total += variant_len(&variants[index]);
                index += 1;
            }
            total
        }
    }
}

/// Serialize `node` into `N` bytes of aether-wire form.
///
/// # Panics
/// Panics when `N` is not [`canonical_len_docs`] for `node`, and as
/// [`canonical_len_docs`] does.
#[must_use]
pub const fn canonical_serialize_docs<const N: usize>(node: &DocNode) -> [u8; N] {
    let mut out = [0u8; N];
    let pos = canonical_write_docs(node, &mut out, 0);
    assert!(pos == N, "canonical_serialize_docs: size mismatch between len pass and serialize pass");
    out
}

/// Write `node`'s aether-wire bytes into `out` at `cursor`, returning the
/// advanced cursor: the form a record that embeds a doc tree beside other
/// fields writes it in.
///
/// # Panics
/// As [`canonical_len_docs`] does, and when `out` is too short.
pub const fn canonical_write_docs(node: &DocNode, out: &mut [u8], cursor: usize) -> usize {
    match node {
        DocNode::Leaf | DocNode::Opaque => write_u32_le(DOC_LEAF, out, cursor),
        DocNode::Option(cell) => {
            let pos = write_u32_le(DOC_OPTION, out, cursor);
            write_cell(cell, out, pos)
        }
        DocNode::Vec(cell) => {
            let pos = write_u32_le(DOC_VEC, out, cursor);
            write_cell(cell, out, pos)
        }
        DocNode::Array(cell) => {
            let pos = write_u32_le(DOC_ARRAY, out, cursor);
            write_cell(cell, out, pos)
        }
        DocNode::Map { key, value } => {
            let pos = write_u32_le(DOC_MAP, out, cursor);
            let pos = write_cell(key, out, pos);
            write_cell(value, out, pos)
        }
        DocNode::Struct { fields } => {
            let pos = write_u32_le(DOC_STRUCT, out, cursor);
            write_fields(doc_fields(fields), out, pos)
        }
        DocNode::Enum { variants } => {
            let variants = doc_variants(variants);
            let pos = write_u32_le(DOC_ENUM, out, cursor);
            let mut pos = write_count(variants.len(), out, pos);
            let mut index = 0;
            while index < variants.len() {
                let variant = &variants[index];
                pos = write_str(doc_text(&variant.doc), out, pos);
                pos = write_fields(doc_fields(&variant.fields), out, pos);
                index += 1;
            }
            pos
        }
    }
}

const fn doc_text(doc: &Doc) -> &str {
    match doc {
        Doc::Written(text) => cow_str_as_str(text),
        Doc::Missing(message) => panic!("{}", *message),
    }
}

const fn cell_node(cell: &DocCell) -> &DocNode {
    match cell {
        DocCell::Static(node) => node,
        DocCell::Owned(_) => panic!("canonical docs: Owned DocCell not supported in const context"),
    }
}

const fn cell_len(cell: &DocCell) -> usize {
    canonical_len_docs(cell_node(cell))
}

const fn write_cell(cell: &DocCell, out: &mut [u8], cursor: usize) -> usize {
    canonical_write_docs(cell_node(cell), out, cursor)
}

const fn fields_len(fields: &[FieldDoc]) -> usize {
    let mut total = U32_WIDTH;
    let mut index = 0;
    while index < fields.len() {
        total += str_len(doc_text(&fields[index].doc)) + cell_len(&fields[index].node);
        index += 1;
    }
    total
}

const fn write_fields(fields: &[FieldDoc], out: &mut [u8], cursor: usize) -> usize {
    let mut pos = write_count(fields.len(), out, cursor);
    let mut index = 0;
    while index < fields.len() {
        pos = write_str(doc_text(&fields[index].doc), out, pos);
        pos = write_cell(&fields[index].node, out, pos);
        index += 1;
    }
    pos
}

const fn variant_len(variant: &VariantDoc) -> usize {
    str_len(doc_text(&variant.doc)) + fields_len(doc_fields(&variant.fields))
}
