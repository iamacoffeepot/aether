//! Leaf [`WireEncode`] / [`WireDecode`] impls — the same set [`crate::Schema`]
//! covers in `schema_impls`, plus clone-on-write string/slice (needed by the
//! metaschema types) and `Box` (needed by [`crate::schema::SchemaShape`]).

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use super::Error;
use super::owned::{
    WireDecode, WireEncode, decode_bytes, decode_seq, encode_bytes, encode_seq, read_presence, take_array, write_count,
};
use crate::{DagId, KindId, MailboxId, ThreadId, TransformId};

macro_rules! scalar {
    ($($t:ty),+ $(,)?) => {
        $(
            impl WireEncode for $t {
                fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
                    out.extend_from_slice(&self.to_le_bytes());
                    Ok(())
                }
            }

            impl<'de> WireDecode<'de> for $t {
                fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
                    Ok(Self::from_le_bytes(take_array(cursor)?))
                }
            }
        )+
    };
}

scalar!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

impl WireEncode for bool {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        out.push(u8::from(*self));
        Ok(())
    }
}

impl<'de> WireDecode<'de> for bool {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        read_presence(cursor)
    }
}

impl WireEncode for () {
    fn encode(&self, _out: &mut Vec<u8>) -> Result<(), Error> {
        Ok(())
    }
}

impl<'de> WireDecode<'de> for () {
    fn decode(_cursor: &mut &'de [u8]) -> Result<Self, Error> {
        Ok(())
    }
}

impl WireEncode for str {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        encode_bytes(out, self.as_bytes())
    }
}

impl WireEncode for String {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.as_str().encode(out)
    }
}

impl<'de> WireDecode<'de> for String {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        let bytes = decode_bytes(cursor)?;
        Self::from_utf8(bytes).map_err(|_| Error::Utf8)
    }
}

impl WireEncode for Cow<'_, str> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.as_ref().encode(out)
    }
}

impl<'de> WireDecode<'de> for Cow<'static, str> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        String::decode(cursor).map(Cow::Owned)
    }
}

impl<T: WireEncode + Clone> WireEncode for Cow<'_, [T]> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        encode_seq(out, self.as_ref())
    }
}

impl<'de, T: WireDecode<'de> + Clone> WireDecode<'de> for Cow<'static, [T]> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        Vec::<T>::decode(cursor).map(Cow::Owned)
    }
}

impl<T: WireEncode> WireEncode for Vec<T> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        encode_seq(out, self)
    }
}

impl<'de, T: WireDecode<'de>> WireDecode<'de> for Vec<T> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        decode_seq(cursor)
    }
}

impl<T: WireEncode> WireEncode for Option<T> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            None => {
                out.push(0);
                Ok(())
            }
            Some(value) => {
                out.push(1);
                value.encode(out)
            }
        }
    }
}

impl<'de, T: WireDecode<'de>> WireDecode<'de> for Option<T> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        if read_presence(cursor)? {
            T::decode(cursor).map(Some)
        } else {
            Ok(None)
        }
    }
}

impl<T: WireEncode, const N: usize> WireEncode for [T; N] {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        for item in self {
            item.encode(out)?;
        }
        Ok(())
    }
}

impl<'de, T: WireDecode<'de>, const N: usize> WireDecode<'de> for [T; N] {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        let mut items = Vec::with_capacity(N);
        for _ in 0..N {
            items.push(T::decode(cursor)?);
        }
        items.try_into().map_err(|_| Error::Length)
    }
}

impl<T: WireEncode> WireEncode for Box<T> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        (**self).encode(out)
    }
}

impl<'de, T: WireDecode<'de>> WireDecode<'de> for Box<T> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        T::decode(cursor).map(Self::new)
    }
}

impl<K: WireEncode + Ord, V: WireEncode> WireEncode for BTreeMap<K, V> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        // Canonical map order is ascending *encoded-key* bytes, not the map's
        // iteration order. Numeric u32 keys 1 and 256 sort 1 < 256; little-endian
        // they sort `[0,1,0,0] < [1,0,0,0]`. Copy of `MapSerializer::end`.
        let mut entries = Vec::with_capacity(self.len());
        for (key, value) in self {
            let mut key_bytes = Vec::new();
            key.encode(&mut key_bytes)?;
            let mut value_bytes = Vec::new();
            value.encode(&mut value_bytes)?;
            entries.push((key_bytes, value_bytes));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        write_count(out, entries.len())?;
        for (key, value) in entries {
            out.extend_from_slice(&key);
            out.extend_from_slice(&value);
        }
        Ok(())
    }
}

impl<'de, K: WireDecode<'de> + Ord, V: WireDecode<'de>> WireDecode<'de> for BTreeMap<K, V> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        let count = u32::from_le_bytes(take_array(cursor)?) as usize;
        let mut map = Self::new();
        for _ in 0..count {
            let key = K::decode(cursor)?;
            let value = V::decode(cursor)?;
            map.insert(key, value);
        }
        Ok(map)
    }
}

macro_rules! typed_id {
    ($($t:ty),+ $(,)?) => {
        $(
            impl WireEncode for $t {
                fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
                    self.0.encode(out)
                }
            }

            impl<'de> WireDecode<'de> for $t {
                fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
                    u64::decode(cursor).map(Self)
                }
            }
        )+
    };
}

typed_id!(MailboxId, KindId, DagId, TransformId, ThreadId);
