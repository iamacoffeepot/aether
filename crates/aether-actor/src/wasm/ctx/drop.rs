//! The dehydrate-stage ctx — [`WasmDropCtx`], the narrowed handle the
//! `on_dehydrate` save hook is handed, and [`CapturedState`], the in-memory
//! deposit the ADR-0114 §5 composite dehydrate collects into.

use core::marker::PhantomData;

use aether_data::{ActorMail, Kind};

use crate::blob::guest::encode_guest;
use crate::model::ctx::mail_sender::MailSender;
use crate::model::ctx::persistence::Persistence;
use crate::model::Anyone;
use crate::reference::Target;
use crate::wasm::bridge::{mail, persist};
use crate::wasm::inline::Registry;
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
/// Outbound mail goes only through a proof, by
/// [`MailSender::send_detached_to`]; the typed-send, reply, and resolve
/// surfaces are intentionally absent.
// The `Wasm` prefix carries the native/wasm split signal; bare `DropCtx` loses that.
#[allow(clippy::module_name_repetitions)]
pub struct WasmDropCtx<'a> {
    /// The actor's own mailbox id, stamped as the "from" half of every send
    /// (issue 1987).
    mailbox: u64,
    /// ADR-0114 §5: when `Some`, `save_state` records into this buffer
    /// instead of the host import, so the dehydrate compose can collect
    /// the parent's and each child's bundle and pack one composite. `None`
    /// is the ordinary path — `save_state` forwards to the host.
    capture: Option<&'a mut CapturedState>,
    /// The per-component registry, whose held-reply ledger
    /// [`Self::save_state_kind`] grants to the state encode, so a `Held` in
    /// the saved state parks instead of refusing (ADR-0243 §6).
    inline: &'a Registry,
    _borrow: PhantomData<&'a ()>,
}

impl<'a> WasmDropCtx<'a> {
    /// Not part of the public API; called only by [`crate::export!`].
    /// Forwards `save_state` to the host import.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64, inline: &'a Registry) -> Self {
        Self { mailbox, capture: None, inline, _borrow: PhantomData }
    }

    /// Not part of the public API; called only by the dehydrate compose
    /// (`crate::wasm::inline::compose`). `save_state` records into `capture`
    /// rather than the host import, so the composite can be assembled
    /// before a single real host `save_state`.
    #[doc(hidden)]
    #[must_use]
    pub(crate) fn __new_capturing(mailbox: u64, capture: &'a mut CapturedState, inline: &'a Registry) -> Self {
        Self { mailbox, capture: Some(capture), inline, _borrow: PhantomData }
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
    /// [`Persistence::save_state_kind`], and grants the held-reply ledger:
    /// each [`Held`](crate::Held) in `value` parks as saved, for the
    /// replacement's [`PriorState::decode_kind`](crate::PriorState::decode_kind)
    /// to claim back (ADR-0243 §6).
    ///
    /// # Panics
    ///
    /// When `value` does not encode: a length past the `u32` ceiling, or a
    /// `Held` this instance does not hold live.
    pub fn save_state_kind<K: Kind>(&mut self, version: u32, value: &K) {
        let bytes = self.inline.encode_saved_state(value);
        self.save_state(version, &bytes);
    }
}

impl MailSender for WasmDropCtx<'_> {
    fn prev_correlation(&self) -> u64 {
        mail::prev_correlation()
    }

    // By-id detached send, stamping the caller's id as the sender.
    // The encoded values it names by hash stay alive across the host call,
    // whose resolve on send attaches their entries (ADR-0238 decision 3).
    fn send_detached_to<K: ActorMail, I>(&mut self, target: impl Target<K, I, Sender = Anyone>, payload: &K) {
        let encoded = encode_guest(payload);
        mail::send_mail(target.erased().id().0, K::ID.0, &encoded.bytes, 1, true, self.mailbox);
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

    // The generated `on_dehydrate` saves `type State` through this trait
    // method, so the ledger-granting inherent form must apply here too.
    fn save_state_kind<K: Kind>(&mut self, version: u32, value: &K) {
        WasmDropCtx::save_state_kind(self, version, value);
    }
}
