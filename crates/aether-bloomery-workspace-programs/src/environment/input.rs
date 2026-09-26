//! `environment.merge.input`: the two imported trees an environment is merged from.

use aether_bloomery_kinds::{Ref, Tree};

/// The two imported trees an environment is merged from (ADR-0237 decision 3).
///
/// Both are typed citations, so the driver's closure walk carries every
/// member of both trees into the invocation, and the merge reads only the
/// directories it needs.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "environment.merge.input")]
pub struct MergeInput {
    /// The distro userland the environment's root starts from.
    pub base: Ref<Tree>,
    /// The whole toolchain image; the merge takes only its one toolchain directory.
    pub toolchain: Ref<Tree>,
}
