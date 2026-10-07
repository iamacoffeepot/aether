//! The `init` stage ctx — [`WasmInitCtx`], the resolve-only handle
//! `WasmActor::init` is handed. Mail is forbidden here; addressing and
//! sending begin at `wire`.

use core::cell::OnceCell;
use core::marker::PhantomData;

use crate::asset::{AssetInfo, Assets};
use crate::blob::guest;
use crate::wasm::bridge::asset;
use aether_data::Blob;
use alloc::vec::Vec;

/// Init-only capability handle for FFI guests. Resolved during
/// `WasmActor::init`; not available at runtime (the type split fences
/// "when can I resolve?" against "when can I send?" at compile time).
// The `Wasm` prefix carries the native/wasm split signal; bare `InitCtx` loses that.
#[allow(clippy::module_name_repetitions)]
pub struct WasmInitCtx<'a> {
    /// Asset catalog, fetched lazily on the first [`Assets::assets`] call
    /// and cached for the ctx's life — served from the instance's own module
    /// in every hook (ADR-0250).
    catalog: OnceCell<Vec<AssetInfo>>,
    _borrow: PhantomData<&'a ()>,
}

impl WasmInitCtx<'_> {
    /// Not part of the public API; called only by [`crate::export!`].
    #[doc(hidden)]
    #[must_use]
    pub fn __new() -> Self {
        Self { catalog: OnceCell::new(), _borrow: PhantomData }
    }

    // Issue 1987: the init ctx exposes no send verbs. Every send routes
    // through the per-component inline registry, which the init stage does
    // not hold — and init is mail-forbidden anyway (the ctx carries no send
    // surface by design). Addressing + sending begin at `wire`, where
    // `WasmCtx` carries the registry.
}

impl Assets for WasmInitCtx<'_> {
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
