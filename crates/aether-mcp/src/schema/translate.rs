//! `SchemaType` → JSON Schema 2020-12.
//!
//! The walk is iterative: a work stack of visits and assemblies over an
//! output stack of finished subschemas. A visit either pushes a leaf result
//! or pushes its assembly frame followed by its children in reverse, so the
//! children pop in source order and the frame pops after all of them. Depth
//! and node budgets are charged at each visit, before the node is expanded.

use aether_data::{EnumVariant, NamedField, Primitive, SchemaType, Tag};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

use super::vocabulary::{
    MapKeyRule, ScalarRange, map_key_rule, scalar_range, tag_for_schema_type_id, tagged_identifier_pattern,
};
use super::{SchemaBudget, SchemaError};

/// The dialect every tool schema root declares.
///
/// Stating it removes the ambiguity a client would otherwise have over tuple
/// validation, where pre-2020-12 drafts spell the same idea differently.
pub const JSON_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Translate a schema and shape it as a tool `inputSchema` or
/// `outputSchema`.
///
/// The 2025-06-18 tool contract requires an object-shaped root, so only
/// `Unit` and `Struct` are admissible there. A `Unit` root becomes the closed
/// empty object — the tool takes no arguments — rather than the `null` a
/// nested `Unit` becomes.
pub fn translate_tool_schema(schema: &SchemaType, budget: SchemaBudget) -> Result<Value, SchemaError> {
    let mut root = match schema {
        SchemaType::Unit => closed_object(Map::new(), Vec::new()),
        SchemaType::Struct { .. } => match translate(schema, budget)? {
            Value::Object(object) => object,
            _ => return Err(SchemaError::NonObjectRoot),
        },
        _ => return Err(SchemaError::NonObjectRoot),
    };

    root.insert("$schema".to_string(), Value::String(JSON_SCHEMA_DIALECT.to_string()));
    Ok(Value::Object(root))
}

/// Translate any admissible schema, without a root dialect declaration.
///
/// This is the general walk `translate_tool_schema` builds on, and the one to
/// reach for when translating a subschema in its own right.
pub fn translate(schema: &SchemaType, budget: SchemaBudget) -> Result<Value, SchemaError> {
    Translator { work: Vec::new(), output: Vec::new(), nodes: 0, budget }.run(schema)
}

/// One step of the walk.
enum Task<'a> {
    /// Examine a schema node and either emit its result or expand it.
    Visit { schema: &'a SchemaType, depth: usize },
    /// Combine already-emitted children into their parent's result.
    Assemble(Frame<'a>),
}

/// A parent waiting on its children, and what it needs to know to combine
/// them. `base` is the output-stack height when the frame was pushed, so the
/// frame's children are exactly everything above it.
enum Frame<'a> {
    Optional { base: usize },
    Sequence { base: usize },
    FixedSequence { base: usize, len: u32 },
    Object { base: usize, names: Vec<&'a str> },
    Mapping { base: usize, rule: MapKeyRule<'a> },
    Choice { base: usize, plans: Vec<VariantPlan<'a>> },
}

/// How one enum variant renders, and how many children it consumes.
struct VariantPlan<'a> {
    name: &'a str,
    shape: VariantShape<'a>,
}

enum VariantShape<'a> {
    /// A bare string equal to the variant name; consumes no child.
    Unit,
    /// A one-field tuple, whose single field schema is used directly.
    TupleField,
    /// A zero- or many-field tuple, rendered as a fixed-length array.
    TupleSequence(usize),
    /// A struct variant, rendered as a closed object.
    Fields(Vec<&'a str>),
}

struct Translator<'a> {
    work: Vec<Task<'a>>,
    output: Vec<Value>,
    nodes: usize,
    budget: SchemaBudget,
}

impl<'a> Translator<'a> {
    fn run(mut self, root: &'a SchemaType) -> Result<Value, SchemaError> {
        self.work.push(Task::Visit { schema: root, depth: 1 });

        while let Some(task) = self.work.pop() {
            match task {
                Task::Visit { schema, depth } => self.visit(schema, depth)?,
                Task::Assemble(frame) => {
                    let assembled = self.assemble(frame);
                    self.output.push(assembled);
                }
            }
        }

        Ok(self.output.pop().unwrap_or(Value::Null))
    }

    /// Charge one schema node against the budgets. Charging before expansion
    /// is what keeps an over-budget tree from allocating the node that
    /// crosses the line.
    fn charge(&mut self, depth: usize) -> Result<(), SchemaError> {
        self.nodes += 1;
        if self.nodes > self.budget.maximum_nodes {
            return Err(SchemaError::NodesExceeded { maximum: self.budget.maximum_nodes });
        }
        if depth > self.budget.maximum_depth {
            return Err(SchemaError::DepthExceeded { maximum: self.budget.maximum_depth });
        }
        Ok(())
    }

    fn visit(&mut self, schema: &'a SchemaType, depth: usize) -> Result<(), SchemaError> {
        self.charge(depth)?;

        match schema {
            SchemaType::Unit => self.output.push(json!({ "type": "null" })),
            SchemaType::Bool => self.output.push(json!({ "type": "boolean" })),
            SchemaType::Scalar(primitive) => self.output.push(scalar_schema(*primitive)),
            SchemaType::String => self.output.push(json!({ "type": "string" })),
            SchemaType::Bytes => self.output.push(byte_array_schema()),
            SchemaType::TypeId(type_id) => {
                self.output.push(tagged_identifier_schema(tag_for_schema_type_id(*type_id)?));
            }
            SchemaType::Option(inner) => self.expand(Frame::Optional { base: self.output.len() }, [&**inner], depth),
            SchemaType::Vec(inner) => self.expand(Frame::Sequence { base: self.output.len() }, [&**inner], depth),
            SchemaType::Array { element, len } => {
                self.expand(Frame::FixedSequence { base: self.output.len(), len: *len }, [&**element], depth);
            }
            SchemaType::Struct { fields, .. } => {
                let frame = Frame::Object { base: self.output.len(), names: distinct_field_names(fields)? };
                self.expand(frame, fields.iter().map(|field| &field.ty), depth);
            }
            SchemaType::Map { key, value } => {
                // The key is a schema node too, and its admitted shapes are
                // shallow, so it is charged but never expanded.
                self.charge(depth + 1)?;
                let frame = Frame::Mapping { base: self.output.len(), rule: map_key_rule(key)? };
                self.expand(frame, [&**value], depth);
            }
            SchemaType::Enum { variants } => self.visit_enum(variants, depth)?,
        }

        Ok(())
    }

    fn visit_enum(&mut self, variants: &'a [EnumVariant], depth: usize) -> Result<(), SchemaError> {
        reject_ambiguous_variants(variants)?;

        let mut plans = Vec::with_capacity(variants.len());
        let mut children: Vec<&'a SchemaType> = Vec::new();
        for variant in variants {
            let shape = match variant {
                EnumVariant::Unit { .. } => VariantShape::Unit,
                EnumVariant::Tuple { fields, .. } => {
                    children.extend(fields.iter());
                    if fields.len() == 1 {
                        VariantShape::TupleField
                    } else {
                        VariantShape::TupleSequence(fields.len())
                    }
                }
                EnumVariant::Struct { fields, .. } => {
                    let names = distinct_field_names(fields)?;
                    children.extend(fields.iter().map(|field| &field.ty));
                    VariantShape::Fields(names)
                }
            };
            plans.push(VariantPlan { name: variant.name(), shape });
        }

        self.expand(Frame::Choice { base: self.output.len(), plans }, children, depth);
        Ok(())
    }

    /// Schedule a parent's assembly and then its children, so the children
    /// pop in source order and the frame pops once they have all landed.
    fn expand<I>(&mut self, frame: Frame<'a>, children: I, depth: usize)
    where
        I: IntoIterator<Item = &'a SchemaType>,
        I::IntoIter: DoubleEndedIterator,
    {
        self.work.push(Task::Assemble(frame));
        for child in children.into_iter().rev() {
            self.work.push(Task::Visit { schema: child, depth: depth + 1 });
        }
    }

    fn assemble(&mut self, frame: Frame<'a>) -> Value {
        match frame {
            Frame::Optional { base } => {
                let inner = self.take_one(base);
                json!({ "anyOf": [inner, { "type": "null" }] })
            }
            Frame::Sequence { base } => {
                let element = self.take_one(base);
                json!({ "type": "array", "items": element })
            }
            Frame::FixedSequence { base, len } => {
                let element = self.take_one(base);
                json!({ "type": "array", "items": element, "minItems": len, "maxItems": len })
            }
            Frame::Object { base, names } => {
                let children = self.output.split_off(base);
                object_of(&names, children)
            }
            Frame::Mapping { base, rule } => {
                let value = self.take_one(base);
                let mut mapping = Map::new();
                mapping.insert("type".to_string(), json!("object"));
                mapping.insert("additionalProperties".to_string(), value);
                if let MapKeyRule::Enumerated(keys) = rule {
                    mapping.insert("propertyNames".to_string(), json!({ "enum": keys }));
                }
                Value::Object(mapping)
            }
            Frame::Choice { base, plans } => {
                let mut children = self.output.split_off(base).into_iter();
                if plans.is_empty() {
                    // An enum with no variants admits nothing, and the
                    // always-failing schema says exactly that.
                    return json!({ "not": {} });
                }
                let branches: Vec<Value> = plans.into_iter().map(|plan| variant_schema(&plan, &mut children)).collect();
                json!({ "oneOf": branches })
            }
        }
    }

    fn take_one(&mut self, base: usize) -> Value {
        self.output.split_off(base).into_iter().next().unwrap_or(Value::Null)
    }
}

/// The externally tagged rendering of one enum variant, matching the codec's
/// `encode_enum_body` and `decode_enum_body`.
fn variant_schema(plan: &VariantPlan<'_>, children: &mut impl Iterator<Item = Value>) -> Value {
    let body = match &plan.shape {
        VariantShape::Unit => return json!({ "type": "string", "const": plan.name }),
        VariantShape::TupleField => children.next().unwrap_or(Value::Null),
        VariantShape::TupleSequence(len) => {
            let items: Vec<Value> = children.take(*len).collect();
            json!({ "type": "array", "prefixItems": items, "items": false, "minItems": len, "maxItems": len })
        }
        VariantShape::Fields(names) => object_of(names, children.take(names.len()).collect()),
    };

    object_of(&[plan.name], vec![body])
}

/// A closed object whose properties are exactly `names`, all required.
///
/// Every field is required even when its schema is `Option`: the codec
/// requires each named field to be present and represents absence as an
/// explicit null, so a client that omits the member is sending something the
/// wire cannot carry.
fn object_of(names: &[&str], children: Vec<Value>) -> Value {
    let properties: Map<String, Value> = names.iter().map(|name| (*name).to_string()).zip(children).collect();
    Value::Object(closed_object(properties, names.iter().map(|name| json!(name)).collect()))
}

fn closed_object(properties: Map<String, Value>, required: Vec<Value>) -> Map<String, Value> {
    let mut object = Map::new();
    object.insert("type".to_string(), json!("object"));
    object.insert("properties".to_string(), Value::Object(properties));
    object.insert("required".to_string(), Value::Array(required));
    object.insert("additionalProperties".to_string(), json!(false));
    object
}

fn scalar_schema(primitive: Primitive) -> Value {
    match scalar_range(primitive) {
        ScalarRange::Integer { minimum, maximum } => {
            json!({ "type": "integer", "minimum": integer_bound(minimum), "maximum": integer_bound(maximum) })
        }
        ScalarRange::Float { magnitude } => {
            json!({ "type": "number", "minimum": -magnitude, "maximum": magnitude })
        }
    }
}

/// Render an exact integer bound.
///
/// Every bound in the primitive table fits `i64` or `u64`; the `Null` arm is
/// unreachable for any value that table can produce.
fn integer_bound(value: i128) -> Value {
    if let Ok(signed) = i64::try_from(value) {
        return json!(signed);
    }
    u64::try_from(value).map_or(Value::Null, |unsigned| json!(unsigned))
}

fn byte_array_schema() -> Value {
    json!({ "type": "array", "items": { "type": "integer", "minimum": 0, "maximum": 255 } })
}

fn tagged_identifier_schema(tag: Tag) -> Value {
    json!({ "type": "string", "pattern": tagged_identifier_pattern(tag) })
}

/// The field names of one struct, refusing a repeat.
///
/// A derived schema cannot produce a repeat, but `SchemaType` is public and
/// serializable, so registration defends against a hand-authored tree that
/// would make the JSON object ambiguous.
fn distinct_field_names(fields: &[NamedField]) -> Result<Vec<&str>, SchemaError> {
    let mut seen = BTreeSet::new();
    fields
        .iter()
        .map(|field| {
            let name = field.name.as_ref();
            if seen.insert(name) {
                Ok(name)
            } else {
                Err(SchemaError::DuplicateStructField { name: name.to_string() })
            }
        })
        .collect()
}

/// Refuse an enum whose variants collide by name or by discriminant.
///
/// A repeated name makes the externally tagged JSON ambiguous; a repeated
/// discriminant makes the wire decoding ambiguous. Both are unreachable from
/// the derive and both are reachable from a hand-authored tree.
fn reject_ambiguous_variants(variants: &[EnumVariant]) -> Result<(), SchemaError> {
    let mut names = BTreeSet::new();
    let mut discriminants = BTreeSet::new();
    for variant in variants {
        if !names.insert(variant.name()) {
            return Err(SchemaError::DuplicateEnumVariant { name: variant.name().to_string() });
        }
        if !discriminants.insert(variant.discriminant()) {
            return Err(SchemaError::DuplicateEnumDiscriminant { discriminant: variant.discriminant() });
        }
    }
    Ok(())
}
