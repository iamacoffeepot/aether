//! What a tool that changes the tree returns.

use alloc::string::String;

use aether_bloomery_kinds::{Ref, Tree};

/// A tree-changing tool's result: the tree after the call and what it did.
///
/// The loop that runs the call reads a result of this kind and binds its
/// tree into every later call. A call that changed nothing returns the tree
/// it was given, and its summary says why.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.program.edited")]
pub struct Edited {
    /// The tree after the call.
    tree: Ref<Tree>,
    /// One sentence on what the call changed, or why it changed nothing.
    summary: String,
}

impl Edited {
    /// The call left `tree`, and `summary` says what it did.
    #[must_use]
    pub fn new(tree: Ref<Tree>, summary: impl Into<String>) -> Self {
        Self { tree, summary: summary.into() }
    }

    /// The tree after the call.
    #[must_use]
    pub const fn tree(&self) -> Ref<Tree> {
        self.tree
    }

    /// What the call changed, or why it changed nothing.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }
}
