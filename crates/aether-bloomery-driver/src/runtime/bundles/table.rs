//! Digest-keyed shared load lifecycle: one read, one load, one root per digest (ADR-0226 decision 2).
//!
//! Each transition moves the digest's [`LoadState`] out of the map, matches
//! on it, and reinserts: a state that doesn't match is reinserted unchanged
//! and the transition reports [`OutOfStep`]. There is no placeholder state
//! and no panic path. [`BundleTable::finish_load`] is the only `Loading`
//! exit, and it matches [`LoadOutcome`]
//! exhaustively.

use std::collections::BTreeMap;

use aether_bloomery_kinds::{BundleDeclarations, Detail, Digest, ProgramDeclaration};
use aether_bloomery_program::Declaration;
use aether_data::{SchemaType, wire};

use super::{DeclaredRoles, LoadState};
use crate::runtime::core::{LoadOutcome, RootRoles};

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

    /// Every bundle whose sections have decoded and declare programs, each
    /// program with its input and result kinds' names and schemas, in digest
    /// order. A bundle still reading, or unavailable, is left out.
    pub fn declarations(&self) -> Vec<BundleDeclarations> {
        self.states
            .iter()
            .filter_map(|(bundle, state)| {
                let programs = state.roles()?.programs()?.declarations().iter().map(program_declaration).collect();
                Some(BundleDeclarations { bundle: *bundle, programs })
            })
            .collect()
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

    /// `Declared` -> `Loading`, handing back the roles the root is cast to
    /// and the wasm; `None` (state unchanged) when not `Declared`.
    pub fn begin_load(&mut self, bundle: &Digest) -> Option<(RootRoles, Vec<u8>)> {
        match self.states.remove(bundle) {
            Some(LoadState::Declared { roles, wasm }) => {
                let root = roles.root_roles();
                self.states.insert(*bundle, LoadState::Loading { roles });
                Some((root, wasm))
            }
            other => {
                if let Some(state) = other {
                    self.states.insert(*bundle, state);
                }
                None
            }
        }
    }

    /// The one load transition: `Loading` -> `Ready` (loaded or adopted) or
    /// `Unavailable(error)`.
    pub fn finish_load(&mut self, bundle: &Digest, outcome: LoadOutcome) -> Result<(), OutOfStep> {
        match self.states.remove(bundle) {
            Some(LoadState::Loading { roles }) => {
                let state = match outcome {
                    LoadOutcome::Loaded | LoadOutcome::Adopted => LoadState::Ready { roles },
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

/// One decoded record as the declarations reply carries it: each schema as
/// its `SchemaType` wire bytes.
fn program_declaration(declared: &Declaration) -> ProgramDeclaration {
    let schema_bytes = |schema: &SchemaType| {
        wire::to_vec(schema).expect("a decoded schema encodes: wire encoding into a Vec fails only past u32")
    };
    ProgramDeclaration {
        name: declared.program.name.clone(),
        mode: declared.program.mode,
        intent: declared.program.intent.clone(),
        input: declared.program.input,
        input_name: declared.input_kind.name.clone(),
        input_schema: schema_bytes(&declared.input_kind.schema),
        result: declared.program.result,
        result_name: declared.result_kind.name.clone(),
        result_schema: schema_bytes(&declared.result_kind.schema),
    }
}
