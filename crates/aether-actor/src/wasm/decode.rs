//! What a guest's decode may consult: `guest_ctx`, the context every guest
//! decode starts from, and [`decode_config`], a guest's decode of its
//! `Config` (ADR-0231 §3, #7501).
//!
//! A `ProtocolPath<P>` proves its claim at decode by asking the context
//! whether the route standing at the path published every row of `P`. On
//! wasm32 the context borrows `GuestRoutes`, whose answer comes from the
//! host's registry through the `route_rows_p32` host fn — the read a native
//! decode makes — so a guest decodes a protocol path wherever a native actor
//! can: in mail, in its config, in an inline child's config, and in saved
//! state. `GuestRoutes` is the routes twin of the blob layer's
//! `GuestResolver`: a unit type the context borrows.
//!
//! The context's routes are read only while a `ProtocolPath` field decodes
//! (`DecodeCtx::prove_route_covers` is its one reader), so a kind with no
//! protocol path makes no host call: it pays the pointer store that builds
//! the context and nothing else.
//!
//! The host build of the SDK has no engine to ask, so it keeps the empty
//! context, as it keeps no blob resolver, and a `ProtocolPath` refuses
//! `ProtocolPathUnchecked` there.

use alloc::format;
#[cfg(target_arch = "wasm32")]
use alloc::sync::Arc;

use aether_data::Kind;
use aether_data::wire::DecodeCtx;
#[cfg(target_arch = "wasm32")]
use aether_data::wire::PublishedRoutes;
#[cfg(target_arch = "wasm32")]
use aether_data::{ErasedActorPath, KindId, ReplyContract};

use super::ActorInitError;
#[cfg(target_arch = "wasm32")]
use super::bridge::address;

/// The engine's published routes, as a guest reads them: one
/// `route_rows_p32` host call per path asked for. Nothing is kept between
/// calls (ADR-0230 §3: no answer is kept per route).
#[cfg(target_arch = "wasm32")]
struct GuestRoutes;

#[cfg(target_arch = "wasm32")]
impl PublishedRoutes for GuestRoutes {
    fn published_rows(&self, path: &ErasedActorPath) -> Option<Arc<[(KindId, ReplyContract)]>> {
        address::route_rows(path).rows.map(Arc::from)
    }
}

/// The context every guest decode starts from: it proves a `ProtocolPath`
/// against the engine's published routes. A caller adds the hooks its decode
/// also needs, the blob resolver for mail and the held ledger for saved
/// state.
#[cfg(target_arch = "wasm32")]
pub(crate) fn guest_ctx<'a>() -> DecodeCtx<'a> {
    DecodeCtx::empty().routes(&GuestRoutes)
}

/// The context every guest decode starts from. The host build of the SDK has
/// no engine to ask, so it proves no route.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn guest_ctx<'a>() -> DecodeCtx<'a> {
    DecodeCtx::empty()
}

/// A guest's decode of the `Config` of the actor published as `actor`, from
/// the bytes its load, spawn, or republish carried.
///
/// Not part of the public API: `export!`'s init shims and the inline spawn
/// paths are the callers.
///
/// # Errors
///
/// An [`ActorInitError`] naming the actor, the config kind, and the decode's
/// own refusal, so a `ProtocolPath` that does not prove says which path and
/// why.
#[doc(hidden)]
pub fn decode_config<C: Kind>(actor: &str, bytes: &[u8]) -> Result<C, ActorInitError> {
    C::decode_with(bytes, &mut guest_ctx())
        .map_err(|error| ActorInitError::new(format!("{actor}: config `{}` did not decode: {error}", C::NAME)))
}
