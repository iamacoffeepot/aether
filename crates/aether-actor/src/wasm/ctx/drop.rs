//! The dehydrate-stage ctx — [`WasmDropCtx`], the narrowed handle the
//! `on_dehydrate` save hook is handed, and [`CapturedState`], the in-memory
//! deposit the ADR-0114 §5 composite dehydrate collects into.

use core::marker::PhantomData;

use aether_data::{Kind, MailboxId, mailbox_id_from_name};

use crate::model::ctx::mail_sender::MailSender;
use crate::model::ctx::persistence::Persistence;
use crate::model::{Addressable, CallerAddressable, CallerScope, CallerScoped, HandlesKind, Singleton};
use crate::wasm::bridge::{mail, persist};
use alloc::vec::Vec;

/// A `save_state` deposit captured in memory instead of forwarded to the
/// host `save_state` import (ADR-0114 §5). The dehydrate compose hands the
/// parent and each inline child a [`WasmDropCtx`] bound to one of these so
/// it can collect every saved blob and pack them into a single composite,
/// then call the real host `save_state` once.
#[derive(Default)]
pub struct CapturedState {
    /// The most recent `(version, bytes)` the hook saved. `None` until the
    /// hook calls `save_state`; the last call wins (mirroring the host's
    /// single-`Option<StateBundle>` overwrite contract).
    saved: Option<(u32, Vec<u8>)>,
}

impl CapturedState {
    /// Take the captured `(version, bytes)`, leaving the slot empty.
    #[must_use]
    pub fn take(&mut self) -> Option<(u32, Vec<u8>)> {
        self.saved.take()
    }
}

/// Narrowed capability handle for the `on_dehydrate` save hook.
/// Outbound mail still works through [`MailSender`]; the reply / resolve
/// surfaces are intentionally absent.
// The `Wasm` prefix carries the native/wasm split signal; bare `DropCtx` loses that.
#[allow(clippy::module_name_repetitions)]
pub struct WasmDropCtx<'a> {
    /// The actor's own mailbox id (its lineage carry), so a buffered
    /// `send` resolves the receiver through `R::resolve(self.mailbox)`
    /// like every other ctx (ADR-0099 §5).
    mailbox: u64,
    /// The actor's logical parent mailbox. Legacy guests and cluster roots
    /// without parent metadata use [`MailboxId::NONE`].
    parent: u64,
    /// ADR-0114 §5: when `Some`, `save_state` records into this buffer
    /// instead of the host import, so the dehydrate compose can collect
    /// the parent's and each child's bundle and pack one composite. `None`
    /// is the ordinary path — `save_state` forwards to the host.
    capture: Option<&'a mut CapturedState>,
    _borrow: PhantomData<&'a ()>,
}

impl<'a> WasmDropCtx<'a> {
    /// Not part of the public API; called only by [`crate::export!`].
    /// Forwards `save_state` to the host import.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64, parent: u64) -> Self {
        Self { mailbox, parent, capture: None, _borrow: PhantomData }
    }

    /// Not part of the public API; called only by the dehydrate compose
    /// (`crate::wasm::inline::compose`). `save_state` records into `capture`
    /// rather than the host import, so the composite can be assembled
    /// before a single real host `save_state`.
    #[doc(hidden)]
    #[must_use]
    pub(crate) fn __new_capturing(mailbox: u64, parent: u64, capture: &'a mut CapturedState) -> Self {
        Self { mailbox, parent, capture: Some(capture), _borrow: PhantomData }
    }

    fn scope_mailbox(&self, scope: CallerScope) -> u64 {
        scope.select(MailboxId(self.mailbox), MailboxId(self.parent)).0
    }

    /// Deposit a migration bundle. Mirrors [`Persistence::save_state`].
    /// When this ctx was built capturing (ADR-0114 §5), the deposit is
    /// recorded in the capture buffer; otherwise it forwards to the host.
    ///
    /// # Panics
    /// Panics if the host `save_state` import returns non-zero — fail-fast
    /// per ADR-0063: the persistence bridge is part of the substrate
    /// contract and a failure here means the runtime is in an
    /// unrecoverable state. (The capturing path cannot fail.)
    pub fn save_state(&mut self, version: u32, bytes: &[u8]) {
        if let Some(capture) = self.capture.as_mut() {
            capture.saved = Some((version, bytes.to_vec()));
            return;
        }
        let status = persist::save_state(version, bytes);
        assert_eq!(status, 0, "aether-actor: save_state failed (status {status})");
    }

    /// Persist a typed kind value. Mirrors
    /// [`Persistence::save_state_kind`].
    pub fn save_state_kind<K>(&mut self, version: u32, value: &K)
    where
        K: Kind + aether_data::Schema + serde::Serialize,
    {
        <Self as Persistence>::save_state_kind::<K>(self, version, value);
    }
}

impl MailSender for WasmDropCtx<'_> {
    //noinspection DuplicatedCode
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        mail::send_mail(
            R::resolve(self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0,
            K::ID.0,
            &bytes,
            1,
            false,
            self.mailbox,
        );
    }

    //noinspection DuplicatedCode
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        mail::send_mail(
            R::resolve(self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0,
            K::ID.0,
            bytes,
            payloads.len() as u32,
            false,
            self.mailbox,
        );
    }

    //noinspection DuplicatedCode
    // Runtime-name send escape hatch (the `MailSender::send_to_named` contract):
    // the recipient name is supplied at runtime, no compile-time `R` to resolve.
    #[allow(clippy::disallowed_methods)]
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        mail::send_mail(mailbox_id_from_name(name).0, K::ID.0, &bytes, 1, false, self.mailbox);
    }

    fn prev_correlation(&self) -> u64 {
        mail::prev_correlation()
    }

    //noinspection DuplicatedCode
    fn send_detached<R, K>(&mut self, payload: &K)
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        mail::send_mail(
            R::resolve(self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0,
            K::ID.0,
            &bytes,
            1,
            true,
            self.mailbox,
        );
    }

    //noinspection DuplicatedCode
    // Runtime-name detached escape hatch — the `send_to_named` counterpart.
    #[allow(clippy::disallowed_methods)]
    fn send_detached_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        mail::send_mail(mailbox_id_from_name(name).0, K::ID.0, &bytes, 1, true, self.mailbox);
    }

    //noinspection DuplicatedCode
    // By-id detached send — the by-name body with the caller's id.
    fn send_detached_to<K: Kind>(&mut self, id: MailboxId, payload: &K) {
        let bytes = payload.encode_into_bytes();
        mail::send_mail(id.0, K::ID.0, &bytes, 1, true, self.mailbox);
    }
}

impl Persistence for WasmDropCtx<'_> {
    fn save_state(&mut self, version: u32, bytes: &[u8]) {
        // Route through the inherent `save_state` so the ADR-0114 §5
        // capture path applies — the generated `on_dehydrate` hooks reach
        // the bundle through `Persistence::save_state_kind`, which calls
        // this trait method, so a capturing ctx must intercept here too.
        WasmDropCtx::save_state(self, version, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_ctx_preserves_parent_scope() {
        let ctx = WasmDropCtx::__new(0x4d02, 0x4d01);

        assert_eq!(ctx.scope_mailbox(CallerScope::Root), MailboxId::NONE.0);
        assert_eq!(ctx.scope_mailbox(CallerScope::Current), 0x4d02);
        assert_eq!(ctx.scope_mailbox(CallerScope::Parent), 0x4d01);
    }
}
