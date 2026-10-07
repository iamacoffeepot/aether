//! The `wire` stage ctx — [`WireCtx`], the wrapper the post-init `wire`
//! hook is handed (ADR-0250).

use core::cell::OnceCell;
use core::ops::{Deref, DerefMut};

use super::WasmCtx;
use crate::asset::{AssetInfo, Assets};
use crate::blob::guest;
use crate::model::ctx::Erased;
use crate::wasm::bridge::asset;
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
    /// Asset catalog, fetched lazily on the first [`Assets::assets`] call
    /// and cached for the ctx's life. A `wire` body that never enumerates
    /// assets pays no hostcall; one that only pulls by name (`asset(name)`)
    /// never touches this cell.
    catalog: OnceCell<Vec<AssetInfo>>,
}

impl<'ctx, 'a, A> WireCtx<'ctx, 'a, A> {
    /// Not part of the public API; called only by the `#[actor]` macro's
    /// `wire` forwarder, which wraps the [`WasmCtx`] it builds for the
    /// lifecycle call.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(inner: &'ctx mut WasmCtx<'a, A>) -> Self {
        Self { inner, catalog: OnceCell::new() }
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
        self.catalog.get_or_init(asset::fetch_catalog).as_slice()
    }

    fn asset(&mut self, name: &str) -> Option<Vec<u8>> {
        asset::fetch_asset(name)
    }

    fn asset_blob(&mut self, name: &str) -> Option<Blob> {
        guest::asset_blob(name)
    }
}
