//! Bloomery-owned canonical rendering of a [`SchemaType`] (ADR-0187).
//!
//! A persisted kind's digest is a hash of this rendering, not of
//! `SchemaType`'s own wire encoding. A change to `SchemaType` in the data
//! crate — a new variant, a field on a named-field record — therefore moves
//! no existing pinned digest and strands no stored row.

use alloc::vec::Vec;
use core::fmt;

use aether_data::schema::{EnumVariant, NamedField, Primitive, SchemaType};

const TAG_UNIT: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_SCALAR: u8 = 2;
const TAG_STRING: u8 = 3;
const TAG_BYTES: u8 = 4;
const TAG_OPTION: u8 = 5;
const TAG_VEC: u8 = 6;
const TAG_ARRAY: u8 = 7;
const TAG_STRUCT: u8 = 8;
const TAG_ENUM: u8 = 9;
const TAG_MAP: u8 = 10;
const TAG_TYPE_ID: u8 = 11;

const VARIANT_UNIT: u8 = 0;
const VARIANT_TUPLE: u8 = 1;
const VARIANT_STRUCT: u8 = 2;

const NODE_BUDGET: usize = 100_000;
const DEPTH_BUDGET: usize = 256;

/// Why a schema could not be rendered into canonical bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderError {
    /// The walk visited more nodes than the rendering budget.
    NodeBudget,
    /// Nesting exceeded the rendering depth budget.
    DepthBudget,
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodeBudget => write!(f, "schema rendering exceeded the node budget"),
            Self::DepthBudget => write!(f, "schema rendering exceeded the depth budget"),
        }
    }
}

/// One pending node on the iterative walk. Container headers emit first; the
/// remaining work is pushed so the walk never recurses.
enum Frame<'a> {
    Schema { schema: &'a SchemaType, depth: usize },
    Field { name: &'a str, schema: &'a SchemaType, depth: usize },
    Variant { variant: &'a EnumVariant, depth: usize },
}

/// Render `schema` under `kind` into the bytes [`schema_digest`](super::schema_digest) hashes.
///
/// # Errors
///
/// [`RenderError::NodeBudget`] or [`RenderError::DepthBudget`] when the tree
/// exceeds the walk caps. Compiled-in kinds are well under both; a budget
/// error is a desynchronized walker, not a legitimate schema.
pub fn render_schema(kind: &str, schema: &SchemaType) -> Result<Vec<u8>, RenderError> {
    let mut out = Vec::new();
    push_str(&mut out, kind);
    let mut stack = vec![Frame::Schema { schema, depth: 0 }];
    let mut nodes = 0usize;
    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Schema { schema, depth } => {
                visit(&mut nodes, depth)?;
                emit_schema(&mut out, &mut stack, schema, depth)?;
            }
            Frame::Field { name, schema, depth } => {
                push_str(&mut out, name);
                stack.push(Frame::Schema { schema, depth });
            }
            Frame::Variant { variant, depth } => {
                emit_variant(&mut out, &mut stack, variant, depth)?;
            }
        }
    }
    Ok(out)
}

fn visit(nodes: &mut usize, depth: usize) -> Result<(), RenderError> {
    if depth > DEPTH_BUDGET {
        return Err(RenderError::DepthBudget);
    }
    *nodes = nodes.saturating_add(1);
    if *nodes > NODE_BUDGET {
        return Err(RenderError::NodeBudget);
    }
    Ok(())
}

fn emit_schema<'a>(
    out: &mut Vec<u8>,
    stack: &mut Vec<Frame<'a>>,
    schema: &'a SchemaType,
    depth: usize,
) -> Result<(), RenderError> {
    match schema {
        SchemaType::Unit => out.push(TAG_UNIT),
        SchemaType::Bool => out.push(TAG_BOOL),
        SchemaType::Scalar(primitive) => {
            out.push(TAG_SCALAR);
            out.push(primitive_tag(*primitive));
        }
        SchemaType::String => out.push(TAG_STRING),
        SchemaType::Bytes => out.push(TAG_BYTES),
        SchemaType::Option(inner) => {
            out.push(TAG_OPTION);
            stack.push(Frame::Schema { schema: inner, depth: depth.saturating_add(1) });
        }
        SchemaType::Vec(inner) => {
            out.push(TAG_VEC);
            stack.push(Frame::Schema { schema: inner, depth: depth.saturating_add(1) });
        }
        SchemaType::Array { element, len } => {
            out.push(TAG_ARRAY);
            push_u32(out, *len);
            stack.push(Frame::Schema { schema: element, depth: depth.saturating_add(1) });
        }
        SchemaType::Struct { fields, repr_c } => {
            out.push(TAG_STRUCT);
            out.push(u8::from(*repr_c));
            push_u32(out, u32_len(fields.len())?);
            push_fields(stack, fields, depth.saturating_add(1));
        }
        SchemaType::Enum { variants } => {
            out.push(TAG_ENUM);
            push_u32(out, u32_len(variants.len())?);
            push_variants(stack, variants, depth.saturating_add(1));
        }
        SchemaType::Map { key, value } => {
            out.push(TAG_MAP);
            let nested = depth.saturating_add(1);
            stack.push(Frame::Schema { schema: value, depth: nested });
            stack.push(Frame::Schema { schema: key, depth: nested });
        }
        SchemaType::TypeId(id) => {
            out.push(TAG_TYPE_ID);
            out.extend_from_slice(&id.to_le_bytes());
        }
    }
    Ok(())
}

fn emit_variant<'a>(
    out: &mut Vec<u8>,
    stack: &mut Vec<Frame<'a>>,
    variant: &'a EnumVariant,
    depth: usize,
) -> Result<(), RenderError> {
    match variant {
        EnumVariant::Unit { name, discriminant } => {
            out.push(VARIANT_UNIT);
            push_str(out, name);
            push_u32(out, *discriminant);
        }
        EnumVariant::Tuple { name, discriminant, fields } => {
            out.push(VARIANT_TUPLE);
            push_str(out, name);
            push_u32(out, *discriminant);
            push_u32(out, u32_len(fields.len())?);
            push_schemas(stack, fields, depth);
        }
        EnumVariant::Struct { name, discriminant, fields } => {
            out.push(VARIANT_STRUCT);
            push_str(out, name);
            push_u32(out, *discriminant);
            push_u32(out, u32_len(fields.len())?);
            push_fields(stack, fields, depth);
        }
    }
    Ok(())
}

fn push_fields<'a>(stack: &mut Vec<Frame<'a>>, fields: &'a [NamedField], depth: usize) {
    for field in fields.iter().rev() {
        stack.push(Frame::Field { name: &field.name, schema: &field.ty, depth });
    }
}

fn push_variants<'a>(stack: &mut Vec<Frame<'a>>, variants: &'a [EnumVariant], depth: usize) {
    for variant in variants.iter().rev() {
        stack.push(Frame::Variant { variant, depth });
    }
}

fn push_schemas<'a>(stack: &mut Vec<Frame<'a>>, schemas: &'a [SchemaType], depth: usize) {
    for schema in schemas.iter().rev() {
        stack.push(Frame::Schema { schema, depth });
    }
}

fn push_str(out: &mut Vec<u8>, value: &str) {
    push_u32(out, u32::try_from(value.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(value.as_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn u32_len(len: usize) -> Result<u32, RenderError> {
    u32::try_from(len).map_err(|_| RenderError::NodeBudget)
}

fn primitive_tag(primitive: Primitive) -> u8 {
    match primitive {
        Primitive::U8 => 0,
        Primitive::U16 => 1,
        Primitive::U32 => 2,
        Primitive::U64 => 3,
        Primitive::I8 => 4,
        Primitive::I16 => 5,
        Primitive::I32 => 6,
        Primitive::I64 => 7,
        Primitive::F32 => 8,
        Primitive::F64 => 9,
    }
}
