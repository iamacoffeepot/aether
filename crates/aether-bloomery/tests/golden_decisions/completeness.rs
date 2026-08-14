//! Structural completeness of the golden fixture beside it (#4960).
//!
//! The pinned bytes in `main.rs` freeze the shape of every position the
//! representative actually reaches, and nothing else. A family the
//! representative omits is free to gain a field, encode wider on every new row,
//! and still leave those bytes untouched — so the fixture's coverage is as
//! load-bearing as its bytes, and until now it was maintained by hand and
//! audited by whoever happened to read it.
//!
//! The property is decidable. The decisions graph describes itself through
//! `aether_data::Schema`, so the set of positions the wire reaches is derivable
//! from [`Decision::SCHEMA`], and the set the fixture reaches is derivable from
//! the fixture's own wire bytes read back through that same schema. This module
//! computes both and names the difference.
//!
//! Two boundaries are deliberate.
//!
//! The walk is rooted at [`Decision`], the graph under `Decisions::effects`.
//! `Decisions::outcome` is a single value, not a sequence, so no one fixture
//! value could reach every outcome — its coverage is a different question with
//! a different answer, and claiming it here would be a lie.
//!
//! A required position is one whose *shape* the pinned bytes would otherwise
//! not freeze: every named struct type, and every enum variant that carries a
//! payload. A unit variant carries no shape — it is a discriminant and nothing
//! else — so a fixture that omits one freezes no less of the graph than one
//! that includes it, while demanding all thirteen `StageId`s and every
//! `EvidenceKind` would inflate the fixture past reading for no added freeze.

use std::collections::BTreeSet;

use aether_bloomery::Decision;
use aether_codec::decode_schema;
use aether_data::Schema;
use aether_data::schema::{EnumVariant, LabelNode, SchemaType, VariantLabel};
use aether_data::wire::to_vec;
use serde_json::Value;

use super::representative;

/// The decisions graph is a few hundred positions deep in total. A walk that
/// exhausts this has desynchronized from the schema rather than found a big
/// graph, and says so instead of running away.
const WALK_BUDGET: usize = 100_000;

/// One pending position on the value walk: a decoded value paired with the
/// schema and label node that describe it.
type Step<'a> = (Value, &'a SchemaType, &'a LabelNode);

/// The trailing segment of a `Schema::LABEL` path — `Digest` out of
/// `aether_bloomery::digest::Digest`. The full path is the label's own value;
/// the leaf is what a reader recognizes in a failure message.
fn type_name(label: Option<&str>) -> String {
    label
        .expect("every struct and enum in the decisions graph carries a Schema LABEL")
        .rsplit("::")
        .next()
        .expect("a label path has at least one segment")
        .to_owned()
}

/// The label sitting under a container's cell, for the container arms whose
/// schema and label trees advance together.
fn element_label(labels: &LabelNode) -> &LabelNode {
    match labels {
        LabelNode::Option(cell) | LabelNode::Vec(cell) | LabelNode::Array(cell) => cell,
        other => panic!("a container schema needs a container label, found {other:?}"),
    }
}

/// The label under a map's value cell.
fn map_value_label(labels: &LabelNode) -> &LabelNode {
    match labels {
        LabelNode::Map { value, .. } => value,
        other => panic!("a map schema needs a map label, found {other:?}"),
    }
}

/// Every position under [`Decision`] whose shape the fixture has to instantiate
/// for the pinned bytes to freeze it.
fn required_positions() -> BTreeSet<String> {
    let schema = Decision::SCHEMA;
    let labels = Decision::LABEL_NODE;

    let mut positions = BTreeSet::new();
    let mut expanded = BTreeSet::new();
    let mut budget = WALK_BUDGET;
    let mut work = vec![(&schema, &labels)];

    while let Some((schema, labels)) = work.pop() {
        budget = budget.checked_sub(1).expect("the schema walk stays inside its budget");
        match schema {
            SchemaType::Struct { fields, .. } => {
                let LabelNode::Struct { type_label, fields: field_labels, .. } = labels else {
                    panic!("a struct schema needs a struct label, found {labels:?}");
                };
                let name = type_name(type_label.as_deref());
                if !expanded.insert(name.clone()) {
                    continue;
                }
                positions.insert(name);
                work.extend(fields.iter().zip(field_labels.iter()).map(|(field, label)| (&field.ty, label)));
            }
            SchemaType::Enum { variants } => {
                let LabelNode::Enum { type_label, variants: variant_labels } = labels else {
                    panic!("an enum schema needs an enum label, found {labels:?}");
                };
                let name = type_name(type_label.as_deref());
                if !expanded.insert(name.clone()) {
                    continue;
                }
                for (variant, label) in variants.iter().zip(variant_labels.iter()) {
                    match (variant, label) {
                        // A unit variant is a bare discriminant: no payload, so
                        // no shape the fixture could freeze by carrying it.
                        (EnumVariant::Unit { .. }, VariantLabel::Unit { .. }) => {}
                        (
                            EnumVariant::Tuple { name: variant, fields, .. },
                            VariantLabel::Tuple { fields: labels, .. },
                        ) => {
                            positions.insert(format!("{name}::{variant}"));
                            work.extend(fields.iter().zip(labels.iter()));
                        }
                        (
                            EnumVariant::Struct { name: variant, fields, .. },
                            VariantLabel::Struct { fields: labels, .. },
                        ) => {
                            positions.insert(format!("{name}::{variant}"));
                            work.extend(fields.iter().zip(labels.iter()).map(|(field, label)| (&field.ty, label)));
                        }
                        (variant, label) => panic!("variant {variant:?} and label {label:?} disagree on shape"),
                    }
                }
            }
            SchemaType::Option(inner) | SchemaType::Vec(inner) => work.push((inner, element_label(labels))),
            SchemaType::Array { element, .. } => work.push((element, element_label(labels))),
            // A map key is restricted to `String`, an integer scalar, or `Bool`
            // (`SchemaType::Map`), none of which is a nominal type, so the key
            // side holds no position either walk could reach.
            SchemaType::Map { value, .. } => work.push((value, map_value_label(labels))),
            SchemaType::Unit
            | SchemaType::Bool
            | SchemaType::Scalar(_)
            | SchemaType::String
            | SchemaType::Bytes
            | SchemaType::TypeId(_) => {}
        }
    }

    positions
}

/// Every position the fixture's own wire bytes reach.
///
/// The value is encoded the way the journal encodes it — `wire::to_vec` over
/// the serde impls — and read back through the schema. A schema that
/// misdescribed those bytes could not decode them, so this walk cannot report
/// coverage of a graph the column does not actually carry.
fn reached_positions(effects: &[Decision]) -> BTreeSet<String> {
    let schema = Decision::SCHEMA;
    let labels = Decision::LABEL_NODE;

    let mut positions = BTreeSet::new();
    let mut budget = WALK_BUDGET;
    let mut work: Vec<Step<'_>> = effects
        .iter()
        .map(|effect| {
            let bytes = to_vec(effect).expect("a fixture effect wire-encodes");
            let value = decode_schema(&bytes, &schema)
                .unwrap_or_else(|error| panic!("Decision::SCHEMA must describe its own wire bytes: {error}"));
            (value, &schema, &labels)
        })
        .collect();

    while let Some((value, schema, labels)) = work.pop() {
        budget = budget.checked_sub(1).expect("the value walk stays inside its budget");
        match schema {
            SchemaType::Struct { fields, .. } => {
                let LabelNode::Struct { type_label, fields: field_labels, .. } = labels else {
                    panic!("a struct schema needs a struct label, found {labels:?}");
                };
                positions.insert(type_name(type_label.as_deref()));
                let mut object = expect_object(&value);
                work.extend(
                    fields
                        .iter()
                        .zip(field_labels.iter())
                        .map(|(field, label)| (take_field(&mut object, &field.name), &field.ty, label)),
                );
            }
            SchemaType::Enum { variants } => {
                if let Some((position, steps)) = enum_step(value, variants, labels) {
                    positions.insert(position);
                    work.extend(steps);
                }
            }
            SchemaType::Option(inner) => {
                if !value.is_null() {
                    work.push((value, inner, element_label(labels)));
                }
            }
            SchemaType::Vec(inner) | SchemaType::Array { element: inner, .. } => {
                let label = element_label(labels);
                let Value::Array(elements) = value else {
                    panic!("a sequence schema needs an array value, found {value}");
                };
                work.extend(elements.into_iter().map(|element| (element, &**inner, label)));
            }
            SchemaType::Map { value: entry, .. } => {
                let label = map_value_label(labels);
                let Value::Object(entries) = value else {
                    panic!("a map schema needs an object value, found {value}");
                };
                work.extend(entries.into_iter().map(|(_, entry_value)| (entry_value, &**entry, label)));
            }
            SchemaType::Unit
            | SchemaType::Bool
            | SchemaType::Scalar(_)
            | SchemaType::String
            | SchemaType::Bytes
            | SchemaType::TypeId(_) => {}
        }
    }

    positions
}

/// The position one decoded enum value reaches, and the steps into its payload.
///
/// `None` for a unit variant: it decodes to its bare name, has no payload to
/// step into, and holds no position the fixture could fail to freeze.
fn enum_step<'a>(value: Value, variants: &'a [EnumVariant], labels: &'a LabelNode) -> Option<(String, Vec<Step<'a>>)> {
    let LabelNode::Enum { type_label, variants: variant_labels } = labels else {
        panic!("an enum schema needs an enum label, found {labels:?}");
    };
    let Value::Object(selected) = value else {
        return None;
    };

    let (chosen, payload) = selected.into_iter().next().expect("a decoded enum names exactly one variant");
    let index = variants
        .iter()
        .position(|variant| variant.name() == chosen)
        .unwrap_or_else(|| panic!("decoded variant `{chosen}` is not in the schema"));
    let position = format!("{}::{chosen}", type_name(type_label.as_deref()));

    let steps = match (&variants[index], &variant_labels[index]) {
        // One tuple field decodes as the bare field value; several decode as an
        // array, in declaration order.
        (EnumVariant::Tuple { fields, .. }, VariantLabel::Tuple { fields: labels, .. }) => {
            let payload = if fields.len() == 1 {
                vec![payload]
            } else {
                let Value::Array(elements) = payload else {
                    panic!("a multi-field tuple variant needs an array value, found {payload}");
                };
                elements
            };
            payload
                .into_iter()
                .zip(fields.iter().zip(labels.iter()))
                .map(|(value, (field, label))| (value, field, label))
                .collect()
        }
        (EnumVariant::Struct { fields, .. }, VariantLabel::Struct { fields: labels, .. }) => {
            let mut payload = expect_object(&payload);
            fields
                .iter()
                .zip(labels.iter())
                .map(|(field, label)| (take_field(&mut payload, &field.name), &field.ty, label))
                .collect()
        }
        (variant, label) => panic!("variant {variant:?} and label {label:?} disagree on shape"),
    };

    Some((position, steps))
}

fn expect_object(value: &Value) -> serde_json::Map<String, Value> {
    value.as_object().unwrap_or_else(|| panic!("a struct schema needs an object value, found {value}")).clone()
}

fn take_field(object: &mut serde_json::Map<String, Value>, name: &str) -> Value {
    object.remove(name).unwrap_or_else(|| panic!("decoded value is missing field `{name}`"))
}

// Tripwire: the golden fixture cannot silently cover less than the wire graph
// it exists to freeze. The pinned bytes freeze only the positions the
// representative reaches, so a family reachable from `Decision` but absent from
// the fixture is a payload type free to gain a field, encode wider on every new
// row, and pass the pinned-bytes assertion untouched — while boot replay of the
// rows written before it fatally aborts. Adding a variant or a payload type
// anywhere under `Decision` fails here, in the same change, naming what is
// missing.
#[test]
fn every_wire_reachable_family_is_represented() {
    let effects = representative().effects;
    let unrepresented: Vec<String> = required_positions().difference(&reached_positions(&effects)).cloned().collect();

    assert!(
        unrepresented.is_empty(),
        "unrepresented in the golden fixture: {}\n\
         {} of the positions reachable from Decision are not instantiated by `representative()`, \
         so the pinned bytes do not freeze their shape — extend it to reach each, then repin \
         GOLDEN_DECISIONS.",
        unrepresented.join(", "),
        unrepresented.len(),
    );
}
