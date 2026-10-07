//! The `wire` stage ctx — [`WireCtx`], the wrapper the post-init `wire`
//! hook is handed (ADR-0250).

use core::ops::{Deref, DerefMut};

use super::WasmCtx;
use crate::asset::{AssetInfo, Assets};
use crate::blob::guest;
use crate::model::ctx::Erased;
use aether_data::Blob;
use alloc::vec::Vec;

/// The context `wire` receives. A thin borrow-wrapper around the post-init
/// [`WasmCtx`] that `Deref`s to it, so every send / subscribe / resolve verb
/// a `wire` body already uses keeps working unchanged through the deref. It
/// forwards the [`Assets`] surface to the same host calls the other ctxs use,
/// served from the instance's own module in every hook (ADR-0250). Kept as a
/// type so every `wire` signature in the tree keeps working.
///
/// Two lifetimes: `'ctx` is the borrow of the underlying ctx the FFI
/// membrane owns for the call, `'a` is that ctx's own lifetime. The
/// `#[actor]` macro constructs this around the `WasmCtx` it already builds
/// for `wire`, so authors only ever name it as `&mut WireCtx<'_, '_>`.
#[allow(clippy::module_name_repetitions)]
pub struct WireCtx<'ctx, 'a, A = Erased> {
    inner: &'ctx mut WasmCtx<'a, A>,
}

impl<'ctx, 'a, A> WireCtx<'ctx, 'a, A> {
    /// Not part of the public API; called only by the `#[actor]` macro's
    /// `wire` forwarder, which wraps the [`WasmCtx`] it builds for the
    /// lifecycle call.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(inner: &'ctx mut WasmCtx<'a, A>) -> Self {
        Self { inner }
    }
}

impl<'a, A> Deref for WireCtx<'_, 'a, A> {
    type Target = WasmCtx<'a, A>;
    fn deref(&self) -> &WasmCtx<'a, A> {
        self.inner
    }
}

impl<'a, A> DerefMut for WireCtx<'_, 'a, A> {
    fn deref_mut(&mut self) -> &mut WasmCtx<'a, A> {
        self.inner
    }
}

impl<A> Assets for WireCtx<'_, '_, A> {
    fn assets(&self) -> &[AssetInfo] {
        self.inner.inline.assets()
    }

    fn asset(&mut self, name: &str) -> Option<Vec<u8>> {
        guest::asset(name)
    }

    fn asset_blob(&mut self, name: &str) -> Option<Blob> {
        guest::asset_blob(name)
    }
}
