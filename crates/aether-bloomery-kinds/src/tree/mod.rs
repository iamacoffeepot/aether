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
//! Build outputs are never entries in any tree. That is a rule of the
//! snapshot brick, stated here so a tree is not mistaken for a build graph.

mod entries;
mod name;
mod node;
mod path;

use alloc::collections::BTreeMap;

pub use entries::TreeError;
pub use name::{Name, NameError};
pub use node::Node;
pub use path::{Path, PathError};

use entries::Entries;

/// A directory of named entries. Valid at all times: every [`Name`] is valid
/// by construction, and no two entries collide under NFC plus case folding.
///
/// The empty tree is a legal value with a real digest, like Git's empty tree
/// object.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.tree")]
pub struct Tree {
    entries: Entries,
}

impl Tree {
    /// Accept a map whose names do not collide under NFC plus case folding.
    ///
    /// # Errors
    ///
    /// [`TreeError::Collides`] names the first pair in canonical name order.
    pub fn new(entries: BTreeMap<Name, Node>) -> Result<Self, TreeError> {
        Ok(Self { entries: Entries::new(entries)? })
    }

    /// A directory with no entries.
    #[must_use]
    pub fn empty() -> Self {
        Self { entries: Entries::empty() }
    }

    /// Borrow the entries. Iteration is canonical name order.
    #[must_use]
    pub fn entries(&self) -> &BTreeMap<Name, Node> {
        self.entries.as_map()
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;

    use crate::Ref;

    use super::{Name, Node, Tree, TreeError};

    fn file(bytes: &[u8]) -> Node {
        Node::File(Ref::of_bytes(bytes))
    }

    #[test]
    fn colliding_names_are_refused() {
        let mut entries = BTreeMap::new();
        entries.insert(Name::new("README").expect("valid"), file(b"a"));
        entries.insert(Name::new("readme").expect("valid"), file(b"b"));
        match Tree::new(entries) {
            Err(TreeError::Collides { a, b }) => {
                assert_eq!(a.as_str(), "README");
                assert_eq!(b.as_str(), "readme");
            }
            other => panic!("expected Collides, got {other:?}"),
        }
    }
}
