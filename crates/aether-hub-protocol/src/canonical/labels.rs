//! Canonical `KindLabels` sidecar serializer (ADR-0032). Produces
//! postcard-compatible bytes for the `aether.kinds.labels` custom
//! section at const-eval time, matching the substrate/hub runtime
//! decode via `postcard::from_bytes::<KindLabels>`.

use crate::types::{KindLabels, LabelCell, LabelNode, VariantLabel};

use super::primitives::{
    cow_label_nodes, cow_str_as_str, cow_strs, cow_variant_labels, option_str_len, str_len,
    varint_usize_len, write_option_str, write_str, write_varint_usize,
};

const LABEL_ANONYMOUS: u8 = 0;
const LABEL_OPTION: u8 = 1;
const LABEL_VEC: u8 = 2;
const LABEL_ARRAY: u8 = 3;
const LABEL_STRUCT: u8 = 4;
const LABEL_ENUM: u8 = 5;

const VARIANT_LABEL_UNIT: u8 = 0;
const VARIANT_LABEL_TUPLE: u8 = 1;
const VARIANT_LABEL_STRUCT: u8 = 2;

/// Byte length for `KindLabels` postcard encoding.
pub const fn canonical_len_labels(labels: &KindLabels) -> usize {
    str_len(cow_str_as_str(&labels.kind_label)) + label_node_len(&labels.root)
}

const fn label_node_len(node: &LabelNode) -> usize {
    match node {
        LabelNode::Anonymous => 1,
        LabelNode::Option(cell) => 1 + label_cell_len(cell),
        LabelNode::Vec(cell) => 1 + label_cell_len(cell),
        LabelNode::Array(cell) => 1 + label_cell_len(cell),
        LabelNode::Struct {
            type_label,
            field_names,
            fields,
        } => {
            let names = cow_strs(field_names);
            let fs = cow_label_nodes(fields);
            let mut total = 1 + option_str_len(type_label);
            total += varint_usize_len(names.len());
            let mut i = 0;
            while i < names.len() {
                total += str_len(cow_str_as_str(&names[i]));
                i += 1;
            }
            total += varint_usize_len(fs.len());
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
            let mut total = 1 + option_str_len(type_label);
            total += varint_usize_len(vs.len());
            let mut i = 0;
            while i < vs.len() {
                total += variant_label_len(&vs[i]);
                i += 1;
            }
            total
        }
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
        VariantLabel::Unit { name } => 1 + str_len(cow_str_as_str(name)),
        VariantLabel::Tuple { name, fields } => {
            let fs = cow_label_nodes(fields);
            let mut total = 1 + str_len(cow_str_as_str(name)) + varint_usize_len(fs.len());
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
            let mut total = 1 + str_len(cow_str_as_str(name));
            total += varint_usize_len(names.len());
            let mut i = 0;
            while i < names.len() {
                total += str_len(cow_str_as_str(&names[i]));
                i += 1;
            }
            total += varint_usize_len(fs.len());
            let mut i = 0;
            while i < fs.len() {
                total += label_node_len(&fs[i]);
                i += 1;
            }
            total
        }
    }
}

/// Serialize `labels` into `N` bytes of canonical postcard form.
pub const fn canonical_serialize_labels<const N: usize>(labels: &KindLabels) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = write_str(cow_str_as_str(&labels.kind_label), &mut out, 0);
    pos = write_label_node(&labels.root, &mut out, pos);
    if pos != N {
        panic!("canonical_serialize_labels: size mismatch between len pass and serialize pass");
    }
    out
}

const fn write_label_node(node: &LabelNode, out: &mut [u8], cursor: usize) -> usize {
    let mut pos = cursor;
    match node {
        LabelNode::Anonymous => {
            out[pos] = LABEL_ANONYMOUS;
            pos += 1;
        }
        LabelNode::Option(cell) => {
            out[pos] = LABEL_OPTION;
            pos += 1;
            pos = write_label_cell(cell, out, pos);
        }
        LabelNode::Vec(cell) => {
            out[pos] = LABEL_VEC;
            pos += 1;
            pos = write_label_cell(cell, out, pos);
        }
        LabelNode::Array(cell) => {
            out[pos] = LABEL_ARRAY;
            pos += 1;
            pos = write_label_cell(cell, out, pos);
        }
        LabelNode::Struct {
            type_label,
            field_names,
            fields,
        } => {
            let names = cow_strs(field_names);
            let fs = cow_label_nodes(fields);
            out[pos] = LABEL_STRUCT;
            pos += 1;
            pos = write_option_str(type_label, out, pos);
            pos = write_varint_usize(names.len(), out, pos);
            let mut i = 0;
            while i < names.len() {
                pos = write_str(cow_str_as_str(&names[i]), out, pos);
                i += 1;
            }
            pos = write_varint_usize(fs.len(), out, pos);
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
            out[pos] = LABEL_ENUM;
            pos += 1;
            pos = write_option_str(type_label, out, pos);
            pos = write_varint_usize(vs.len(), out, pos);
            let mut i = 0;
            while i < vs.len() {
                pos = write_variant_label(&vs[i], out, pos);
                i += 1;
            }
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
            out[pos] = VARIANT_LABEL_UNIT;
            pos += 1;
            pos = write_str(cow_str_as_str(name), out, pos);
        }
        VariantLabel::Tuple { name, fields } => {
            let fs = cow_label_nodes(fields);
            out[pos] = VARIANT_LABEL_TUPLE;
            pos += 1;
            pos = write_str(cow_str_as_str(name), out, pos);
            pos = write_varint_usize(fs.len(), out, pos);
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
            out[pos] = VARIANT_LABEL_STRUCT;
            pos += 1;
            pos = write_str(cow_str_as_str(name), out, pos);
            pos = write_varint_usize(names.len(), out, pos);
            let mut i = 0;
            while i < names.len() {
                pos = write_str(cow_str_as_str(&names[i]), out, pos);
                i += 1;
            }
            pos = write_varint_usize(fs.len(), out, pos);
            let mut i = 0;
            while i < fs.len() {
                pos = write_label_node(&fs[i], out, pos);
                i += 1;
            }
        }
    }
    pos
}
