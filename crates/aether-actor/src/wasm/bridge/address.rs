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
//!
//! Its sibling is the guest half of the `published_rows_p32` host fn (ADR-0231
//! §4): the guest hands the host the position of a reference it already holds,
//! and the host answers the rows that route published while it is `Live` as
//! one [`__PublishedRows`], delivered the same way. This is the transport
//! under `WasmCtx::cast`, which applies the protocol's `admits` rule to the
//! rows and mints the typed reference itself.
//!
//! A third sibling is the guest half of the `live_route_p32` host fn
//! (ADR-0230 §3, #7205): the guest hands the host a typed path's text, and
//! the host answers the position of the `Live` route standing under exactly
//! that canonical name, or none, as one [`__LiveRoute`], delivered the same
//! way. This is the transport under `WasmCtx::resolve`, which mints the
//! proof itself from a `Some` answer.
//!
//! A fourth sibling is the guest half of the `route_rows_p32` host fn
//! (ADR-0231 §3, #7501): the guest hands the host a typed path's text, and
//! the host answers the rows of the `Live` or `Dropped` route standing under
//! exactly that canonical name, or none, as one [`__PublishedRows`],
//! delivered the same way. This is the transport under a guest's decode of a
//! `ProtocolPath<P>`, which checks the protocol's rows against the answer.

use aether_data::{ErasedActorPath, KindId, ReplyContract, wire};
use alloc::string::String;
use alloc::vec::Vec;

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
pub fn resolve_path(path: &ErasedActorPath) -> __ResolvedPath {
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

/// The host's answer to one `published_rows_p32` call, wire-encoded into the
/// buffer it delivers. The ABI between the substrate's host fn and this SDK,
/// defined once here beside [`__ResolvedPath`] so the two sides cannot
/// disagree on its shape.
///
/// The answer to a `route_rows_p32` call too, which reads a different set of
/// routes.
///
/// Not part of the public API: a guest reaches it only as the `Option` of
/// `WasmCtx::cast` or inside a `ProtocolPath` decode, and the substrate
/// names it only to encode the answer.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct __PublishedRows {
    /// The rows the route published, or `None` when the read that produced
    /// the answer finds no route. `published_rows_p32` finds a `Live` route
    /// only; `route_rows_p32` finds a `Live` or `Dropped` one under exactly
    /// the path's canonical name.
    pub rows: Option<Vec<(KindId, ReplyContract)>>,
}

/// Ask the host for the rows the route at `position` published while it is
/// `Live`, and decode its answer.
///
/// # Panics
///
/// Panics when the delivered bytes do not decode as a [`__PublishedRows`]: the
/// host and this SDK disagree on the ABI, which no guest can recover from
/// (ADR-0063).
pub fn published_rows(position: u64) -> __PublishedRows {
    // SAFETY: FFI import; the host always hands back a live `(ptr, len)`, or
    // traps.
    let packed = unsafe { raw::published_rows(position) };
    let (ptr, len) = unpack(packed);
    // SAFETY: the return is always a live host-delivered buffer.
    let bytes = unsafe { take_delivered(ptr, len) };
    wire::from_bytes(&bytes).unwrap_or_else(|error| {
        panic!("aether-actor: published_rows: the host's answer does not decode as __PublishedRows: {error}")
    })
}

/// The host's answer to one `live_route_p32` call, wire-encoded into the
/// buffer it delivers. The ABI between the substrate's host fn and this SDK,
/// defined once here beside [`__ResolvedPath`] so the two sides cannot
/// disagree on its shape.
///
/// Not part of the public API: a guest reaches it only as the `Result` of
/// `WasmCtx::resolve`, and the substrate names it only to encode the answer.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct __LiveRoute {
    /// The position of the `Live` route standing under exactly the path's
    /// canonical name, or `None` for `Starting`, `Dropped`, never
    /// registered, or a fold collision with a different canonical name.
    pub position: Option<u64>,
}

/// Ask the host for the position of the `Live` route standing under exactly
/// `path`'s canonical name, and decode its answer.
///
/// This is the transport under `WasmCtx::resolve`, which mints the
/// `ActorRef<R>` from a `Some` answer — the guest twin of the native
/// `NativeCtx::resolve` over a `ProtocolPath<P>`, both reading
/// `Registry::live_route`.
///
/// # Panics
///
/// Panics when the delivered bytes do not decode as a [`__LiveRoute`]: the
/// host and this SDK disagree on the ABI, which no guest can recover from
/// (ADR-0063).
pub fn live_route(path: &ErasedActorPath) -> __LiveRoute {
    let text = path.as_str();
    // SAFETY: FFI import; the host copies the path out before returning and
    // always hands back a live `(ptr, len)`, or traps.
    let packed = unsafe { raw::live_route(abi32(text.as_ptr().addr()), abi32(text.len())) };
    let (ptr, len) = unpack(packed);
    // SAFETY: the return is always a live host-delivered buffer.
    let bytes = unsafe { take_delivered(ptr, len) };
    wire::from_bytes(&bytes).unwrap_or_else(|error| {
        panic!("aether-actor: live_route: the host's answer does not decode as __LiveRoute: {error}")
    })
}

/// Ask the host for the rows of the `Live` or `Dropped` route standing under
/// exactly `path`'s canonical name, and decode its answer.
///
/// This is the transport under a guest's decode of a `ProtocolPath<P>`
/// (ADR-0231 §3): the guest twin of `impl PublishedRoutes for Registry`,
/// both reading `Registry::route_rows`. wasm32-only: its one caller is.
///
/// # Panics
///
/// Panics when the delivered bytes do not decode as a [`__PublishedRows`]: the
/// host and this SDK disagree on the ABI, which no guest can recover from
/// (ADR-0063).
#[cfg(target_arch = "wasm32")]
pub fn route_rows(path: &ErasedActorPath) -> __PublishedRows {
    let text = path.as_str();
    // SAFETY: FFI import; the host copies the path out before returning and
    // always hands back a live `(ptr, len)`, or traps.
    let packed = unsafe { raw::route_rows(abi32(text.as_ptr().addr()), abi32(text.len())) };
    let (ptr, len) = unpack(packed);
    // SAFETY: the return is always a live host-delivered buffer.
    let bytes = unsafe { take_delivered(ptr, len) };
    wire::from_bytes(&bytes).unwrap_or_else(|error| {
        panic!("aether-actor: route_rows: the host's answer does not decode as __PublishedRows: {error}")
    })
}
