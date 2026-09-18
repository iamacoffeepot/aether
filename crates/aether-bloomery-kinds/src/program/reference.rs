//! Identity of a program inside a WASM bundle.

use crate::Digest;
use crate::program::name::ProgramName;

/// A program recorded by its bundle digest and name within that bundle.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct ProgramRef {
    bundle: Digest,
    name: ProgramName,
}

impl ProgramRef {
    /// Bind a bundle digest to a program name.
    #[must_use]
    pub const fn new(bundle: Digest, name: ProgramName) -> Self {
        Self { bundle, name }
    }

    /// Digest of the WASM bundle that carries this program.
    #[must_use]
    pub const fn bundle(&self) -> Digest {
        self.bundle
    }

    /// Name that distinguishes this program inside the bundle.
    #[must_use]
    pub const fn name(&self) -> &ProgramName {
        &self.name
    }
}
