//! `proof.clippy.input`: the source tree, the environment, and the vendored crate sources a clippy proof runs over.

use aether_bloomery_kinds::{Ref, Tree};
use aether_workspace::Environment;

/// What one clippy proof runs over (ADR-0237 decisions 2 and 4).
///
/// All three are typed citations, so the driver's closure walk carries every
/// member into the invocation; the program itself reads only this input.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "proof.clippy.input")]
pub struct ClippyInput {
    /// The cargo workspace under proof, written out at `/work`.
    pub source: Ref<Tree>,
    /// The run's whole root filesystem and its tool table, read from the head
    /// `(aether.workspace.environment, <platform>)`.
    pub environment: Ref<Environment>,
    /// The `cargo vendor` tree for the source's `Cargo.lock`, mounted
    /// read-only at `/vendor`: the `Vendored.tree` of a `vendor.cargo`
    /// transition over a source with the same `Cargo.lock`. An empty tree
    /// serves a crate with no dependencies.
    pub vendor: Ref<Tree>,
}
