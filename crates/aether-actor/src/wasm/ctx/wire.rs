//! The `wire` stage ctx — [`WireCtx`], the window-bearing wrapper the
//! post-init `wire` hook is handed (ADR-0163 §3).

use core::cell::OnceCell;
use core::ops::{Deref, DerefMut};

use super::WasmCtx;
use crate::asset::{AssetCatalog, AssetInfo, AssetWindow};
use crate::wasm::bridge::asset;
use alloc::vec::Vec;

/// The window-bearing context `wire` receives (ADR-0163 §3). A thin
/// borrow-wrapper around the post-init [`WasmCtx`] that `Deref`s to it, so
/// every send / subscribe / resolve verb a `wire` body already uses keeps
/// working unchanged through the deref. Its reason to exist is the asset
/// load window: `WireCtx` is the ctx type through which an actor reads the
/// bytes it ships in `aether.asset.<path>` custom sections
/// ([`crate::AssetWindow`]), and taking it — rather than a bare
/// [`WasmCtx`] — is what makes "fetch an asset after the window closed" a
/// compile error (a handler is handed a [`WasmCtx`], which carries no
/// asset surface).
///
/// This slice lands the type and its `wire`-signature sweep; the guest
/// transport that fills the window across the FFI (delivering the catalog
/// and serving `asset(name)` inside a wasm guest) is the named follow-up.
/// Until it lands the wrapper adds no methods of its own — the breaking
/// signature change is taken now, while no bundle actor yet depends on the
/// payload path, so a later slice fills the window without re-breaking
/// every `wire`.
///
/// Two lifetimes: `'ctx` is the borrow of the underlying ctx the FFI
/// membrane owns for the call, `'a` is that ctx's own lifetime. The
/// `#[actor]` macro constructs this around the `WasmCtx` it already builds
/// for `wire`, so authors only ever name it as `&mut WireCtx<'_, '_>`.
// The `Wire` prefix carries the load-window signal; a bare `Ctx` would lose it.
#[allow(clippy::module_name_repetitions)]
pub struct WireCtx<'ctx, 'a> {
    inner: &'ctx mut WasmCtx<'a>,
    /// ADR-0163 §3 asset catalog, fetched lazily on the first
    /// [`AssetCatalog::assets`] call and cached for the ctx's life. A
    /// `wire` body that never enumerates assets pays no hostcall; one that
    /// only pulls by name (`asset(name)`) never touches this cell.
    catalog: OnceCell<Vec<AssetInfo>>,
}

impl<'ctx, 'a> WireCtx<'ctx, 'a> {
    /// Not part of the public API; called only by the `#[actor]` macro's
    /// `wire` forwarder, which wraps the [`WasmCtx`] it builds for the
    /// lifecycle call.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(inner: &'ctx mut WasmCtx<'a>) -> Self {
        Self { inner, catalog: OnceCell::new() }
    }
}

impl<'a> Deref for WireCtx<'_, 'a> {
    type Target = WasmCtx<'a>;
    fn deref(&self) -> &WasmCtx<'a> {
        self.inner
    }
}

impl<'a> DerefMut for WireCtx<'_, 'a> {
    fn deref_mut(&mut self) -> &mut WasmCtx<'a> {
        self.inner
    }
}

impl AssetCatalog for WireCtx<'_, '_> {
    fn assets(&self) -> &[AssetInfo] {
        self.catalog.get_or_init(asset::fetch_catalog).as_slice()
    }
}

impl AssetWindow for WireCtx<'_, '_> {
    fn asset(&mut self, name: &str) -> Option<Vec<u8>> {
        asset::fetch_asset(name)
    }
}
