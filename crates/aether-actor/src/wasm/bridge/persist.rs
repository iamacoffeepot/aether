// Wire-encode: `usize → u32` narrowings forward `(ptr, len)` pairs
// to the wasm32 host-fn ABI (`_p32` convention, ADR-0024).
#![allow(clippy::cast_possible_truncation)]

//! Migration-bundle FFI bridge — free function in a `pub(crate)` module.
//!
//! `save_state` is the ADR-0016 deposit hook called inside `on_dehydrate`
//! to hand a typed bundle to the replacement instance. Persistence is
//! conceptually distinct from mail (it's a one-shot byte deposit, not a
//! routed envelope), so it lives in its own module rather than alongside
//! [`super::mail`].
//!
//! Native actors do not have a `replace_component`-style hot reload
//! path — only wasm components do. The native ctx structs deliberately
//! do not impl [`crate::actor::ctx::Persistence`].

use crate::wasm::raw;

/// Deposit a migration bundle for the substrate to hand to the
/// replacement instance via `on_rehydrate` (ADR-0016). Bytes are
/// copied into a substrate-owned buffer immediately, so the
/// caller is free to drop the slice on return.
///
/// Returns `0` on success; non-zero on substrate rejection
/// (today: 1 MiB cap exceeded or internal OOB — both component
/// bugs). Meaningful inside `on_dehydrate`, where the substrate
/// hands the bundle to the replacement instance via `on_rehydrate`.
#[must_use]
pub fn save_state(version: u32, bytes: &[u8]) -> u32 {
    // SAFETY: forwards to `raw::save_state`, whose ABI is documented
    // at the import site in `ffi/raw.rs`. The `(ptr, len)` pair is
    // derived from the `&[u8]` slice we just received, which the
    // borrow checker proves is valid for `bytes.len()` bytes for
    // the duration of the call; the substrate copies the bundle
    // out of guest memory before returning.
    unsafe { raw::save_state(version, bytes.as_ptr().addr() as u32, bytes.len() as u32) }
}
