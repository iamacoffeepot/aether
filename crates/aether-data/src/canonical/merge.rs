//! Merge a positional [`SchemaShape`] with its parallel [`LabelNode`] back
//! into a named [`SchemaType`] (ADR-0032). The canonical bytes carry the
//! shape, the `Kind::ID` hash input, and the labels sidecar carries the
//! nominal half; every reader that holds both rebuilds the schema here: the
//! substrate's `aether.kinds` manifest reader and the bloomery program
//! record decoder.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

use crate::schema::{
    EnumVariant, LabelNode, NamedField, SchemaCell, SchemaShape, SchemaType, VariantLabel, VariantShape,
};

/// Cap on [`merge_schema`] recursion depth. The nesting it recurses over
/// comes from bytes a reader did not produce (a wasm custom section), so an
/// unbounded depth would let a hostile or malformed record overflow the
/// stack (CLAUDE.md's recursion-over-wire-data rule).
pub const MAX_MERGE_DEPTH: usize = 64;

/// Why [`merge_schema`] refused a shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeError {
    /// The shape nests deeper than [`MAX_MERGE_DEPTH`].
    TooDeep,
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooDeep => write!(f, "schema nesting exceeds depth cap {MAX_MERGE_DEPTH}"),
        }
    }
}

impl Error for MergeError {}

/// Merge `shape` with its parallel `labels` into a named [`SchemaType`].
/// `None` labels produce anonymous field, variant, and type names; the shape
/// drives every structural decision. A labels node whose arm disagrees with
/// the shape's (a `Struct` against an `Enum`) falls back to anonymous, since
/// the shape is what the canonical bytes, and so `K::ID`, agreed on.
///
/// # Errors
///
/// [`MergeError::TooDeep`] when the shape nests past [`MAX_MERGE_DEPTH`].
pub fn merge_schema(shape: &SchemaShape, labels: Option<&LabelNode>) -> Result<SchemaType, MergeError> {
    merge(shape, labels, 0)
}

fn check_merge_depth(depth: usize) -> Result<(), MergeError> {
    if depth > MAX_MERGE_DEPTH {
        return Err(MergeError::TooDeep);
    }
    Ok(())
}

fn merge(shape: &SchemaShape, label: Option<&LabelNode>, depth: usize) -> Result<SchemaType, MergeError> {
    check_merge_depth(depth)?;
    let schema = match shape {
        SchemaShape::Unit => SchemaType::Unit,
        SchemaShape::Bool => SchemaType::Bool,
        SchemaShape::Scalar(p) => SchemaType::Scalar(*p),
        SchemaShape::String => SchemaType::String,
        SchemaShape::Bytes => SchemaType::Bytes,
        SchemaShape::Blob => SchemaType::Blob,
        SchemaShape::Ticket { reply } => SchemaType::Ticket { reply: *reply },
        SchemaShape::Option(inner) => {
            let inner_label = match label {
                Some(LabelNode::Option(cell)) => Some(&**cell),
                _ => None,
            };
            SchemaType::Option(SchemaCell::owned(merge(inner, inner_label, depth + 1)?))
        }
        SchemaShape::Vec(inner) => {
            let inner_label = match label {
                Some(LabelNode::Vec(cell)) => Some(&**cell),
                _ => None,
            };
            SchemaType::Vec(SchemaCell::owned(merge(inner, inner_label, depth + 1)?))
        }
        SchemaShape::Array { element, len } => {
            let element_label = match label {
                Some(LabelNode::Array(cell)) => Some(&**cell),
                _ => None,
            };
            SchemaType::Array { element: SchemaCell::owned(merge(element, element_label, depth + 1)?), len: *len }
        }
        SchemaShape::Struct { fields, repr_c } => {
            let (field_names, field_labels) = match label {
                Some(LabelNode::Struct { field_names, fields: field_labels, .. }) => {
                    (Some(&**field_names), Some(&**field_labels))
                }
                _ => (None, None),
            };
            let named_fields = fields
                .iter()
                .enumerate()
                .map(|(idx, ft)| {
                    let name = field_name(field_names, idx);
                    let field_label = field_labels.and_then(|labels| labels.get(idx));
                    Ok(NamedField { name, ty: merge(ft, field_label, depth + 1)? })
                })
                .collect::<Result<Vec<_>, MergeError>>()?;
            SchemaType::Struct { fields: Cow::Owned(named_fields), repr_c: *repr_c }
        }
        SchemaShape::Enum { variants } => {
            let variant_labels = match label {
                Some(LabelNode::Enum { variants: vs, .. }) => Some(&**vs),
                _ => None,
            };
            let merged = variants
                .iter()
                .enumerate()
                .map(|(idx, v)| merge_variant(v, variant_labels.and_then(|vs| vs.get(idx)), depth + 1))
                .collect::<Result<Vec<_>, MergeError>>()?;
            SchemaType::Enum { variants: Cow::Owned(merged) }
        }
        SchemaShape::Map { key, value } => {
            // Issue #232: parallel-walk the labels Map arm so any nominal info
            // inside key/value types (struct field names etc.) survives the
            // shape→type rebuild. Mismatched labels (or no labels at all)
            // collapse to anonymous on each side independently — the schema
            // arm always wins.
            let (key_label, value_label) = match label {
                Some(LabelNode::Map { key: kc, value: vc }) => (Some(&**kc), Some(&**vc)),
                _ => (None, None),
            };
            SchemaType::Map {
                key: SchemaCell::owned(merge(key, key_label, depth + 1)?),
                value: SchemaCell::owned(merge(value, value_label, depth + 1)?),
            }
        }
        SchemaShape::TypeId(id) => SchemaType::TypeId(*id),
    };
    Ok(schema)
}

/// The label at `idx`, or an anonymous (empty) name when the labels carry
/// none there.
fn field_name(names: Option<&[Cow<'static, str>]>, idx: usize) -> Cow<'static, str> {
    names.and_then(|names| names.get(idx)).cloned().unwrap_or_else(|| Cow::Owned(String::new()))
}

fn merge_variant(shape: &VariantShape, label: Option<&VariantLabel>, depth: usize) -> Result<EnumVariant, MergeError> {
    let variant = match shape {
        VariantShape::Unit { discriminant } => {
            let name = match label {
                Some(VariantLabel::Unit { name }) => name.clone(),
                _ => Cow::Owned(String::new()),
            };
            EnumVariant::Unit { name, discriminant: *discriminant }
        }
        VariantShape::Tuple { discriminant, fields } => {
            let (name, field_labels) = match label {
                Some(VariantLabel::Tuple { name, fields: fl }) => (name.clone(), Some(&**fl)),
                _ => (Cow::Owned(String::new()), None),
            };
            let merged = fields
                .iter()
                .enumerate()
                .map(|(idx, ft)| merge(ft, field_labels.and_then(|fl| fl.get(idx)), depth + 1))
                .collect::<Result<Vec<_>, MergeError>>()?;
            EnumVariant::Tuple { name, discriminant: *discriminant, fields: Cow::Owned(merged) }
        }
        VariantShape::Struct { discriminant, fields } => {
            let (name, field_names, field_labels) = match label {
                Some(VariantLabel::Struct { name, field_names: fn_, fields: fl }) => {
                    (name.clone(), Some(&**fn_), Some(&**fl))
                }
                _ => (Cow::Owned(String::new()), None, None),
            };
            let named = fields
                .iter()
                .enumerate()
                .map(|(idx, ft)| {
                    Ok(NamedField {
                        name: field_name(field_names, idx),
                        ty: merge(ft, field_labels.and_then(|fl| fl.get(idx)), depth + 1)?,
                    })
                })
                .collect::<Result<Vec<_>, MergeError>>()?;
            EnumVariant::Struct { name, discriminant: *discriminant, fields: Cow::Owned(named) }
        }
    };
    Ok(variant)
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use super::{MAX_MERGE_DEPTH, MergeError, merge_schema};
    use crate::schema::{SchemaShape, SchemaType};

    /// A `SchemaShape::Option` chain `depth` levels deep around a `Unit` leaf.
    fn nested_option_shape(depth: usize) -> SchemaShape {
        let mut shape = SchemaShape::Unit;
        for _ in 0..depth {
            shape = SchemaShape::Option(Box::new(shape));
        }
        shape
    }

    #[test]
    fn merge_errors_past_max_merge_depth() {
        // Tripwire: the merge's own depth cap must fire before native
        // recursion over an attacker-controlled nesting depth overflows
        // the stack (CLAUDE.md's recursion-over-wire-data rule).
        assert_eq!(merge_schema(&nested_option_shape(MAX_MERGE_DEPTH + 2), None), Err(MergeError::TooDeep));
    }

    #[test]
    fn merge_succeeds_within_max_merge_depth() {
        let merged = merge_schema(&nested_option_shape(4), None).expect("a shallow shape merges");
        let mut schema = &merged;
        for _ in 0..4 {
            let SchemaType::Option(inner) = schema else {
                panic!("expected Option");
            };
            schema = &**inner;
        }
        assert_eq!(schema, &SchemaType::Unit);
    }
}
