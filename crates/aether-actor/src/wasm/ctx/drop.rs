//! Persistence-only context for the read-only `on_dehydrate` hook.

use alloc::vec::Vec;

use aether_data::Kind;

use crate::model::ctx::persistence::Persistence;
use crate::wasm::bridge::persist;

/// A migration deposit captured while composing a parent and its children.
#[derive(Default)]
pub struct CapturedState {
    saved: Option<(u32, Vec<u8>)>,
}

impl CapturedState {
    #[must_use]
    pub fn take(&mut self) -> Option<(u32, Vec<u8>)> {
        self.saved.take()
    }
}

/// The only capability offered to a dehydration hook. The hook must not
/// mutate logical guest state, including through interior mutability.
#[allow(clippy::module_name_repetitions)]
pub struct WasmDropCtx<'a> {
    capture: Option<&'a mut CapturedState>,
}

impl<'a> WasmDropCtx<'a> {
    /// Construct the host-backed context used by `export!`.
    #[doc(hidden)]
    #[must_use]
    pub fn __new() -> Self {
        Self { capture: None }
    }

    pub(crate) fn __new_capturing(capture: &'a mut CapturedState) -> Self {
        Self { capture: Some(capture) }
    }

    /// Deposit one migration bundle. The most recent deposit wins. Host
    /// rejection traps the hook and aborts replacement preparation.
    ///
    /// # Panics
    /// Panics if the substrate rejects the deposit.
    pub fn save_state(&mut self, version: u32, bytes: &[u8]) {
        if let Some(capture) = self.capture.as_mut() {
            capture.saved = Some((version, bytes.to_vec()));
            return;
        }
        let status = persist::save_state(version, bytes);
        assert_eq!(status, 0, "aether-actor: save_state failed (status {status})");
    }

    /// Persist a kind-framed migration bundle.
    pub fn save_state_kind<K>(&mut self, version: u32, value: &K)
    where
        K: Kind + aether_data::Schema + serde::Serialize,
    {
        <Self as Persistence>::save_state_kind::<K>(self, version, value);
    }
}

impl Persistence for WasmDropCtx<'_> {
    fn save_state(&mut self, version: u32, bytes: &[u8]) {
        WasmDropCtx::save_state(self, version, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn capturing_context_retains_last_deposit() {
        let mut captured = CapturedState::default();
        {
            let mut ctx = WasmDropCtx::__new_capturing(&mut captured);
            ctx.save_state(1, &[1]);
            ctx.save_state(2, &[2]);
        }
        assert_eq!(captured.take(), Some((2, vec![2])));
    }
}
