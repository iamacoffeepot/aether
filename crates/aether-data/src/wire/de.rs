//! `serde::Deserializer` over the aether wire format (ADR-0118).
//!
//! The format carries no type or field tags, so it is **not** self-describing:
//! decoding is driven entirely by the requested type. `deserialize_any` and
//! `deserialize_ignored_any` therefore error — every value is read against a
//! concrete `deserialize_*` request whose shape matches the schema both ends
//! already hold. Structs and tuples decode positionally (`visit_seq`); enums
//! read the `u32` variant index and dispatch.

use core::str::from_utf8;

use serde::de::{
    self, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};

use super::Error;

/// A cursor over wire bytes (the version byte already stripped by the caller).
pub struct Deserializer<'de> {
    input: &'de [u8],
}

impl<'de> Deserializer<'de> {
    pub fn new(input: &'de [u8]) -> Self {
        Self { input }
    }

    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
    }

    pub fn remaining(&self) -> &'de [u8] {
        self.input
    }

    fn take(&mut self, n: usize) -> Result<&'de [u8], Error> {
        if self.input.len() < n {
            return Err(Error::UnexpectedEof);
        }
        let (head, tail) = self.input.split_at(n);
        self.input = tail;
        Ok(head)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn read_byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn read_count(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take_array::<4>()?))
    }

    fn read_bool(&mut self) -> Result<bool, Error> {
        match self.read_byte()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(Error::InvalidBool(other)),
        }
    }

    fn read_str(&mut self) -> Result<&'de str, Error> {
        let len = self.read_count()? as usize;
        let bytes = self.take(len)?;
        from_utf8(bytes).map_err(|_| Error::Utf8)
    }
}

impl<'de> de::Deserializer<'de> for &mut Deserializer<'de> {
    type Error = Error;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(Error::NotSelfDescribing)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(Error::NotSelfDescribing)
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_bool(self.read_bool()?)
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i8(i8::from_le_bytes(self.take_array::<1>()?))
    }
    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i16(i16::from_le_bytes(self.take_array::<2>()?))
    }
    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i32(i32::from_le_bytes(self.take_array::<4>()?))
    }
    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i64(i64::from_le_bytes(self.take_array::<8>()?))
    }
    fn deserialize_i128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i128(i128::from_le_bytes(self.take_array::<16>()?))
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u8(self.read_byte()?)
    }
    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u16(u16::from_le_bytes(self.take_array::<2>()?))
    }
    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u32(u32::from_le_bytes(self.take_array::<4>()?))
    }
    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u64(u64::from_le_bytes(self.take_array::<8>()?))
    }
    fn deserialize_u128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u128(u128::from_le_bytes(self.take_array::<16>()?))
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_f32(f32::from_le_bytes(self.take_array::<4>()?))
    }
    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_f64(f64::from_le_bytes(self.take_array::<8>()?))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let code = u32::from_le_bytes(self.take_array::<4>()?);
        let c = char::from_u32(code).ok_or(Error::InvalidChar(code))?;
        visitor.visit_char(c)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_borrowed_str(self.read_str()?)
    }
    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_borrowed_str(self.read_str()?)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let len = self.read_count()? as usize;
        visitor.visit_borrowed_bytes(self.take(len)?)
    }
    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let len = self.read_count()? as usize;
        visitor.visit_borrowed_bytes(self.take(len)?)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.read_byte()? {
            0 => visitor.visit_none(),
            1 => visitor.visit_some(self),
            other => Err(Error::InvalidBool(other)),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let remaining = self.read_count()?;
        visitor.visit_seq(Counted {
            de: self,
            remaining,
        })
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        let remaining = u32::try_from(len).map_err(|_| Error::Length)?;
        visitor.visit_seq(Counted {
            de: self,
            remaining,
        })
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        let remaining = u32::try_from(len).map_err(|_| Error::Length)?;
        visitor.visit_seq(Counted {
            de: self,
            remaining,
        })
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let remaining = self.read_count()?;
        visitor.visit_map(Counted {
            de: self,
            remaining,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        let remaining = u32::try_from(fields.len()).map_err(|_| Error::Length)?;
        visitor.visit_seq(Counted {
            de: self,
            remaining,
        })
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_enum(Enum { de: self })
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u32(u32::from_le_bytes(self.take_array::<4>()?))
    }
}

/// `SeqAccess` / `MapAccess` over a fixed remaining count. For `Vec` / `Map` the
/// count was read from the wire; for tuples / structs / tuple-and-struct enum
/// variants it is the schema-known field count (no count on the wire).
struct Counted<'a, 'de> {
    de: &'a mut Deserializer<'de>,
    remaining: u32,
}

impl<'de> SeqAccess<'de> for Counted<'_, 'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(&mut *self.de).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining as usize)
    }
}

impl<'de> MapAccess<'de> for Counted<'_, 'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(&mut *self.de).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        seed.deserialize(&mut *self.de)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining as usize)
    }
}

/// `EnumAccess`: read the `u32` variant index and feed it to the variant seed,
/// then expose the variant body.
struct Enum<'a, 'de> {
    de: &'a mut Deserializer<'de>,
}

impl<'a, 'de> EnumAccess<'de> for Enum<'a, 'de> {
    type Error = Error;
    type Variant = Variant<'a, 'de>;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Error> {
        let index = u32::from_le_bytes(self.de.take_array::<4>()?);
        let value = seed.deserialize(index.into_deserializer())?;
        Ok((value, Variant { de: self.de }))
    }
}

struct Variant<'a, 'de> {
    de: &'a mut Deserializer<'de>,
}

impl<'de> VariantAccess<'de> for Variant<'_, 'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        seed.deserialize(&mut *self.de)
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        let remaining = u32::try_from(len).map_err(|_| Error::Length)?;
        visitor.visit_seq(Counted {
            de: self.de,
            remaining,
        })
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        let remaining = u32::try_from(fields.len()).map_err(|_| Error::Length)?;
        visitor.visit_seq(Counted {
            de: self.de,
            remaining,
        })
    }
}
