//! The packed-pointer convention shared by the guest filter shims and
//! the future host (ADR-0137, mirroring the `_p32` pointer-typed exports
//! of `aether-actor`'s `export!`).
//!
//! A guest export that returns a `(ptr, len)` region — `filter`'s outbound
//! envelope, `state_save`'s serialized blob — packs both halves into one
//! `u64`: the guest-memory pointer in the high 32 bits, the byte length in
//! the low 32 bits. wasm32 pointers and lengths are 32-bit, so both fit
//! without loss and the host unpacks the pair with a single shift + mask.
//!
//! ## Guest filter ABI reference
//!
//! The `#[behavior]` macro emits this ABI for a Rust author, so a Rust
//! script never touches it directly — but it is also the contract a raw,
//! hand-authored script (wasm text, or a non-Rust script generator) must
//! satisfy to instantiate against the wasmi-embedding script host (the
//! `host` feature's `BehaviorHost`). Rust `u32`/`u64` map to wasm's
//! `i32`/`i64` (wasm has no unsigned value types), so a WAT author reads
//! every signature below in `iNN` form.
//!
//! **Required exports**
//!
//! - `memory` — the linear memory the host reads and writes; resolved with
//!   `Instance::get_memory(&store, "memory")`. A script that exports no
//!   `memory` fails to instantiate.
//! - `alloc: (i32, i32, i32, i32) -> i32` — `(old_ptr, old_size, align,
//!   new_size) -> ptr`, the `cabi_realloc`-shaped allocator described
//!   below.
//! - `filter: (i64, i32, i32) -> i64` — `(kind_id, ptr, len) -> packed`.
//!   The host writes the inbound mail's encoded bytes into a fresh guest
//!   region (see `alloc`) and calls `filter` with that region's `(ptr,
//!   len)` and the mail's `KindId` as `i64`; the return is a packed
//!   `(ptr, len)` `u64` (see [`pack_ptr_len`] / [`unpack_ptr_len`])
//!   pointing at the encoded [`crate::envelope::FilterOutput`].
//!
//! **Optional exports**
//!
//! - `state_save: () -> i64` — returns a packed `(ptr, len)` `u64`
//!   pointing at the script's serialized migration blob.
//! - `state_load: (i32, i32) -> i32` — `(ptr, len) -> _`, offered the
//!   guest region holding a prior migration blob; the return value is
//!   unused.
//!
//! A stateless script omits both — the host resolves them with a fallible
//! lookup (`Instance::get_typed_func(..).ok()`), so a missing export is not
//! an instantiation failure, and the host simply carries no migration blob
//! for that script across a reload.
//!
//! **The `alloc` convention.** `alloc` is `cabi_realloc`-shaped:
//! `(old_ptr, old_size, align, new_size) -> ptr`. The host only ever
//! requests a *fresh* region for an inbound payload, so it always calls
//! `alloc(0, 0, 1, len)` — a zero old pointer/size (nothing to grow or
//! free) and alignment 1 (the payload is raw bytes, no alignment
//! requirement) — writes `len` bytes at the returned `ptr`, then passes
//! `(ptr, len)` on to `filter` / `state_load`.
//!
//! **The packed return.** `filter` and `state_save` each return a `u64`
//! built by [`pack_ptr_len`]: the guest-memory pointer in the high 32
//! bits, the byte length in the low 32. The host reads that region out of
//! guest linear memory and never frees it — wasm has no memory-shrink
//! operation, so an unreclaimed guest allocation per call is the accepted
//! cost of the convention, not a leak to fix.
//!
//! **Payload framing.** `filter`'s returned region holds an encoded
//! [`crate::envelope::FilterOutput`] — see [`crate::envelope::encode`] /
//! [`crate::envelope::decode`] for the `ENVELOPE_VERSION`-byte-then-wire-body
//! layout. The handled-kind skip-set the host reads at instantiation time
//! (not from a guest call) lives in the `aether.behavior.exports` custom
//! section — see [`crate::manifest::EXPORTS_SECTION`] /
//! [`crate::manifest::decode_exports_manifest`] for its byte layout. Both
//! layouts are owned by their respective modules; this reference only
//! names them.

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
