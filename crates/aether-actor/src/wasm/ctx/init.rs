//! The `init` stage ctx — [`WasmInitCtx`], the resolve-only handle
//! `WasmActor::init` is handed. Mail is forbidden here; addressing and
//! sending begin at `wire`.

use core::cell::OnceCell;
use core::marker::PhantomData;

use aether_data::{Kind, MailboxId};

use crate::asset::{AssetCatalog, AssetInfo, AssetWindow};
use crate::mail::mailbox::{KindId, Mailbox, resolve, resolve_mailbox};
use crate::wasm::bridge::asset;
use alloc::vec::Vec;

/// Init-only capability handle for FFI guests. Resolved during
/// `WasmActor::init`; not available at runtime (the type split fences
/// "when can I resolve?" against "when can I send?" at compile time).
// The `Wasm` prefix carries the native/wasm split signal; bare `InitCtx` loses that.
#[allow(clippy::module_name_repetitions)]
pub struct WasmInitCtx<'a> {
    mailbox: u64,
    /// ADR-0163 §3 asset catalog, fetched lazily on the first
    /// [`AssetCatalog::assets`] call and cached for the ctx's life —
    /// `init` is inside the load window, so asset access is live here.
    catalog: OnceCell<Vec<AssetInfo>>,
    _borrow: PhantomData<&'a ()>,
}

impl WasmInitCtx<'_> {
    /// Not part of the public API; called only by [`crate::export!`].
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64) -> Self {
        Self { mailbox, catalog: OnceCell::new(), _borrow: PhantomData }
    }

    /// The component's own mailbox id — the value the substrate uses to
    /// address `receive` calls to this instance.
    #[must_use]
    pub fn mailbox_id(&self) -> MailboxId {
        MailboxId(self.mailbox)
    }

    /// Resolve a kind by its `const ID`. Pure compile-time construction
    /// under ADR-0030 Phase 2 — no host-fn round trip, never fails.
    #[must_use]
    pub const fn resolve<K: Kind>(&self) -> KindId<K> {
        resolve::<K>()
    }

    /// Resolve a mailbox by name and bind it to kind `K`, producing a
    /// typed [`Mailbox<K>`]. Pure compile-time construction; the returned
    /// token is pure addressing.
    #[must_use]
    pub const fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K> {
        resolve_mailbox::<K>(name)
    }

    // Issue 1987: the init ctx exposes no `actor()` / `resolve_actor()`
    // sender shortcut. A `WasmActorMailbox` is now a ctx-bound sender that
    // routes through the per-component inline registry, which the init
    // stage does not hold — and init is mail-forbidden anyway (the ctx
    // carries no send surface by design). Addressing + sending begin at
    // `wire`, where `WasmCtx` carries the registry.
}

impl AssetCatalog for WasmInitCtx<'_> {
    fn assets(&self) -> &[AssetInfo] {
        self.catalog.get_or_init(asset::fetch_catalog).as_slice()
    }
}

impl AssetWindow for WasmInitCtx<'_> {
    fn asset(&mut self, name: &str) -> Option<Vec<u8>> {
        asset::fetch_asset(name)
    }
}
