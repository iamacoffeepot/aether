//! What a proof tool takes: the arguments the model writes for a clippy proof,
//! and the session values every proof binds, the environment and the vendor
//! tree.

use aether_bloomery_kinds::{Ref, Tree};
use aether_bloomery_workspace::Environment;

/// The arguments of `proof.clippy`: none. The proof always runs over the
/// whole workspace in the session's tree, so the model writes `{}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "proof.clippy.args")]
pub struct ClippyArgs;

/// What every proof binds besides the tree it runs over: the environment the
/// run happens in and the crate sources it builds against (ADR-0237
/// decisions 3 and 4). The session that offers a proof binds it, and the
/// model never sees it.
///
/// Both are typed citations, so the driver's closure walk carries them into
/// the invocation.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "proof.bound")]
pub struct ProofBound {
    /// The run's whole root filesystem and its tool table, read from the head
    /// `(aether.workspace.environment, <platform>)`.
    environment: Ref<Environment>,
    /// The `cargo vendor` tree for the source's `Cargo.lock`, mounted
    /// read-only at `/vendor`: the `Vendored.tree` of a `vendor.cargo`
    /// transition over a source with the same `Cargo.lock`. An empty tree
    /// serves a workspace with no dependencies.
    vendor: Ref<Tree>,
}

impl ProofBound {
    /// Proofs that run in `environment` and build against the crate sources
    /// in `vendor`.
    #[must_use]
    pub const fn new(environment: Ref<Environment>, vendor: Ref<Tree>) -> Self {
        Self { environment, vendor }
    }

    /// The environment the run happens in.
    #[must_use]
    pub const fn environment(&self) -> Ref<Environment> {
        self.environment
    }

    /// The vendored crate sources, mounted at `/vendor`.
    #[must_use]
    pub const fn vendor(&self) -> Ref<Tree> {
        self.vendor
    }
}
