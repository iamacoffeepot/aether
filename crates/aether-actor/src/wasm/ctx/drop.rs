//! The dehydrate-stage ctx — [`WasmDropCtx`], the narrowed handle the
//! `on_dehydrate` save hook is handed, and [`CapturedState`], the in-memory
//! deposit the ADR-0114 §5 composite dehydrate collects into.

use alloc::format;
use alloc::vec::Vec;
use core::marker::PhantomData;

use aether_data::Kind;

use crate::model::ctx::persistence::Persistence;
use crate::wasm::ActorInitError;
use crate::wasm::bridge::persist;
use crate::wasm::inline::Registry;

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

/// Narrowed capability handle for the `on_dehydrate` save hook. It saves and
/// does not send (ADR-0249 §8): the old guest keeps running when its republish
/// aborts, and mail it sent from the save hook could not be taken back. The
/// hook that announces a departure is `unwire`. The typed-send, reply, and
/// resolve surfaces are absent for the same reason.
// The `Wasm` prefix carries the native/wasm split signal; bare `DropCtx` loses that.
#[allow(clippy::module_name_repetitions)]
pub struct WasmDropCtx<'a> {
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
    pub fn __new(inline: &'a Registry) -> Self {
        Self { capture: None, inline, _borrow: PhantomData }
    }

    /// Not part of the public API; called only by the dehydrate compose
    /// (`crate::wasm::inline::compose`). `save_state` records into `capture`
    /// rather than the host import, so the composite can be assembled
    /// before a single real host `save_state`.
    #[doc(hidden)]
    #[must_use]
    pub(crate) fn __new_capturing(capture: &'a mut CapturedState, inline: &'a Registry) -> Self {
        Self { capture: Some(capture), inline, _borrow: PhantomData }
    }

    /// Deposit a migration bundle. Mirrors [`Persistence::save_state`].
    /// When this ctx was built capturing (ADR-0114 §5), the deposit is
    /// recorded in the capture buffer; otherwise it forwards to the host.
    ///
    /// # Errors
    /// When the host `save_state` import refuses the bundle, with the status
    /// it returned: the bundle is past the size cap, the host has no memory
    /// for it, or the range is outside guest memory. The capturing path
    /// cannot fail.
    pub fn save_state(&mut self, version: u32, bytes: &[u8]) -> Result<(), ActorInitError> {
        if let Some(capture) = self.capture.as_mut() {
            capture.saved = Some((version, bytes.to_vec()));
            return Ok(());
        }

        let status = persist::save_state(version, bytes);
        if status == 0 {
            return Ok(());
        }
        Err(ActorInitError::from(format!("the host refused the saved state (status {status})")))
    }

    /// Persist a typed kind value. Mirrors
    /// [`Persistence::save_state_kind`], and grants the held-reply ledger:
    /// each [`Held`](crate::Held) in `value` parks as saved, for the
    /// replacement's [`PriorState::decode_kind`](crate::PriorState::decode_kind)
    /// to claim back (ADR-0243 §6).
    ///
    /// # Errors
    /// When `value` does not encode (a length past the `u32` ceiling, or a
    /// `Held` this instance does not hold live), or when the host refuses the
    /// bundle as [`Self::save_state`] describes.
    pub fn save_state_kind<K: Kind>(&mut self, version: u32, value: &K) -> Result<(), ActorInitError> {
        let bytes = self.inline.encode_saved_state(value)?;
        self.save_state(version, &bytes)
    }
}

impl Persistence for WasmDropCtx<'_> {
    fn save_state(&mut self, version: u32, bytes: &[u8]) -> Result<(), ActorInitError> {
        // Route through the inherent `save_state` so the ADR-0114 §5
        // capture path applies — the generated `on_dehydrate` hooks reach
        // the bundle through `Persistence::save_state_kind`, which calls
        // this trait method, so a capturing ctx must intercept here too.
        WasmDropCtx::save_state(self, version, bytes)
    }

    // The generated `on_dehydrate` saves `type State` through this trait
    // method, so the ledger-granting inherent form must apply here too.
    fn save_state_kind<K: Kind>(&mut self, version: u32, value: &K) -> Result<(), ActorInitError> {
        WasmDropCtx::save_state_kind(self, version, value)
    }
}
