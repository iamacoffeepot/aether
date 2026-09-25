//! Guest blob reads (ADR-0238 decisions 2 and 9): the guest half of the
//! `blob_hold_p32` / `blob_read_p32` / `blob_drop_p32` host fns.
//!
//! A guest's `Shared` [`aether_data::Blob`] is backed by a `GuestHold` that
//! names its store entry by hash. Each call hands the host a pointer to that
//! 32-byte hash in guest memory, and the host resolves it only against this
//! instance's own blob table, so a guessed hash reaches nothing. Every call
//! returns the host's status unchanged: a negative value means the table
//! neither pins nor holds the hash, and the caller decides what that means.

use aether_data::BlobHash;

use crate::wasm::raw;

/// A guest address or length as the `_p32` ABI's `u32`. Guest memory is
/// addressed by 32 bits on wasm32, so the conversion is exact there;
/// saturating keeps it total without a cast.
fn abi32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Take one hold on the blob `hash` names and return its length, or a
/// negative host status with no hold taken. [`drop_hold`] gives it back.
pub fn hold(hash: &BlobHash) -> i64 {
    // SAFETY: `hash_ptr` points at the 32 bytes of `hash`, which the borrow
    // keeps alive for the call; the host copies them out before returning.
    unsafe { raw::blob_hold(abi32(hash.as_bytes().as_ptr().addr())) }
}

/// Copy bytes of the blob `hash` names, from `offset`, into `dst`. Returns
/// how many the host copied (at most `dst.len()` and `MAX_READ_BYTES`; `0` at
/// or past the end), or a negative host status with `dst` untouched.
pub fn read(hash: &BlobHash, offset: u64, dst: &mut [u8]) -> i64 {
    // SAFETY: `hash_ptr` points at the 32 bytes of `hash` and `dst_ptr` at
    // `dst`, a live exclusive borrow of `dst.len()` bytes; the host writes at
    // most `dst_len` bytes into it and keeps no pointer past the call. A
    // `dst` longer than `u32::MAX` bytes cannot exist in wasm32 memory, and
    // saturating its length only lowers how much the host may write.
    unsafe {
        raw::blob_read(abi32(hash.as_bytes().as_ptr().addr()), offset, abi32(dst.as_mut_ptr().addr()), abi32(dst.len()))
    }
}

/// Give back one of this instance's holds on the blob `hash` names. The host
/// warns and does nothing for a hash the table holds no hold on.
pub fn drop_hold(hash: &BlobHash) {
    // SAFETY: `hash_ptr` points at the 32 bytes of `hash`, which the borrow
    // keeps alive for the call; the host copies them out before returning.
    unsafe { raw::blob_drop(abi32(hash.as_bytes().as_ptr().addr())) }
}
