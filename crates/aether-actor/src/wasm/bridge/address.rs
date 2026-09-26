//! ADR-0230 §3 (#6786) path-proof FFI bridge — the guest half of the
//! `resolve_path_p32` host fn.
//!
//! The guest hands the host a validated actor path (a slice in guest memory).
//! The host resolves it and proves the answer through the same crate-private
//! path the native `resolve_path` verb takes, encodes the outcome as one
//! [`__ResolvedPath`], and delivers the bytes through the guest's own
//! allocator as the packed `(ptr << 32) | len`, the way `asset_catalog_p32`
//! delivers its catalog. One delivered buffer carries every outcome, so the
//! host keeps no per-call status cell a second import would read back.
//!
//! This is the transport under `WasmCtx::resolve_path`, which mints the proof
//! from a `Live` answer.

use aether_data::{ActorPath, wire};
use alloc::string::String;

use super::abi32;
use super::asset::{take_delivered, unpack};
use crate::wasm::raw;

/// The host's answer to one `resolve_path_p32` call, wire-encoded into the
/// buffer it delivers. The ABI between the substrate's host fn and this SDK,
/// defined once here so the two sides cannot disagree on its shape.
///
/// Not part of the public API: a guest reaches it only as the
/// `Result` of `WasmCtx::resolve_path`, and the substrate names it only to
/// encode the answer.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum __ResolvedPath {
    /// The path names a `Live` route at `position`.
    Live {
        /// The route's position, which the SDK mints into a proof at once.
        position: u64,
    },
    /// The registry refused the path: an unknown or instanced root, an illegal
    /// or ambiguous segment, an over-cap path, or no route at the canonical path.
    Unresolved {
        /// The registry's refusal, rendered as text.
        detail: String,
    },
    /// The path names a route whose actor is not `Live` yet.
    NotLive {
        /// The canonical path the route was found at.
        canonical_path: String,
    },
}

/// Ask the host to prove `path` and decode its answer.
///
/// # Panics
///
/// Panics when the delivered bytes do not decode as a [`__ResolvedPath`]: the
/// host and this SDK disagree on the ABI, which no guest can recover from
/// (ADR-0063).
pub fn resolve_path(path: &ActorPath) -> __ResolvedPath {
    let text = path.as_str();
    // SAFETY: FFI import; the host copies the path out before returning and
    // always hands back a live `(ptr, len)`, or traps.
    let packed = unsafe { raw::resolve_path(abi32(text.as_ptr().addr()), abi32(text.len())) };
    let (ptr, len) = unpack(packed);
    // SAFETY: the return is always a live host-delivered buffer.
    let bytes = unsafe { take_delivered(ptr, len) };
    wire::from_bytes(&bytes).unwrap_or_else(|error| {
        panic!("aether-actor: resolve_path: the host's answer does not decode as __ResolvedPath: {error}")
    })
}
