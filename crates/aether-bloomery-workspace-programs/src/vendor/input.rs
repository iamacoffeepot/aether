//! `vendor.cargo.input`: the source tree and the environment a cargo vendor run fetches for.

use aether_bloomery_kinds::{Ref, Tree};
use aether_workspace::Environment;

/// What one cargo vendor run fetches for (ADR-0237 decisions 2 and 4).
///
/// Both are typed citations, so the driver's closure walk carries every
/// member into the invocation; the program itself reads only this input.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "vendor.cargo.input")]
pub struct VendorInput {
    /// The cargo workspace whose `Cargo.lock` is vendored, mounted read-only
    /// at `/source`.
    pub source: Ref<Tree>,
    /// The run's whole root filesystem and its tool table, read from the head
    /// `(aether.workspace.environment, <platform>)`.
    pub environment: Ref<Environment>,
}
