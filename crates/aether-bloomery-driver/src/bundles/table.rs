//! Digest-keyed shared load lifecycle: one read, one load, one root per digest (ADR-0226 decision 2).
//!
//! Each transition moves the digest's [`LoadState`] out of the map, matches
//! on it, and reinserts: a state that doesn't match is reinserted unchanged
//! and the transition reports [`OutOfStep`]. There is no placeholder state
//! and no panic path. [`BundleTable::finish_load`] is the only `Loading`
//! exit, and it matches [`LoadOutcome`]
//! exhaustively.

use std::collections::BTreeMap;

use aether_bloomery_kinds::{Detail, Digest};

use super::{DeclaredRoles, LoadState};
use crate::core::LoadOutcome;

/// The load step a transition expected was not the digest's current state.
#[derive(Debug)]
pub struct OutOfStep;

/// Digest-keyed shared load lifecycle: one read, one load, one root per digest.
#[derive(Debug, Default)]
pub struct BundleTable {
    states: BTreeMap<Digest, LoadState>,
}

impl BundleTable {
    /// The digest's load state, if it has ever been seen.
    pub fn state(&self, bundle: &Digest) -> Option<&LoadState> {
        self.states.get(bundle)
    }

    /// Whether the digest's root is loaded, role-agnostic: `true` only when `Ready`. #6222's adoption seam.
    pub fn ready(&self, bundle: &Digest) -> bool {
        matches!(self.states.get(bundle), Some(LoadState::Ready { .. }))
    }

    /// Whether the digest's read or load is in flight (`Reading` or `Loading`).
    pub fn pending(&self, bundle: &Digest) -> bool {
        matches!(self.states.get(bundle), Some(LoadState::Reading | LoadState::Loading { .. }))
    }

    /// Insert `Reading` for an unseen digest; `true` when the caller must issue the read.
    pub fn begin_read(&mut self, bundle: Digest) -> bool {
        if self.states.contains_key(&bundle) {
            return false;
        }
        self.states.insert(bundle, LoadState::Reading);
        true
    }

    /// `Reading` -> `Declared` or `Unavailable`.
    pub fn finish_read(
        &mut self,
        bundle: &Digest,
        read: Result<(DeclaredRoles, Vec<u8>), Detail>,
    ) -> Result<(), OutOfStep> {
        match self.states.remove(bundle) {
            Some(LoadState::Reading) => {
                let state = match read {
                    Ok((roles, wasm)) => LoadState::Declared { roles, wasm },
                    Err(reason) => LoadState::Unavailable(reason),
                };
                self.states.insert(*bundle, state);
                Ok(())
            }
            other => {
                if let Some(state) = other {
                    self.states.insert(*bundle, state);
                }
                Err(OutOfStep)
            }
        }
    }

    /// `Declared` -> `Loading`, handing back the wasm; `None` (state unchanged) when not `Declared`.
    pub fn begin_load(&mut self, bundle: &Digest) -> Option<Vec<u8>> {
        match self.states.remove(bundle) {
            Some(LoadState::Declared { roles, wasm }) => {
                self.states.insert(*bundle, LoadState::Loading { roles });
                Some(wasm)
            }
            other => {
                if let Some(state) = other {
                    self.states.insert(*bundle, state);
                }
                None
            }
        }
    }

    /// The one load transition: `Loading` -> `Ready` or `Unavailable(error)`.
    pub fn finish_load(&mut self, bundle: &Digest, outcome: LoadOutcome) -> Result<(), OutOfStep> {
        match self.states.remove(bundle) {
            Some(LoadState::Loading { roles }) => {
                let state = match outcome {
                    LoadOutcome::Loaded => LoadState::Ready { roles },
                    LoadOutcome::Failed { error } => LoadState::Unavailable(Detail::new(error)),
                };
                self.states.insert(*bundle, state);
                Ok(())
            }
            other => {
                if let Some(state) = other {
                    self.states.insert(*bundle, state);
                }
                Err(OutOfStep)
            }
        }
    }
}
