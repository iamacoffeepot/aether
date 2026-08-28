//! The borrowing walk that holds a client value to the admitted subset,
//! before `encode_schema` sees it.
//!
//! The walk is iterative and its stack holds *cursors*, not nodes: a sequence
//! frame remembers the slice and its position rather than pushing every item,
//! so stack depth equals nesting depth and never payload breadth. Nothing in
//! the payload is copied — segments borrow from the schema, and map-key
//! segments borrow from the value.
//!
//! The same function runs on the way out. After `decode_schema_strict`
//! projects a provider's output bytes to JSON, the decoded wrapper is scanned
//! against the same admitted subset. That is normally redundant with wire
//! decoding, and it is kept because it is not always: a known `TypeId` whose
//! reserved sentinel projects as a numeric compatibility value rather than
//! the advertised tagged string passes the decoder and fails here, which is
//! the correct answer for a value that must satisfy an advertised
//! `outputSchema`.

use aether_data::{EnumVariant, NamedField, Primitive, SchemaType};
use serde_json::{Map, Value, map::Iter as MapIter};
use std::fmt::Write as _;

use super::vocabulary::{
    MapKeyRule, ScalarRange, is_tagged_identifier, map_key_rule, scalar_range, tag_for_schema_type_id,
};
use super::{SchemaBudget, ValidationError};

/// The element schema of a `Bytes` leaf.
///
/// `Bytes` is an array of byte values at this boundary, and giving the walk a
/// borrowable element schema lets each element ride the ordinary sequence
/// cursor — so a long byte array is charged and path-reported like any other
/// array instead of being checked in one unbounded step.
static BYTE_ELEMENT: SchemaType = SchemaType::Scalar(Primitive::U8);

/// Check a borrowed JSON value against an admitted schema.
///
/// A `Unit` schema requires an explicit null here. The codec's unit arm
/// discards whatever it is given, which is the right compatibility behavior
/// for the wire and the wrong behavior for a boundary that told the client
/// the value is null.
pub fn validate_client_value(value: &Value, schema: &SchemaType, budget: SchemaBudget) -> Result<(), ValidationError> {
    let mut stack = vec![Frame { segment: Segment::Root, body: Body::Pending { value, schema } }];
    let mut nodes = 0usize;

    while !stack.is_empty() {
        if stack.len() > budget.maximum_depth {
            return Err(fault(&stack, format!("nesting deeper than {} levels", budget.maximum_depth)));
        }

        let action = match step(&mut stack, &mut nodes, budget) {
            Ok(action) => action,
            Err(reason) => return Err(fault(&stack, reason)),
        };

        apply(&mut stack, action);
    }

    Ok(())
}

/// Where the walk currently is, for rendering a path on failure.
#[derive(Clone, Copy)]
enum Segment<'a> {
    Root,
    Field(&'a str),
    Index(usize),
    Key(&'a str),
    Variant(&'a str),
}

struct Frame<'a> {
    segment: Segment<'a>,
    body: Body<'a>,
}

enum Body<'a> {
    /// A value that has not been examined yet.
    Pending { value: &'a Value, schema: &'a SchemaType },
    /// A homogeneous sequence, walked by position.
    Items { items: &'a [Value], element: &'a SchemaType, next: usize },
    /// A heterogeneous fixed sequence — a tuple variant's positional fields
    /// — where each position carries its own schema.
    Positions { items: &'a [Value], fields: &'a [SchemaType], next: usize },
    /// A struct's declared fields, walked in schema order.
    Fields { object: &'a Map<String, Value>, fields: &'a [NamedField], next: usize },
    /// A map's entries, walked in object order.
    Entries { entries: MapIter<'a>, value: &'a SchemaType },
    /// A frame whose work moved to a child; it pops on its next turn.
    Spent,
}

enum Action<'a> {
    /// This frame is finished.
    Pop,
    /// Continue on this frame with a different cursor, at the same path.
    Replace(Body<'a>),
    /// Visit a child; this frame has more turns after it.
    Push(Frame<'a>),
    /// Visit a child and finish; this frame's work is entirely the child's.
    Descend(Frame<'a>),
}

fn apply<'a>(stack: &mut Vec<Frame<'a>>, action: Action<'a>) {
    match action {
        Action::Pop => {
            stack.pop();
        }
        Action::Replace(body) => {
            if let Some(top) = stack.last_mut() {
                top.body = body;
            }
        }
        Action::Push(child) => stack.push(child),
        Action::Descend(child) => {
            if let Some(top) = stack.last_mut() {
                top.body = Body::Spent;
            }
            stack.push(child);
        }
    }
}

fn step<'a>(stack: &mut [Frame<'a>], nodes: &mut usize, budget: SchemaBudget) -> Result<Action<'a>, String> {
    let Some(frame) = stack.last_mut() else {
        return Ok(Action::Pop);
    };

    match &mut frame.body {
        Body::Spent => Ok(Action::Pop),
        Body::Pending { value, schema } => {
            *nodes += 1;
            if *nodes > budget.maximum_nodes {
                return Err(format!("more than {} values", budget.maximum_nodes));
            }
            classify(value, schema)
        }
        Body::Items { items, element, next } => {
            let (items, element) = (*items, *element);
            if *next >= items.len() {
                return Ok(Action::Pop);
            }
            let index = *next;
            *next += 1;
            Ok(Action::Push(Frame {
                segment: Segment::Index(index),
                body: Body::Pending { value: &items[index], schema: element },
            }))
        }
        Body::Positions { items, fields, next } => {
            let (items, fields) = (*items, *fields);
            if *next >= fields.len() {
                return Ok(Action::Pop);
            }
            let index = *next;
            *next += 1;
            Ok(Action::Push(Frame {
                segment: Segment::Index(index),
                body: Body::Pending { value: &items[index], schema: &fields[index] },
            }))
        }
        Body::Fields { object, fields, next } => {
            let (object, fields) = (*object, *fields);
            if *next >= fields.len() {
                return Ok(Action::Pop);
            }
            let field = &fields[*next];
            *next += 1;
            let name = field.name.as_ref();
            let value = object.get(name).ok_or_else(|| format!("missing field `{name}`"))?;
            Ok(Action::Push(Frame { segment: Segment::Field(name), body: Body::Pending { value, schema: &field.ty } }))
        }
        Body::Entries { entries, value } => {
            let value = *value;
            match entries.next() {
                None => Ok(Action::Pop),
                Some((key, entry)) => Ok(Action::Push(Frame {
                    segment: Segment::Key(key),
                    body: Body::Pending { value: entry, schema: value },
                })),
            }
        }
    }
}

/// Examine one value against one schema, either accepting it outright or
/// choosing the cursor that walks its children.
fn classify<'a>(value: &'a Value, schema: &'a SchemaType) -> Result<Action<'a>, String> {
    match schema {
        SchemaType::Unit => accept(value.is_null(), "null"),
        SchemaType::Bool => accept(value.is_boolean(), "a boolean"),
        SchemaType::String => accept(value.is_string(), "a string"),
        SchemaType::Scalar(primitive) => check_scalar(value, *primitive).map(|()| Action::Pop),
        SchemaType::TypeId(type_id) => {
            let tag = tag_for_schema_type_id(*type_id).map_err(|error| error.to_string())?;
            let candidate = value.as_str().unwrap_or_default();
            accept(is_tagged_identifier(candidate, tag), &format!("a `{}-` tagged identifier", tag.prefix()))
        }
        SchemaType::Bytes => {
            let items = expect_array(value, "an array of byte values")?;
            Ok(Action::Replace(Body::Items { items, element: &BYTE_ELEMENT, next: 0 }))
        }
        SchemaType::Option(inner) => {
            if value.is_null() {
                return Ok(Action::Pop);
            }
            Ok(Action::Replace(Body::Pending { value, schema: inner }))
        }
        SchemaType::Vec(inner) => {
            let items = expect_array(value, "an array")?;
            Ok(Action::Replace(Body::Items { items, element: inner, next: 0 }))
        }
        SchemaType::Array { element, len } => {
            let items = expect_array(value, "an array")?;
            let expected = *len as usize;
            if items.len() != expected {
                return Err(format!("expected exactly {expected} items, found {}", items.len()));
            }
            Ok(Action::Replace(Body::Items { items, element, next: 0 }))
        }
        SchemaType::Struct { fields, .. } => {
            let object = expect_object(value, "an object")?;
            if object.len() != fields.len() {
                return Err(describe_field_mismatch(object, fields));
            }
            Ok(Action::Replace(Body::Fields { object, fields, next: 0 }))
        }
        SchemaType::Map { key, value: entry } => {
            let object = expect_object(value, "an object")?;
            reject_unrenderable_keys(object, key)?;
            Ok(Action::Replace(Body::Entries { entries: object.iter(), value: entry }))
        }
        SchemaType::Enum { variants } => classify_enum(value, variants),
    }
}

/// The externally tagged enum shapes the codec reads: a bare string for a
/// fieldless variant, and a one-property object for every other.
fn classify_enum<'a>(value: &'a Value, variants: &'a [EnumVariant]) -> Result<Action<'a>, String> {
    if let Some(name) = value.as_str() {
        let fieldless =
            variants.iter().any(|variant| matches!(variant, EnumVariant::Unit { .. }) && variant.name() == name);
        return accept(fieldless, "the name of a fieldless variant");
    }

    let object = expect_object(value, "a variant name or a one-property variant object")?;
    let mut entries = object.iter();
    let Some((name, body)) = entries.next().filter(|_| object.len() == 1) else {
        return Err("expected exactly one variant property".to_string());
    };
    let Some(variant) = variants.iter().find(|variant| variant.name() == name) else {
        return Err(format!("unknown variant `{name}`"));
    };

    let segment = Segment::Variant(name.as_str());
    match variant {
        EnumVariant::Unit { .. } => Err(format!("variant `{name}` is fieldless; send its name as a string")),
        EnumVariant::Tuple { fields, .. } if fields.len() == 1 => {
            Ok(Action::Descend(Frame { segment, body: Body::Pending { value: body, schema: &fields[0] } }))
        }
        EnumVariant::Tuple { fields, .. } => {
            let items = expect_array(body, "an array of variant fields")?;
            if items.len() != fields.len() {
                return Err(format!("variant `{name}` expects exactly {} fields", fields.len()));
            }
            Ok(Action::Descend(Frame { segment, body: Body::Positions { items, fields, next: 0 } }))
        }
        EnumVariant::Struct { fields, .. } => {
            let object = expect_object(body, "an object of variant fields")?;
            if object.len() != fields.len() {
                return Err(describe_field_mismatch(object, fields));
            }
            Ok(Action::Descend(Frame { segment, body: Body::Fields { object, fields, next: 0 } }))
        }
    }
}

fn check_scalar(value: &Value, primitive: Primitive) -> Result<(), String> {
    let number = value.as_number().ok_or_else(|| "expected a number".to_string())?;

    match scalar_range(primitive) {
        ScalarRange::Integer { minimum, maximum } => {
            let integral = number
                .as_i64()
                .map(i128::from)
                .or_else(|| number.as_u64().map(i128::from))
                .ok_or_else(|| format!("expected an integer in {minimum}..={maximum}"))?;
            if integral < minimum || integral > maximum {
                return Err(format!("expected an integer in {minimum}..={maximum}"));
            }
            Ok(())
        }
        ScalarRange::Float { magnitude } => {
            // The finite-range check is the point of this arm: the codec
            // narrows an `f64` to `f32` with a plain cast, so a finite JSON
            // number above the `f32` range would silently become an infinity
            // the advertised schema never described.
            let float = number.as_f64().ok_or_else(|| "expected a number".to_string())?;
            if !float.is_finite() || float.abs() > magnitude {
                return Err(format!("expected a finite number of magnitude at most {magnitude:e}"));
            }
            Ok(())
        }
    }
}

/// Every key of a map object must render back through the codec's key
/// grammar. A schema whose key type was never admissible reaching this walk
/// is an internal inconsistency, and it is reported rather than skipped.
fn reject_unrenderable_keys(object: &Map<String, Value>, key: &SchemaType) -> Result<(), String> {
    match map_key_rule(key).map_err(|error| error.to_string())? {
        MapKeyRule::AnyString => Ok(()),
        MapKeyRule::Enumerated(admitted) => object
            .keys()
            .find(|key| !admitted.contains(&key.as_str()))
            .map_or(Ok(()), |unexpected| Err(format!("key `{unexpected}` is not one of {admitted:?}"))),
    }
}

/// Name the first field that makes an object's shape wrong.
///
/// Only reached once a length check has already failed, so the scan it costs
/// is on the error path alone.
fn describe_field_mismatch(object: &Map<String, Value>, fields: &[NamedField]) -> String {
    if let Some(missing) = fields.iter().find(|field| !object.contains_key(field.name.as_ref())) {
        return format!("missing field `{}`", missing.name);
    }
    object
        .keys()
        .find(|key| !fields.iter().any(|field| field.name.as_ref() == key.as_str()))
        .map_or_else(|| "unexpected object shape".to_string(), |unexpected| format!("unexpected field `{unexpected}`"))
}

fn accept<'a>(condition: bool, expected: &str) -> Result<Action<'a>, String> {
    if condition {
        Ok(Action::Pop)
    } else {
        Err(format!("expected {expected}"))
    }
}

fn expect_array<'a>(value: &'a Value, expected: &str) -> Result<&'a [Value], String> {
    value.as_array().map(Vec::as_slice).ok_or_else(|| format!("expected {expected}"))
}

fn expect_object<'a>(value: &'a Value, expected: &str) -> Result<&'a Map<String, Value>, String> {
    value.as_object().ok_or_else(|| format!("expected {expected}"))
}

fn fault(stack: &[Frame<'_>], reason: String) -> ValidationError {
    ValidationError { path: render_path(stack), reason }
}

fn render_path(stack: &[Frame<'_>]) -> String {
    let mut path = String::new();
    for frame in stack {
        match frame.segment {
            Segment::Root => path.push('$'),
            Segment::Field(name) | Segment::Variant(name) => {
                path.push('.');
                path.push_str(name);
            }
            Segment::Index(index) => {
                let _ = write!(path, "[{index}]");
            }
            Segment::Key(key) => {
                let _ = write!(path, "[{key:?}]");
            }
        }
    }
    path
}
