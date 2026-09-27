//! The one assertion every validated leaf's rule table runs.

use alloc::format;
use core::fmt::Debug;

use aether_data::wire::{WireDecode, WireEncode, decode_from_slice, encode_to_vec};

/// Assert one rule of a validated newtype through both doors: `new` refuses
/// `reject` with `error` and decode refuses its encoding, while `new` accepts
/// the nearest neighbour `accept` and decode of its encoding gives the same
/// value. Decode reads the inner value's plain encoding, as a peer that never
/// ran `new` would send it.
pub fn assert_rule<T, I, E>(new: impl Fn(I) -> Result<T, E>, reject: I, error: E, accept: I)
where
    T: Debug + PartialEq + for<'de> WireDecode<'de>,
    I: Debug + WireEncode,
    E: Debug + PartialEq,
{
    let decode = |inner: &I| decode_from_slice::<T>(&encode_to_vec(inner).expect("encode inner"));

    let label = format!("{reject:?}");
    assert!(decode(&reject).is_err(), "decode refuses {label}");
    assert_eq!(new(reject).err(), Some(error), "new refuses {label}");

    let label = format!("{accept:?}");
    let decoded = decode(&accept).ok();
    let accepted = new(accept).unwrap_or_else(|error| panic!("new accepts {label}: {error:?}"));
    assert_eq!(decoded, Some(accepted), "decode accepts {label}");
}
