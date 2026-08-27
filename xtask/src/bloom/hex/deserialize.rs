//! The accepting half: a digest in a request body may be written either as the
//! canonical 32-number array or as a 64-hex string, and either resolves to the
//! same bytes before the value leaves this module.
//!
//! A `Deserializer` adapter for the same reason [`super::serialize`] is a
//! serializer one: hex is accepted at digest positions and nowhere else. A pass
//! that rewrote every 64-hex-looking string in the body would also rewrite an
//! idempotency key, a work-order description, or any other operator prose that
//! happened to look like a digest. Keying on the newtype-struct name `Digest`
//! asks for the value's type instead of guessing from its spelling, so a
//! malformed digest is a refusal that names its field rather than a silent
//! misparse.

use serde::de::{
    DeserializeOwned, DeserializeSeed, Deserializer, EnumAccess, Error as _, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};
use serde::forward_to_deserialize_any;
use serde_json::map::IntoIter as ObjectEntries;
use serde_json::{Map, Number, Value};
use std::vec::IntoIter as ArrayItems;

use super::{DIGEST, digest_from_hex};

/// Decode a request body, accepting either digest form.
pub fn from_slice<T: DeserializeOwned>(body: &[u8]) -> Result<T, serde_json::Error> {
    T::deserialize(HexDigests(serde_json::from_slice(body)?))
}

/// A deserializer over one already-parsed JSON value that resolves a hex string
/// at a digest position into the canonical bytes, and is otherwise the value's
/// own deserializer.
struct HexDigests(Value);

impl<'de> Deserializer<'de> for HexDigests {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Null => visitor.visit_unit(),
            Value::Bool(value) => visitor.visit_bool(value),
            Value::Number(number) => visit_number(&number, visitor),
            Value::String(text) => visitor.visit_string(text),
            Value::Array(items) => visitor.visit_seq(Elements(items.into_iter())),
            Value::Object(fields) => visitor.visit_map(Fields::over(fields)),
        }
    }

    /// The hook. At a digest position a string is the hex form and is decoded
    /// here; anything else is handed on unchanged, so the canonical byte array
    /// takes exactly the path it always did.
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if name == DIGEST
            && let Value::String(text) = &self.0
        {
            let Some(digest) = digest_from_hex(text) else {
                return Err(Self::Error::custom(
                    "a digest is 64 hex characters or 32 bytes; this is neither".to_owned(),
                ));
            };
            let bytes = digest.as_bytes().iter().copied().map(Value::from).collect();
            return visitor.visit_newtype_struct(Self(Value::Array(bytes)));
        }
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Null => visitor.visit_none(),
            present => visitor.visit_some(Self(present)),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::String(variant) => visitor.visit_enum(Variant { variant, content: None }),
            Value::Object(fields) if fields.len() == 1 => {
                let (variant, content) = fields.into_iter().next().expect("a one-entry map yields exactly one entry");
                visitor.visit_enum(Variant { variant, content: Some(content) })
            }
            _ => Err(Self::Error::custom("expected a variant name or a single-variant object".to_owned())),
        }
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf unit unit_struct seq tuple
        tuple_struct map struct identifier ignored_any
    }
}

/// Hand a JSON number to the visitor as the widest type that holds it, leaving
/// the numeric conversion to the target's own visitor the way any deserializer
/// over parsed JSON does.
fn visit_number<'de, V: Visitor<'de>>(number: &Number, visitor: V) -> Result<V::Value, serde_json::Error> {
    if let Some(value) = number.as_u64() {
        visitor.visit_u64(value)
    } else if let Some(value) = number.as_i64() {
        visitor.visit_i64(value)
    } else if let Some(value) = number.as_f64() {
        visitor.visit_f64(value)
    } else {
        Err(serde_json::Error::custom(format!("`{number}` is not a representable number")))
    }
}

/// The elements of a JSON array, each deserialized through the adapter so a
/// digest nested inside one is still recognized.
struct Elements(ArrayItems<Value>);

impl<'de> SeqAccess<'de> for Elements {
    type Error = serde_json::Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error> {
        self.0.next().map(|item| seed.deserialize(HexDigests(item))).transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

/// The fields of a JSON object, deserialized through the adapter and named in
/// any failure they produce.
///
/// The naming is what makes a malformed digest actionable: the value's own
/// deserializer knows the digest is unreadable but not what it was called, and
/// the field name lives here. Prefixing on the way out also composes — a
/// failure inside a nested object arrives already carrying the inner field, so
/// the operator reads the path down to it rather than the leaf alone.
struct Fields {
    entries: ObjectEntries,
    /// The value paired with the key just yielded, awaiting its `next_value_seed`.
    pending: Option<Value>,
    /// That key's name, for the failure message.
    field: Option<String>,
}

impl Fields {
    fn over(fields: Map<String, Value>) -> Self {
        Self { entries: fields.into_iter(), pending: None, field: None }
    }
}

impl<'de> MapAccess<'de> for Fields {
    type Error = serde_json::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error> {
        let Some((field, value)) = self.entries.next() else {
            return Ok(None);
        };
        self.pending = Some(value);

        let key = seed.deserialize(HexDigests(Value::String(field.clone())))?;
        self.field = Some(field);

        Ok(Some(key))
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Self::Error> {
        let value = self.pending.take().unwrap_or(Value::Null);
        let field = self.field.take().unwrap_or_default();

        seed.deserialize(HexDigests(value)).map_err(|error| Self::Error::custom(format!("{field}: {error}")))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

/// One externally-tagged enum variant: its name, and the content a non-unit
/// variant carries.
struct Variant {
    variant: String,
    content: Option<Value>,
}

impl<'de> EnumAccess<'de> for Variant {
    type Error = serde_json::Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self), Self::Error> {
        let name = seed.deserialize(HexDigests(Value::String(self.variant.clone())))?;
        Ok((name, self))
    }
}

impl<'de> VariantAccess<'de> for Variant {
    type Error = serde_json::Error;

    fn unit_variant(self) -> Result<(), Self::Error> {
        match self.content {
            None => Ok(()),
            Some(_) => Err(Self::Error::custom(format!("variant `{}` takes no content", self.variant))),
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Self::Error> {
        seed.deserialize(HexDigests(self.content()?))
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error> {
        HexDigests(self.content()?).deserialize_any(visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        HexDigests(self.content()?).deserialize_any(visitor)
    }
}

impl Variant {
    /// The content a non-unit variant must carry.
    fn content(self) -> Result<Value, serde_json::Error> {
        self.content.ok_or_else(|| {
            serde_json::Error::custom(format!("variant `{}` needs content but was named alone", self.variant))
        })
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery::{ConfigRegistry, Digest, Evidence, EvidenceKind, Membership, Workpiece, WorkpieceId};
    use serde_json::json;

    use super::from_slice;
    use crate::bloom::hex::to_vec;

    /// A member whose digest-typed fields — `scope_revision` and the approval's
    /// `subject` / `detail` — each carry a distinguishable value.
    fn member() -> Membership {
        Membership {
            workpiece: WorkpieceId("wp-a".to_owned()),
            scope_revision: Digest::from_bytes([7; 32]),
            configs: ConfigRegistry::default(),
            approval: Evidence {
                subject: Digest::from_bytes([8; 32]),
                kind: EvidenceKind::Approval,
                detail: Digest::from_bytes([9; 32]),
            },
        }
    }

    #[test]
    fn both_digest_spellings_decode_to_the_same_value() {
        let member = member();

        let canonical: Membership =
            from_slice(&serde_json::to_vec(&member).expect("a member renders canonically")).expect("and decodes back");
        let hex: Membership = from_slice(&to_vec(&member).expect("a member renders in hex")).expect("and decodes back");

        assert_eq!(canonical, member, "the canonical byte-array spelling still decodes");
        assert_eq!(hex, member, "the hex spelling decodes to the same member, digest for digest");
    }

    #[test]
    fn malformed_hex_names_the_field_it_failed_in() {
        let mut body = serde_json::to_value(member()).expect("a member renders as JSON");
        body["scope_revision"] = json!("abc");

        let reported = from_slice::<Membership>(body.to_string().as_bytes())
            .expect_err("a short hex digest is a refusal, not a truncated read")
            .to_string();

        assert!(reported.contains("scope_revision"), "the failure names its field: {reported}");
        assert!(reported.contains("64 hex characters"), "and says what it wanted: {reported}");
    }

    #[test]
    fn a_non_hex_character_is_refused_rather_than_misread() {
        let mut body = serde_json::to_value(member()).expect("a member renders as JSON");
        body["scope_revision"] = json!("z".repeat(64));

        assert!(
            from_slice::<Membership>(body.to_string().as_bytes()).is_err(),
            "64 characters that are not hex are not a digest"
        );
    }

    #[test]
    fn a_string_outside_a_digest_position_is_left_alone() {
        // The misparse a spelling-based rewrite would make. A workpiece id is
        // free-form operator text that can be 64 hex characters wide, and it
        // sits one field away from two digests in the same object.
        let hex_looking = "a".repeat(64);
        let staged = Workpiece {
            id: WorkpieceId(hex_looking.clone()),
            intent: Digest::from_bytes([1; 32]),
            scope_revision: Digest::from_bytes([2; 32]),
        };

        let decoded: Workpiece =
            from_slice(&to_vec(&staged).expect("a workpiece renders in hex")).expect("and decodes back");

        assert_eq!(decoded, staged, "the id stays a string while its neighbours decode as digests");
        assert_eq!(decoded.id.0, hex_looking);
    }
}
