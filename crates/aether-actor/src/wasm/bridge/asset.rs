//! ADR-0163 §3 (#3984) asset load-window FFI bridge — the guest half of
//! the `asset_fetch_p32` / `asset_catalog_p32` host fns.
//!
//! `wire` runs guest-side; asset bytes live host-side in the module file.
//! The bridge is the guest-initiated pull: it hands the host an asset name
//! (a slice in guest memory), the host looks it up in the load window,
//! allocates a buffer through the guest's own allocator, writes the bytes,
//! and returns the packed `(ptr << 32) | len`. The bridge copies that
//! buffer into an owned `Vec` and frees it symmetrically through the same
//! allocator — the host allocated it via `realloc_p32` (which routes to
//! [`realloc_bytes`]), so the guest frees it the same way.
//!
//! This is the transport backing [`crate::AssetWindow`] / [`crate::AssetCatalog`]
//! on the guest ctxs. A payload access after the window closed traps
//! host-side, so no reachable guest path silently returns empty.

// Wire-encode: `usize`/`u64` → `u32` narrowings forward `(ptr, len)` pairs to
// the wasm32 host-fn ABI (`_p32` convention, ADR-0024), same as `ctx.rs`.
#![allow(clippy::cast_possible_truncation)]

use core::slice::from_raw_parts;

use aether_data::wire;
use aether_kinds::AssetInfo;
use alloc::vec::Vec;

use crate::wasm::guest_alloc::realloc_bytes;
use crate::wasm::raw;

/// The host's "no asset by that name in the open window" sentinel — must
/// match `host_fns::ASSET_NOT_FOUND`. Distinct from any real `(ptr, len)`.
const ASSET_NOT_FOUND: u64 = u64::MAX;

/// Alignment the host allocated the delivery buffer with — must match
/// `host_fns::ASSET_ALLOC_ALIGN`. Byte data needs none, so `1` keeps the
/// guest's free layout-exact.
const ASSET_ALLOC_ALIGN: usize = 1;

/// Unpack the ADR-0163 host return `(ptr << 32) | len` into `(ptr, len)`.
fn unpack(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

/// Copy a host-delivered buffer at `(ptr, len)` into an owned `Vec`, then
/// free the buffer through the guest allocator. The host allocated it via
/// the guest's own `realloc_p32` with [`ASSET_ALLOC_ALIGN`], so freeing
/// with the same `(ptr, len, align)` triple is layout-exact.
///
/// # Safety
/// `ptr`/`len` must be a live buffer the host just delivered through
/// `deliver_bytes_to_guest` (this same allocator, this alignment). A
/// zero-length delivery carries no live buffer, so it is not freed.
unsafe fn take_delivered(ptr: u32, len: u32) -> Vec<u8> {
    if len == 0 {
        return Vec::new();
    }
    // SAFETY: the host wrote `len` valid bytes at `ptr` in this guest's
    // linear memory and the region is live until the free below.
    let bytes = unsafe { from_raw_parts(ptr as *const u8, len as usize) }.to_vec();
    // SAFETY: `(ptr, len, ASSET_ALLOC_ALIGN)` matches the host allocation
    // through this same allocator; `new_size = 0` frees it.
    unsafe {
        realloc_bytes(ptr as *mut u8, len as usize, ASSET_ALLOC_ALIGN, 0);
    }
    bytes
}

/// Pull an asset's bytes through the load window (ADR-0163). `None` when
/// the component carries no asset by that name (the host returns the
/// not-found sentinel). A call outside the window traps host-side, so this
/// never silently returns empty for a closed window.
#[must_use]
pub fn fetch_asset(name: &str) -> Option<Vec<u8>> {
    // SAFETY: FFI import; the host copies the name out before returning and
    // hands back either the not-found sentinel or a live `(ptr, len)`.
    let packed = unsafe { raw::asset_fetch(name.as_ptr().addr() as u32, name.len() as u32) };
    if packed == ASSET_NOT_FOUND {
        return None;
    }
    let (ptr, len) = unpack(packed);
    // SAFETY: a non-sentinel return is a live host-delivered buffer.
    Some(unsafe { take_delivered(ptr, len) })
}

/// The component's asset catalog, decoded from the host's wire-encoded
/// `Vec<AssetInfo>` (ADR-0163). Empty when the component carries no assets;
/// a decode failure yields an empty catalog rather than a trap — the
/// metadata surface is best-effort, unlike payload access.
#[must_use]
pub fn fetch_catalog() -> Vec<AssetInfo> {
    // SAFETY: FFI import returning a live `(ptr, len)` to wire bytes.
    let packed = unsafe { raw::asset_catalog() };
    let (ptr, len) = unpack(packed);
    // SAFETY: the return is always a live host-delivered buffer.
    let bytes = unsafe { take_delivered(ptr, len) };
    wire::from_bytes::<Vec<AssetInfo>>(&bytes).unwrap_or_default()
}
