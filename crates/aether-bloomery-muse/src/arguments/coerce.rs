//! The pass over argument JSON that runs before the codec: a canonical
//! decimal string at an integer position becomes the integer, a scalar
//! of the wrong JSON type is refused with how to fix it, and a unit-variant
//! enum that names no variant is refused with the names it takes.
//!
//! The walk is iterative, over an explicit stack, and descends only where
//! the JSON already has the shape the schema wants. Every other position
//! (a missing or unknown key, a non-unit enum, an object at a unit enum, a
//! map, a type id, bytes, a non-object at a struct, a non-array at a
//! container) is left for the codec.

use core::cmp::Reverse;
use core::fmt::{self, Display};

use aether_data::{EnumVariant, Primitive, SchemaType};
use serde_json::Value;

/// The most bytes of a sent value a correction quotes.
const SENT_MAX_BYTES: usize = 64;

/// What a scalar position wants, for the correction's text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wanted {
    Integer { min: i128, max: i128 },
    Float,
    Bool,
    String,
    Variant { names: Vec<String> },
}

/// A scalar of the wrong JSON type, and what to send instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Correction {
    /// The field's path without the `$.` root; empty for the root itself.
    field: String,
    wanted: Wanted,
    /// The offending value, compact and cut.
    sent: String,
}

impl Display for Correction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let field = if self.field.is_empty() {
            "the arguments"
        } else {
            &self.field
        };
        let sent = &self.sent;
        match &self.wanted {
            Wanted::Integer { min, max } => {
                write!(f, "`{field}` must be a JSON number, a whole number from {min} to {max}, e.g. `3`, not `{sent}`")
            }
            Wanted::Float => write!(f, "`{field}` must be a JSON number, e.g. `1.5`, not `{sent}`"),
            Wanted::Bool => write!(f, "`{field}` must be `true` or `false`, not `{sent}`"),
            Wanted::String => write!(f, "`{field}` must be a JSON string, e.g. `\"text\"`, not `{sent}`"),
            Wanted::Variant { names } => {
                let listed = names.iter().map(|name| format!("\"{name}\"")).collect::<Vec<_>>().join(", ");
                write!(f, "`{field}` must be one of {listed}, not `{sent}`")
            }
        }
    }
}

/// Coerce every scalar of `value` the schema wants as an integer, or refuse
/// the first scalar that cannot be one of the type the schema wants.
///
/// # Errors
///
/// The [`Correction`] for the first scalar, in field order, of the wrong JSON
/// type.
pub(super) fn scalars(value: &mut Value, schema: &SchemaType) -> Result<(), Correction> {
    let mut stack = vec![(value, schema, String::new())];
    while let Some((value, schema, field)) = stack.pop() {
        match schema {
            SchemaType::Scalar(primitive) => leaf(value, *primitive, &field)?,
            SchemaType::Bool => {
                if !value.is_boolean() {
                    return Err(correction(&field, Wanted::Bool, value));
                }
            }
            SchemaType::String => {
                if !value.is_string() {
                    return Err(correction(&field, Wanted::String, value));
                }
            }
            SchemaType::Option(inner) => {
                if !value.is_null() {
                    stack.push((value, &**inner, field));
                }
            }
            SchemaType::Vec(element) | SchemaType::Array { element, .. } => {
                if let Value::Array(items) = value {
                    let children =
                        items.iter_mut().enumerate().map(|(at, item)| (item, &**element, format!("{field}[{at}]")));
                    stack.extend(children.rev());
                }
            }
            SchemaType::Struct { fields, .. } => {
                if let Value::Object(object) = value {
                    let mut children: Vec<_> = object
                        .iter_mut()
                        .filter_map(|(key, child)| {
                            let at = fields.iter().position(|named| named.name == key.as_str())?;
                            let name = &fields[at].name;
                            let path = if field.is_empty() {
                                name.to_string()
                            } else {
                                format!("{field}.{name}")
                            };
                            Some((at, child, &fields[at].ty, path))
                        })
                        .collect();
                    children.sort_by_key(|(at, ..)| Reverse(*at));
                    stack.extend(children.into_iter().map(|(_, child, ty, path)| (child, ty, path)));
                }
            }
            SchemaType::Enum { variants } => {
                let all_unit = variants.iter().all(|variant| matches!(variant, EnumVariant::Unit { .. }));
                let is_string = value.is_string();
                let is_object = value.is_object();
                let sent = value.as_str();
                let known = sent.is_some_and(|name| variants.iter().any(|variant| variant.name() == name));
                let unknown_name = is_string && !known;
                let neither_string_nor_object = !is_string && !is_object;
                let refuses = all_unit && (unknown_name || neither_string_nor_object);
                if refuses {
                    let names = variants.iter().map(|variant| variant.name().to_owned()).collect();
                    return Err(correction(&field, Wanted::Variant { names }, value));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn leaf(value: &mut Value, primitive: Primitive, field: &str) -> Result<(), Correction> {
    let Some((min, max)) = bounds(primitive) else {
        return if value.is_number() {
            Ok(())
        } else {
            Err(correction(field, Wanted::Float, value))
        };
    };
    let number = match value {
        Value::Number(number) => number.as_i64().map(i128::from).or_else(|| number.as_u64().map(i128::from)),
        Value::String(text) => canonical(text, min < 0),
        _ => None,
    };
    match number {
        Some(number) if (min..=max).contains(&number) => {
            if value.is_string() {
                let integer =
                    u64::try_from(number).map(Value::from).or_else(|_| i64::try_from(number).map(Value::from));
                if let Ok(integer) = integer {
                    *value = integer;
                }
            }
            Ok(())
        }
        _ => Err(correction(field, Wanted::Integer { min, max }, value)),
    }
}

/// The range of an integer primitive; none for a float.
fn bounds(primitive: Primitive) -> Option<(i128, i128)> {
    Some(match primitive {
        Primitive::U8 => (i128::from(u8::MIN), i128::from(u8::MAX)),
        Primitive::U16 => (i128::from(u16::MIN), i128::from(u16::MAX)),
        Primitive::U32 => (i128::from(u32::MIN), i128::from(u32::MAX)),
        Primitive::U64 => (i128::from(u64::MIN), i128::from(u64::MAX)),
        Primitive::I8 => (i128::from(i8::MIN), i128::from(i8::MAX)),
        Primitive::I16 => (i128::from(i16::MIN), i128::from(i16::MAX)),
        Primitive::I32 => (i128::from(i32::MIN), i128::from(i32::MAX)),
        Primitive::I64 => (i128::from(i64::MIN), i128::from(i64::MAX)),
        Primitive::F32 | Primitive::F64 => return None,
    })
}

/// The integer a canonical decimal spells: ASCII digits with no leading zero
/// except `0` itself, and a leading `-` only when `signed` and never `-0`.
fn canonical(text: &str, signed: bool) -> Option<i128> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(digits) if signed => (true, digits),
        _ => (false, text),
    };
    let well_formed = !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
        && !(negative && digits == "0");
    // Twenty digits cover every 64-bit bound; a longer string is out of range whatever it spells.
    if !well_formed || digits.len() > 20 {
        return None;
    }
    let magnitude: i128 = digits.parse().ok()?;
    Some(if negative {
        -magnitude
    } else {
        magnitude
    })
}

fn correction(field: &str, wanted: Wanted, sent: &Value) -> Correction {
    let mut sent = sent.to_string();
    if sent.len() > SENT_MAX_BYTES {
        let mut end = SENT_MAX_BYTES;
        while !sent.is_char_boundary(end) {
            end -= 1;
        }
        sent.truncate(end);
        sent.push('…');
    }
    Correction { field: field.to_owned(), wanted, sent }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use aether_data::{EnumVariant, NamedField, Primitive, SchemaType};
    use serde_json::{Value, json};

    use super::scalars;

    fn signed_struct() -> SchemaType {
        SchemaType::Struct {
            fields: Cow::Owned(vec![NamedField {
                name: Cow::Borrowed("delta"),
                ty: SchemaType::Scalar(Primitive::I32),
            }]),
            repr_c: false,
        }
    }

    fn ending_enum() -> SchemaType {
        SchemaType::Enum {
            variants: Cow::Owned(vec![
                EnumVariant::Unit { name: Cow::Borrowed("Done"), discriminant: 0 },
                EnumVariant::Unit { name: Cow::Borrowed("Blocked"), discriminant: 1 },
                EnumVariant::Unit { name: Cow::Borrowed("Asked"), discriminant: 2 },
            ]),
        }
    }

    fn ending_struct() -> SchemaType {
        SchemaType::Struct {
            fields: Cow::Owned(vec![NamedField { name: Cow::Borrowed("ending"), ty: ending_enum() }]),
            repr_c: false,
        }
    }

    fn mixed_enum() -> SchemaType {
        SchemaType::Enum {
            variants: Cow::Owned(vec![
                EnumVariant::Unit { name: Cow::Borrowed("Done"), discriminant: 0 },
                EnumVariant::Struct {
                    name: Cow::Borrowed("Named"),
                    discriminant: 1,
                    fields: Cow::Owned(vec![NamedField {
                        name: Cow::Borrowed("depth"),
                        ty: SchemaType::Scalar(Primitive::U8),
                    }]),
                },
            ]),
        }
    }

    #[test]
    fn a_signed_leaf_accepts_a_negative_string_and_refuses_negative_zero() {
        // Catches a signed rule that drops the sign or accepts `-0`.
        let mut accepted = json!({"delta": "-3"});
        assert_eq!(scalars(&mut accepted, &signed_struct()), Ok(()));
        assert_eq!(accepted, json!({"delta": -3}));

        let mut refused = json!({"delta": "-0"});
        assert!(scalars(&mut refused, &signed_struct()).is_err());
    }

    #[test]
    fn a_long_sent_value_is_cut_on_a_char_boundary() {
        // Catches a cut that splits a character or leaves the value unbounded.
        let mut value = Value::String("é".repeat(100));
        let shape = SchemaType::Scalar(Primitive::U8);
        let text = scalars(&mut value, &shape).expect_err("refused").to_string();
        let sent = text.rsplit_once("not `").expect("the sent value").1;
        assert!(sent.ends_with("…`"), "{sent}");
        assert!(sent.len() <= 64 + "…`".len(), "{sent}");
    }

    #[test]
    fn an_element_is_named_by_its_index() {
        // Catches a container element reported without where it sits.
        let shape = SchemaType::Vec(aether_data::SchemaCell::owned(SchemaType::Scalar(Primitive::U8)));
        let mut value = json!([1, "x"]);
        let text = scalars(&mut value, &shape).expect_err("refused").to_string();
        assert!(text.starts_with("`[1]` must be a JSON number"), "{text}");
    }

    #[test]
    fn a_unit_enum_takes_its_variant_name_unchanged() {
        // Catches a correction that fires on a valid name, and one that disturbs the object form the codec reads.
        let mut named = json!({"ending": "Done"});
        assert_eq!(scalars(&mut named, &ending_struct()), Ok(()));
        assert_eq!(named, json!({"ending": "Done"}));

        let mut object = json!({"ending": {"Done": null}});
        assert_eq!(scalars(&mut object, &ending_struct()), Ok(()));
        assert_eq!(object, json!({"ending": {"Done": null}}));
    }

    #[test]
    fn an_unknown_unit_name_is_refused_with_the_names_it_takes() {
        // Catches the terse codec text reaching the model instead of the names.
        let listed = "`ending` must be one of \"Done\", \"Blocked\", \"Asked\"";
        for sent in [json!({"ending": "done"}), json!({"ending": "{\"Done\": {}}"})] {
            let mut value = sent;
            let text = scalars(&mut value, &ending_struct()).expect_err("refused").to_string();
            assert!(text.starts_with(listed), "{text}");
            assert!(text.contains("must be one of"), "{text}");
        }
        let mut lower = json!({"ending": "done"});
        let text = scalars(&mut lower, &ending_struct()).expect_err("refused").to_string();
        assert_eq!(text, "`ending` must be one of \"Done\", \"Blocked\", \"Asked\", not `\"done\"`");
    }

    #[test]
    fn an_enum_with_a_struct_variant_is_left_for_the_codec() {
        // Catches a correction that preempts the codec at an enum it does not understand.
        let mut named = json!("done");
        assert_eq!(scalars(&mut named, &mixed_enum()), Ok(()));
        assert_eq!(named, json!("done"));

        let mut number = json!(7);
        assert_eq!(scalars(&mut number, &mixed_enum()), Ok(()));
        assert_eq!(number, json!(7));
    }
}
