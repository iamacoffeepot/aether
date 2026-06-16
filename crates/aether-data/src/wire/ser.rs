//! `serde::Serializer` over the aether wire format (ADR-0118).
//!
//! Emits into an owned `Vec<u8>` (no version byte — `super::to_vec` adds that at
//! the top-level boundary). Collections are length-prefixed with a `u32`; maps
//! buffer their entries and emit them in ascending encoded-key byte order so the
//! encoding is canonical. Sum-type selectors are the `u32` `variant_index`.

use alloc::vec::Vec;

use serde::{Serialize, ser};

use super::Error;

/// Accumulates wire bytes for one value.
pub struct Serializer {
    output: Vec<u8>,
}

impl Serializer {
    pub fn new() -> Self {
        Self { output: Vec::new() }
    }

    pub fn into_output(self) -> Vec<u8> {
        self.output
    }
}

/// Encode a nested value to its own buffer (used by the buffered seq/map
/// serializers, which must measure or reorder before committing to the parent).
fn encode<T: ?Sized + Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let mut sub = Serializer::new();
    value.serialize(&mut sub)?;
    Ok(sub.output)
}

fn write_count(out: &mut Vec<u8>, len: usize) -> Result<(), Error> {
    let count = u32::try_from(len).map_err(|_| Error::Length)?;
    out.extend_from_slice(&count.to_le_bytes());
    Ok(())
}

impl<'a> ser::Serializer for &'a mut Serializer {
    type Ok = ();
    type Error = Error;

    type SerializeSeq = SeqSerializer<'a>;
    type SerializeTuple = &'a mut Serializer;
    type SerializeTupleStruct = &'a mut Serializer;
    type SerializeTupleVariant = &'a mut Serializer;
    type SerializeMap = MapSerializer<'a>;
    type SerializeStruct = &'a mut Serializer;
    type SerializeStructVariant = &'a mut Serializer;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn serialize_bool(self, v: bool) -> Result<(), Error> {
        self.output.push(u8::from(v));
        Ok(())
    }

    fn serialize_i8(self, v: i8) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_i16(self, v: i16) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_i32(self, v: i32) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_i64(self, v: i64) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_i128(self, v: i128) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u8(self, v: u8) -> Result<(), Error> {
        self.output.push(v);
        Ok(())
    }
    fn serialize_u16(self, v: u16) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_u32(self, v: u32) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_u64(self, v: u64) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_u128(self, v: u128) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_f32(self, v: f32) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_f64(self, v: f64) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_char(self, v: char) -> Result<(), Error> {
        self.output.extend_from_slice(&u32::from(v).to_le_bytes());
        Ok(())
    }

    fn serialize_str(self, v: &str) -> Result<(), Error> {
        write_count(&mut self.output, v.len())?;
        self.output.extend_from_slice(v.as_bytes());
        Ok(())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
        write_count(&mut self.output, v.len())?;
        self.output.extend_from_slice(v);
        Ok(())
    }

    fn serialize_none(self) -> Result<(), Error> {
        self.output.push(0);
        Ok(())
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<(), Error> {
        self.output.push(1);
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
    ) -> Result<(), Error> {
        self.output.extend_from_slice(&variant_index.to_le_bytes());
        Ok(())
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.output.extend_from_slice(&variant_index.to_le_bytes());
        value.serialize(self)
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<SeqSerializer<'a>, Error> {
        Ok(SeqSerializer {
            ser: self,
            count: 0,
            buf: Vec::new(),
        })
    }

    fn serialize_tuple(self, _len: usize) -> Result<&'a mut Serializer, Error> {
        Ok(self)
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<&'a mut Serializer, Error> {
        Ok(self)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<&'a mut Serializer, Error> {
        self.output.extend_from_slice(&variant_index.to_le_bytes());
        Ok(self)
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<MapSerializer<'a>, Error> {
        Ok(MapSerializer {
            ser: self,
            entries: Vec::new(),
            key: None,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<&'a mut Serializer, Error> {
        Ok(self)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<&'a mut Serializer, Error> {
        self.output.extend_from_slice(&variant_index.to_le_bytes());
        Ok(self)
    }
}

/// Buffered sequence serializer: elements accumulate in `buf` so the `u32` count
/// can be written ahead of them even when serde reports no length up front.
pub struct SeqSerializer<'a> {
    ser: &'a mut Serializer,
    count: u32,
    buf: Vec<u8>,
}

impl ser::SerializeSeq for SeqSerializer<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        self.buf.extend_from_slice(&encode(value)?);
        self.count = self.count.checked_add(1).ok_or(Error::Length)?;
        Ok(())
    }

    fn end(self) -> Result<(), Error> {
        self.ser.output.extend_from_slice(&self.count.to_le_bytes());
        self.ser.output.extend_from_slice(&self.buf);
        Ok(())
    }
}

impl ser::SerializeTuple for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeTupleStruct for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeTupleVariant for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeStruct for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeStructVariant for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

/// Buffered map serializer: collects encoded `(key, value)` pairs and emits them
/// in ascending key-byte order at `end`, so equal maps encode identically.
pub struct MapSerializer<'a> {
    ser: &'a mut Serializer,
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    key: Option<Vec<u8>>,
}

impl ser::SerializeMap for MapSerializer<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Error> {
        self.key = Some(encode(key)?);
        Ok(())
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        let key = self
            .key
            .take()
            .ok_or_else(|| Error::Message("map value serialized without a key".into()))?;
        self.entries.push((key, encode(value)?));
        Ok(())
    }

    fn end(mut self) -> Result<(), Error> {
        self.entries.sort_by(|a, b| a.0.cmp(&b.0));
        write_count(&mut self.ser.output, self.entries.len())?;
        for (key, value) in self.entries {
            self.ser.output.extend_from_slice(&key);
            self.ser.output.extend_from_slice(&value);
        }
        Ok(())
    }
}
