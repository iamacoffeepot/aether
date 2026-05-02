//! Shared `wait_reply` helper used by every synchronous SDK wrapper
//! (`io::*_sync`, `handle::sync_*`, `net::fetch_blocking`). Each
//! family carries its own error enum (`SyncIoError`, `SyncHandleError`,
//! `SyncNetError`), so the helper is generic over both the reply
//! kind `K` and an error type that implements [`WaitError`].
//!
//! ADR-0042: the substrate echoes the request's correlation id on the
//! reply; the host fn `wait_reply` parks the component thread until a
//! mail of kind `K` with the matching correlation arrives (or the
//! timeout elapses). The three sentinel return codes (`-1` / `-2` /
//! `-3`) map onto the [`WaitError`] constructors.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use aether_data::Kind;

use crate::raw;

/// Error contract every sync wrapper's error enum needs to implement
/// so [`wait_reply`] can construct the four post-FFI failure modes
/// without knowing which family it's serving.
pub(crate) trait WaitError {
    fn timeout() -> Self;
    fn buffer_too_small() -> Self;
    fn cancelled() -> Self;
    fn decode(message: String) -> Self;
}

/// Allocate a `capacity`-sized scratch buffer in guest memory, park
/// on `raw::wait_reply` for a mail of kind `K` with the given
/// `expected_correlation`, and postcard-decode the written bytes.
/// Replaces the per-family duplicates that previously lived in
/// `io.rs`, `handle.rs`, and inline in `net::fetch_blocking`.
pub(crate) fn wait_reply<K, E>(
    timeout_ms: u32,
    capacity: usize,
    expected_correlation: u64,
) -> Result<K, E>
where
    K: Kind + serde::de::DeserializeOwned,
    E: WaitError,
{
    let mut buf: Vec<u8> = vec![0u8; capacity];
    let rc = unsafe {
        raw::wait_reply(
            K::ID.0,
            buf.as_mut_ptr().addr() as u32,
            buf.len() as u32,
            timeout_ms,
            expected_correlation,
        )
    };
    match rc {
        -1 => Err(E::timeout()),
        -2 => Err(E::buffer_too_small()),
        -3 => Err(E::cancelled()),
        n if n >= 0 => {
            let len = n as usize;
            postcard::from_bytes(&buf[..len]).map_err(|e| E::decode(alloc::format!("{e}")))
        }
        _ => Err(E::decode(alloc::format!(
            "unexpected wait_reply return: {rc}"
        ))),
    }
}
