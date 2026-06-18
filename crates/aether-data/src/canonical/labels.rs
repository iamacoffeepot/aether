//! Canonical `KindLabels` sidecar serializer (ADR-0032). Produces
//! ADR-0118 aether-wire bytes for the `aether.kinds.labels` custom
//! section at const-eval time, matching the substrate/hub runtime
//! decode via `wire::from_bytes::<KindLabels>`.

use crate::schema::{KindLabels, LabelCell, LabelNode, VariantLabel};

use super::primitives::{
    U32_WIDTH, U64_WIDTH, cow_label_nodes, cow_str_as_str, cow_strs, cow_variant_labels,
    option_str_len, str_len, write_count, write_option_str, write_str, write_u32_le, write_u64_le,
};

const LABEL_ANONYMOUS: u8 = 0;
const LABEL_OPTION: u8 = 1;
const LABEL_VEC: u8 = 2;
const LABEL_ARRAY: u8 = 3;
const LABEL_STRUCT: u8 = 4;
const LABEL_ENUM: u8 = 5;
const LABEL_REF: u8 = 6;
const LABEL_MAP: u8 = 7;

const VARIANT_LABEL_UNIT: u8 = 0;
const VARIANT_LABEL_TUPLE: u8 = 1;
const VARIANT_LABEL_STRUCT: u8 = 2;

/// Byte length for `KindLabels` aether-wire encoding. `kind_id` is a
/// `KindId`, which wire-encodes as its bare `u64` (`U64_WIDTH` bytes).
#[must_use]
pub const fn canonical_len_labels(labels: &KindLabels) -> usize {
    U64_WIDTH + str_len(cow_str_as_str(&labels.kind_label)) + label_node_len(&labels.root)
}

const fn label_node_len(node: &LabelNode) -> usize {
    match node {
        LabelNode::Anonymous => U32_WIDTH,
        LabelNode::Option(cell)
        | LabelNode::Vec(cell)
        | LabelNode::Array(cell)
        | LabelNode::Ref(cell) => U32_WIDTH + label_cell_len(cell),
        LabelNode::Struct {
            type_label,
            field_names,
            fields,
        } => {
            let names = cow_strs(field_names);
            let fs = cow_label_nodes(fields);
            let mut total = U32_WIDTH + option_str_len(type_label);
            total += U32_WIDTH;
            let mut i = 0;
            while i < names.len() {
                total += str_len(cow_str_as_str(&names[i]));
                i += 1;
            }
            total += U32_WIDTH;
            let mut i = 0;
            while i < fs.len() {
                total += label_node_len(&fs[i]);
                i += 1;
            }
            total
        }
        LabelNode::Enum {
            type_label,
            variants,
        } => {
            let vs = cow_variant_labels(variants);
            let mut total = U32_WIDTH + option_str_len(type_label);
            total += U32_WIDTH;
            let mut i = 0;
            while i < vs.len() {
                total += variant_label_len(&vs[i]);
                i += 1;
            }
            total
        }
        LabelNode::Map { key, value } => U32_WIDTH + label_cell_len(key) + label_cell_len(value),
    }
}

const fn label_cell_len(cell: &LabelCell) -> usize {
    match cell {
        LabelCell::Static(r) => label_node_len(r),
        LabelCell::Owned(_) => {
            panic!("canonical labels: Owned LabelCell not supported in const context");
        }
    }
}

const fn variant_label_len(v: &VariantLabel) -> usize {
    match v {
        VariantLabel::Unit { name } => U32_WIDTH + str_len(cow_str_as_str(name)),
        VariantLabel::Tuple { name, fields } => {
            let fs = cow_label_nodes(fields);
            let mut total = U32_WIDTH + str_len(cow_str_as_str(name)) + U32_WIDTH;
            let mut i = 0;
            while i < fs.len() {
                total += label_node_len(&fs[i]);
                i += 1;
            }
            total
        }
        VariantLabel::Struct {
            name,
            field_names,
            fields,
        } => {
            let names = cow_strs(field_names);
            let fs = cow_label_nodes(fields);
            let mut total = U32_WIDTH + str_len(cow_str_as_str(name));
            total += U32_WIDTH;
            let mut i = 0;
            while i < names.len() {
                total += str_len(cow_str_as_str(&names[i]));
                i += 1;
            }
            total += U32_WIDTH;
            let mut i = 0;
            while i < fs.len() {
                total += label_node_len(&fs[i]);
                i += 1;
            }
            total
        }
    }
}

/// Serialize `labels` into `N` bytes of canonical aether-wire form.
///
/// # Panics
/// Panics if `N` does not match the byte length the size pass
/// (`canonical_serialize_labels_len`) reports for `labels` — fail-fast
/// per ADR-0063: callers pair the two passes via the same `const`
/// inputs, so a mismatch is a bug in the serializer or its caller.
#[must_use]
pub const fn canonical_serialize_labels<const N: usize>(labels: &KindLabels) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = write_u64_le(labels.kind_id.0, &mut out, 0);
    pos = write_str(cow_str_as_str(&labels.kind_label), &mut out, pos);
    pos = write_label_node(&labels.root, &mut out, pos);
    assert!(
        pos == N,
        "canonical_serialize_labels: size mismatch between len pass and serialize pass"
    );
    out
}

const fn write_label_node(node: &LabelNode, out: &mut [u8], cursor: usize) -> usize {
    let mut pos = cursor;
    match node {
        LabelNode::Anonymous => {
            pos = write_u32_le(LABEL_ANONYMOUS as u32, out, pos);
        }
        LabelNode::Option(cell) => {
            pos = write_u32_le(LABEL_OPTION as u32, out, pos);
            pos = write_label_cell(cell, out, pos);
        }
        LabelNode::Vec(cell) => {
            pos = write_u32_le(LABEL_VEC as u32, out, pos);
            pos = write_label_cell(cell, out, pos);
        }
        LabelNode::Array(cell) => {
            pos = write_u32_le(LABEL_ARRAY as u32, out, pos);
            pos = write_label_cell(cell, out, pos);
        }
        LabelNode::Struct {
            type_label,
            field_names,
            fields,
        } => {
            let names = cow_strs(field_names);
            let fs = cow_label_nodes(fields);
            pos = write_u32_le(LABEL_STRUCT as u32, out, pos);
            pos = write_option_str(type_label, out, pos);
            pos = write_count(names.len(), out, pos);
            let mut i = 0;
            while i < names.len() {
                pos = write_str(cow_str_as_str(&names[i]), out, pos);
                i += 1;
            }
            pos = write_count(fs.len(), out, pos);
            let mut i = 0;
            while i < fs.len() {
                pos = write_label_node(&fs[i], out, pos);
                i += 1;
            }
        }
        LabelNode::Enum {
            type_label,
            variants,
        } => {
            let vs = cow_variant_labels(variants);
            pos = write_u32_le(LABEL_ENUM as u32, out, pos);
            pos = write_option_str(type_label, out, pos);
            pos = write_count(vs.len(), out, pos);
            let mut i = 0;
            while i < vs.len() {
                pos = write_variant_label(&vs[i], out, pos);
                i += 1;
            }
        }
        LabelNode::Ref(cell) => {
            pos = write_u32_le(LABEL_REF as u32, out, pos);
            pos = write_label_cell(cell, out, pos);
        }
        LabelNode::Map { key, value } => {
            pos = write_u32_le(LABEL_MAP as u32, out, pos);
            pos = write_label_cell(key, out, pos);
            pos = write_label_cell(value, out, pos);
        }
    }
    pos
}

const fn write_label_cell(cell: &LabelCell, out: &mut [u8], cursor: usize) -> usize {
    match cell {
        LabelCell::Static(r) => write_label_node(r, out, cursor),
        LabelCell::Owned(_) => {
            panic!("canonical labels: Owned LabelCell not supported in const context");
        }
    }
}

const fn write_variant_label(v: &VariantLabel, out: &mut [u8], cursor: usize) -> usize {
    let mut pos = cursor;
    match v {
        VariantLabel::Unit { name } => {
            pos = write_u32_le(VARIANT_LABEL_UNIT as u32, out, pos);
            pos = write_str(cow_str_as_str(name), out, pos);
        }
        VariantLabel::Tuple { name, fields } => {
            let fs = cow_label_nodes(fields);
            pos = write_u32_le(VARIANT_LABEL_TUPLE as u32, out, pos);
            pos = write_str(cow_str_as_str(name), out, pos);
            pos = write_count(fs.len(), out, pos);
            let mut i = 0;
            while i < fs.len() {
                pos = write_label_node(&fs[i], out, pos);
                i += 1;
            }
        }
        VariantLabel::Struct {
            name,
            field_names,
            fields,
        } => {
            let names = cow_strs(field_names);
            let fs = cow_label_nodes(fields);
            pos = write_u32_le(VARIANT_LABEL_STRUCT as u32, out, pos);
            pos = write_str(cow_str_as_str(name), out, pos);
            pos = write_count(names.len(), out, pos);
            let mut i = 0;
            while i < names.len() {
                pos = write_str(cow_str_as_str(&names[i]), out, pos);
                i += 1;
            }
            pos = write_count(fs.len(), out, pos);
            let mut i = 0;
            while i < fs.len() {
                pos = write_label_node(&fs[i], out, pos);
                i += 1;
            }
        }
    }
    pos
}
