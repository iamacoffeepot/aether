//! Persistence-only context for a read-only replacement snapshot.

use alloc::vec::Vec;

use aether_data::{Kind, Schema, wire};

use crate::wasm::SnapshotError;
use crate::wasm::bridge::persist;

/// A snapshot deposit captured inside the inline-child composition walk.
#[derive(Default)]
pub(crate) struct CapturedSnapshot {
    saved: Option<(u32, Vec<u8>)>,
}

impl CapturedSnapshot {
    pub(crate) fn take(&mut self) -> Option<(u32, Vec<u8>)> {
        self.saved.take()
    }
}

/// The only host capability offered while preparing migration state.
/// Snapshot hooks must not mutate logical guest state, including through
/// interior mutability, and must leave it usable if they fail or trap.
pub struct WasmSnapshotCtx<'a> {
    capture: Option<&'a mut CapturedSnapshot>,
}

impl<'a> WasmSnapshotCtx<'a> {
    /// Construct the host-backed context used by `export!`.
    #[doc(hidden)]
    #[must_use]
    pub fn __new() -> Self {
        Self { capture: None }
    }

    pub(crate) fn capturing(capture: &'a mut CapturedSnapshot) -> Self {
        Self { capture: Some(capture) }
    }

    /// Deposit the opaque migration bundle. A second deposit replaces the first.
    pub fn save_state(&mut self, version: u32, bytes: &[u8]) -> Result<(), SnapshotError> {
        if let Some(capture) = self.capture.as_mut() {
            capture.saved = Some((version, bytes.to_vec()));
            return Ok(());
        }
        let status = persist::save_state(version, bytes);
        if status == 0 {
            Ok(())
        } else {
            Err(SnapshotError::new(alloc::format!("save_state host call rejected with status {status}")))
        }
    }

    /// Deposit a kind-framed migration bundle.
    pub fn save_state_kind<K>(&mut self, version: u32, value: &K) -> Result<(), SnapshotError>
    where
        K: Kind + Schema + serde::Serialize,
    {
        let mut bytes = Vec::from(K::ID.0.to_le_bytes());
        bytes.extend_from_slice(
            &wire::to_vec(value)
                .map_err(|error| SnapshotError::new(alloc::format!("snapshot encode failed: {error}")))?,
        );
        self.save_state(version, &bytes)
    }
}
