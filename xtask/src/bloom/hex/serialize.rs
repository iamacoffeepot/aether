//! The rendering half: every digest in a response body serializes as a 64-hex
//! string instead of the canonical 32-number array.
//!
//! It is a `Serializer` adapter rather than a rewrite of the rendered JSON,
//! because only the serializer knows the *type* it is rendering. Once a value
//! is JSON, a digest and a 32-byte `Vec<u8>` are the same array of numbers —
//! the statement words a `Statement` carries are exactly that shape — so a
//! rewrite would have to guess between them and would mangle the one to
//! prettify the other. The adapter keys on the newtype-struct name `Digest`
//! reports for itself, which fires exactly where a digest is, at any depth,
//! without this crate having to name the fields it must reach inside value
//! types it does not own.

use core::fmt::Display;

use serde::ser::{
    Serialize, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
    SerializeTupleStruct, SerializeTupleVariant, Serializer,
};

use super::{DIGEST, hex_encode};

/// Render `value` as JSON with every digest inside it spelled in hex.
pub fn to_vec(value: &impl Serialize) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&HexDigests(value))
}

/// A `Serialize` shim that routes `T` through [`DigestHex`]. Wrapping the value
/// rather than the serializer is what lets the adapter be re-applied to every
/// nested element without the compound serializers below knowing anything about
/// the types they hold.
struct HexDigests<'a, T: ?Sized>(&'a T);

impl<T: Serialize + ?Sized> Serialize for HexDigests<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(DigestHex(serializer))
    }
}

/// A serializer that answers a `Digest` newtype with its hex string and
/// delegates everything else to the serializer it wraps, re-wrapping each
/// nested value so the hook keeps applying all the way down.
struct DigestHex<S>(S);

/// Delegate a scalar `serialize_*` method straight to the inner serializer.
macro_rules! delegate {
    ($($method:ident($($value:ty)?);)*) => {
        $( delegate!(@one $method $($value)?); )*
    };
    (@one $method:ident $value:ty) => {
        fn $method(self, value: $value) -> Result<Self::Ok, Self::Error> {
            self.0.$method(value)
        }
    };
    (@one $method:ident) => {
        fn $method(self) -> Result<Self::Ok, Self::Error> {
            self.0.$method()
        }
    };
}

impl<S: Serializer> Serializer for DigestHex<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = Nested<S::SerializeSeq>;
    type SerializeTuple = Nested<S::SerializeTuple>;
    type SerializeTupleStruct = Nested<S::SerializeTupleStruct>;
    type SerializeTupleVariant = Nested<S::SerializeTupleVariant>;
    type SerializeMap = Nested<S::SerializeMap>;
    type SerializeStruct = Nested<S::SerializeStruct>;
    type SerializeStructVariant = Nested<S::SerializeStructVariant>;

    delegate! {
        serialize_bool(bool);
        serialize_i8(i8);
        serialize_i16(i16);
        serialize_i32(i32);
        serialize_i64(i64);
        serialize_i128(i128);
        serialize_u8(u8);
        serialize_u16(u16);
        serialize_u32(u32);
        serialize_u64(u64);
        serialize_u128(u128);
        serialize_f32(f32);
        serialize_f64(f64);
        serialize_char(char);
        serialize_str(&str);
        serialize_bytes(&[u8]);
        serialize_none();
        serialize_unit();
        serialize_unit_struct(&'static str);
    }

    fn serialize_unit_variant(self, name: &'static str, index: u32, variant: &'static str) -> Result<S::Ok, S::Error> {
        self.0.serialize_unit_variant(name, index, variant)
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<S::Ok, S::Error> {
        self.0.serialize_some(&HexDigests(value))
    }

    /// The hook. A newtype calling itself `Digest` whose contents read back as
    /// 32 bytes renders as hex; anything else — including some future `Digest`
    /// wrapping something other than 32 bytes — falls through to the canonical
    /// rendering rather than being mangled into a wrong string.
    fn serialize_newtype_struct<T: ?Sized + Serialize>(self, name: &'static str, value: &T) -> Result<S::Ok, S::Error> {
        if name == DIGEST
            && let Some(bytes) = digest_bytes(value)
        {
            return self.0.serialize_str(&hex_encode(&bytes));
        }
        self.0.serialize_newtype_struct(name, &HexDigests(value))
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<S::Ok, S::Error> {
        self.0.serialize_newtype_variant(name, index, variant, &HexDigests(value))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, S::Error> {
        self.0.serialize_seq(len).map(Nested)
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, S::Error> {
        self.0.serialize_tuple(len).map(Nested)
    }

    fn serialize_tuple_struct(self, name: &'static str, len: usize) -> Result<Self::SerializeTupleStruct, S::Error> {
        self.0.serialize_tuple_struct(name, len).map(Nested)
    }

    fn serialize_tuple_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, S::Error> {
        self.0.serialize_tuple_variant(name, index, variant, len).map(Nested)
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, S::Error> {
        self.0.serialize_map(len).map(Nested)
    }

    fn serialize_struct(self, name: &'static str, len: usize) -> Result<Self::SerializeStruct, S::Error> {
        self.0.serialize_struct(name, len).map(Nested)
    }

    fn serialize_struct_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, S::Error> {
        self.0.serialize_struct_variant(name, index, variant, len).map(Nested)
    }

    fn collect_str<T: ?Sized + Display>(self, value: &T) -> Result<S::Ok, S::Error> {
        self.0.collect_str(value)
    }
}

/// A compound serializer that re-wraps every value handed to it, so a digest
/// nested inside a sequence, a map, or a struct is rendered the same way one at
/// the top level is. One type serves all seven compound traits — each does the
/// same single thing.
struct Nested<T>(T);

impl<T: SerializeSeq> SerializeSeq for Nested<T> {
    type Ok = T::Ok;
    type Error = T::Error;

    fn serialize_element<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<(), T::Error> {
        self.0.serialize_element(&HexDigests(value))
    }

    fn end(self) -> Result<T::Ok, T::Error> {
        self.0.end()
    }
}

impl<T: SerializeTuple> SerializeTuple for Nested<T> {
    type Ok = T::Ok;
    type Error = T::Error;

    fn serialize_element<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<(), T::Error> {
        self.0.serialize_element(&HexDigests(value))
    }

    fn end(self) -> Result<T::Ok, T::Error> {
        self.0.end()
    }
}

impl<T: SerializeTupleStruct> SerializeTupleStruct for Nested<T> {
    type Ok = T::Ok;
    type Error = T::Error;

    fn serialize_field<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<(), T::Error> {
        self.0.serialize_field(&HexDigests(value))
    }

    fn end(self) -> Result<T::Ok, T::Error> {
        self.0.end()
    }
}

impl<T: SerializeTupleVariant> SerializeTupleVariant for Nested<T> {
    type Ok = T::Ok;
    type Error = T::Error;

    fn serialize_field<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<(), T::Error> {
        self.0.serialize_field(&HexDigests(value))
    }

    fn end(self) -> Result<T::Ok, T::Error> {
        self.0.end()
    }
}

impl<T: SerializeMap> SerializeMap for Nested<T> {
    type Ok = T::Ok;
    type Error = T::Error;

    fn serialize_key<V: ?Sized + Serialize>(&mut self, key: &V) -> Result<(), T::Error> {
        self.0.serialize_key(&HexDigests(key))
    }

    fn serialize_value<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<(), T::Error> {
        self.0.serialize_value(&HexDigests(value))
    }

    fn end(self) -> Result<T::Ok, T::Error> {
        self.0.end()
    }
}

impl<T: SerializeStruct> SerializeStruct for Nested<T> {
    type Ok = T::Ok;
    type Error = T::Error;

    fn serialize_field<V: ?Sized + Serialize>(&mut self, name: &'static str, value: &V) -> Result<(), T::Error> {
        self.0.serialize_field(name, &HexDigests(value))
    }

    fn skip_field(&mut self, name: &'static str) -> Result<(), T::Error> {
        self.0.skip_field(name)
    }

    fn end(self) -> Result<T::Ok, T::Error> {
        self.0.end()
    }
}

impl<T: SerializeStructVariant> SerializeStructVariant for Nested<T> {
    type Ok = T::Ok;
    type Error = T::Error;

    fn serialize_field<V: ?Sized + Serialize>(&mut self, name: &'static str, value: &V) -> Result<(), T::Error> {
        self.0.serialize_field(name, &HexDigests(value))
    }

    fn skip_field(&mut self, name: &'static str) -> Result<(), T::Error> {
        self.0.skip_field(name)
    }

    fn end(self) -> Result<T::Ok, T::Error> {
        self.0.end()
    }
}

/// Read the 32 raw bytes back out of a digest's inner array.
///
/// The hook holds an opaque `&impl Serialize`, so the bytes are recovered by
/// rendering it — bounded by the digest's own size, and the price of reaching
/// inside a type this crate does not own. `None` for anything that is not 32
/// byte-valued numbers, which is what makes the hook fail safe.
fn digest_bytes(value: &(impl Serialize + ?Sized)) -> Option<[u8; 32]> {
    let rendered = serde_json::to_value(value).ok()?;
    let numbers = rendered.as_array()?;
    let mut bytes = [0u8; 32];
    if numbers.len() != bytes.len() {
        return None;
    }
    for (slot, number) in bytes.iter_mut().zip(numbers) {
        *slot = u8::try_from(number.as_u64()?).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use aether_bloomery::{BloomId, Digest};
    use serde::Serialize;

    use super::to_vec;

    /// A digest sitting beside a byte string exactly as long as one — the pair
    /// a renderer has to tell apart.
    #[derive(Serialize)]
    struct Sample {
        words: Vec<u8>,
        subject: Digest,
    }

    #[test]
    fn a_digest_renders_as_hex_at_every_depth() {
        let rendered = String::from_utf8(
            to_vec(&vec![Some(BloomId(Digest::from_bytes([0xab; 32])))]).expect("a nested bloom id renders"),
        )
        .expect("rendered JSON is UTF-8");

        assert_eq!(rendered, format!("[\"{}\"]", "ab".repeat(32)), "a digest inside a seq and an option renders hex");
    }

    #[test]
    fn a_thirty_two_byte_field_that_is_not_a_digest_still_renders_as_bytes() {
        // The discrimination a rewrite of the rendered JSON could not make.
        // Statement words are a `Vec<u8>` that can be exactly digest-length,
        // and hex-encoding those would corrupt the one field on this surface an
        // operator writes prose into.
        let sample = Sample { words: vec![b'a'; 32], subject: Digest::from_bytes([1; 32]) };

        let rendered = String::from_utf8(to_vec(&sample).expect("the sample renders")).expect("rendered JSON is UTF-8");

        assert!(rendered.contains("\"words\":[97,"), "32 bytes stay a byte array: {rendered}");
        assert!(rendered.contains(&format!("\"{}\"", "01".repeat(32))), "the digest beside them is hex: {rendered}");
    }
}
