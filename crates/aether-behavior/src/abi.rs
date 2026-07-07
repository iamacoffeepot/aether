//! The packed-pointer convention shared by the guest filter shims and
//! the future host (ADR-0137, mirroring the `_p32` pointer-typed exports
//! of `aether-actor`'s `export!`).
//!
//! A guest export that returns a `(ptr, len)` region — `filter`'s outbound
//! envelope, `state_save`'s serialized blob — packs both halves into one
//! `u64`: the guest-memory pointer in the high 32 bits, the byte length in
//! the low 32 bits. wasm32 pointers and lengths are 32-bit, so both fit
//! without loss and the host unpacks the pair with a single shift + mask.

/// Pack a guest-memory `(ptr, len)` pair into a single `u64` return value:
/// `(ptr as u64) << 32 | len as u64`.
#[must_use]
pub const fn pack_ptr_len(ptr: u32, len: u32) -> u64 {
    ((ptr as u64) << 32) | (len as u64)
}

/// Recover the `(ptr, len)` pair a [`pack_ptr_len`] `u64` carries — the
/// high 32 bits are the pointer, the low 32 bits the length.
#[must_use]
pub const fn unpack_ptr_len(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)
}

#[cfg(test)]
mod tests {
    use super::{pack_ptr_len, unpack_ptr_len};

    // Tripwire: the exact bit layout the host decodes against. The pinned
    // value is computed by the shift/mask, so it drifts the moment the
    // packing convention changes — which would silently misroute every
    // guest return across the FFI boundary.
    #[test]
    fn pack_ptr_len_bit_layout_is_pinned() {
        let packed = pack_ptr_len(0x1234_5678, 0x9ABC_DEF0);
        assert_eq!(packed, 0x1234_5678_9ABC_DEF0);
        assert_eq!(unpack_ptr_len(packed), (0x1234_5678, 0x9ABC_DEF0));
    }
}
