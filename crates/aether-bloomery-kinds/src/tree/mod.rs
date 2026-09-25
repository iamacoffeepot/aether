//! A tree is a directory: a map from name to entry.
//!
//! Four entry kinds, and why not fewer:
//!
//! - [`Node::Directory`] is forced by the citation rule. [`crate::Ref<Tree>`]
//!   and [`crate::Ref<crate::OpaqueBytes>`] are different kinds, and `append`
//!   checks the prefix.
//! - [`Node::Executable`] is the one pure mode bit. Drop it and every script
//!   in the repository materializes non-runnable.
//! - [`Node::Symlink`] says "make a link" rather than "write these bytes". A
//!   file whose content is `../bin/run` and a link to `../bin/run` are
//!   different states and must not share a digest. The target sits inline;
//!   Git stores it as a blob because a Git tree entry can only hold a hash.
//! - Owner, timestamps, and the other permission bits are dropped on purpose,
//!   as Git drops them.
//!
//! Names are unique byte for byte, as Linux, tar, and Git enforce, so
//! `xt_CONNMARK.h` and `xt_connmark.h` are two entries. No tree is written
//! to a host directory (ADR-0237): trees cross into and out of a container
//! as tar streams, so a case-insensitive filesystem never has to hold one.
//! Every [`Name`] is NFC, so two spellings of one visible name are still the
//! same bytes and the same entry.
//!
//! Build outputs are never entries in any tree. That is a rule of the
//! snapshot brick, stated here so a tree is not mistaken for a build graph.

mod name;
mod node;
mod path;

use alloc::collections::BTreeMap;

pub use name::{Name, NameError};
pub use node::Node;
pub use path::{Path, PathError};

/// A directory of named entries. Valid at all times: every [`Name`] is valid
/// by construction, and the map keys make every name unique byte for byte.
///
/// The empty tree is a legal value with a real digest, like Git's empty tree
/// object.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.tree")]
pub struct Tree {
    entries: BTreeMap<Name, Node>,
}

impl Tree {
    /// A directory holding `entries`. Two names are the same entry only when
    /// their bytes are equal.
    #[must_use]
    pub fn new(entries: BTreeMap<Name, Node>) -> Self {
        Self { entries }
    }

    /// A directory with no entries.
    #[must_use]
    pub fn empty() -> Self {
        Self { entries: BTreeMap::new() }
    }

    /// Borrow the entries. Iteration is canonical name order.
    #[must_use]
    pub fn entries(&self) -> &BTreeMap<Name, Node> {
        &self.entries
    }
}
