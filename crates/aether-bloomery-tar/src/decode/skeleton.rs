//! The directory arena decode fills entry by entry, then seals bottom-up.
//!
//! It holds a [`Name`] and a node or arena index per entry, never a blob.

use std::collections::BTreeMap;
use std::mem;

use aether_bloomery_kinds::{Name, Node, Ref, Tree};

use super::entry::EntryPath;
use super::{DecodeError, Refusal};
use crate::store::TreeSink;

/// Index 0 is the root. A directory is always pushed after its parent, so
/// every directory's index is greater than its parent's.
pub(super) struct Skeleton {
    dirs: Vec<PendingDir>,
}

struct PendingDir {
    entries: BTreeMap<Name, Pending>,
    /// Named by its own entry, rather than created as a missing parent.
    explicit: bool,
}

enum Pending {
    Node(Node),
    Dir(usize),
}

/// A slot checked free for one non-directory entry, handed out before its
/// bytes stream so a refused path costs no blob.
pub(super) struct Vacancy {
    dir: usize,
    name: Name,
}

impl Skeleton {
    pub(super) fn new() -> Self {
        Self { dirs: vec![PendingDir { entries: BTreeMap::new(), explicit: true }] }
    }

    /// Reserve `path` for a non-directory entry.
    pub(super) fn vacancy(&mut self, path: &EntryPath) -> Result<Vacancy, Refusal> {
        let dir = self.parent_of(path)?;
        if self.dirs[dir].entries.contains_key(path.name()) {
            return Err(Refusal::Duplicate);
        }
        Ok(Vacancy { dir, name: path.name().clone() })
    }

    pub(super) fn fill(&mut self, vacancy: Vacancy, node: Node) {
        self.dirs[vacancy.dir].entries.insert(vacancy.name, Pending::Node(node));
    }

    /// Record an explicit directory. One over an implicit directory merges.
    pub(super) fn directory(&mut self, path: &EntryPath) -> Result<(), Refusal> {
        let dir = self.parent_of(path)?;
        match self.dirs[dir].entries.get(path.name()) {
            None => {
                self.push_dir(dir, path.name().clone(), true);
                Ok(())
            }
            Some(&Pending::Dir(child)) if !self.dirs[child].explicit => {
                self.dirs[child].explicit = true;
                Ok(())
            }
            Some(_) => Err(Refusal::Duplicate),
        }
    }

    /// The node an earlier File or Executable entry at `target` recorded.
    pub(super) fn hardlink(&self, target: &EntryPath) -> Result<Node, Refusal> {
        let mut dir = 0;
        for segment in target.parents() {
            let Some(&Pending::Dir(child)) = self.dirs[dir].entries.get(segment) else {
                return Err(Refusal::HardlinkTarget);
            };
            dir = child;
        }
        match self.dirs[dir].entries.get(target.name()) {
            Some(Pending::Node(node @ (Node::File(_) | Node::Executable(_)))) => Ok(node.clone()),
            _ => Err(Refusal::HardlinkTarget),
        }
    }

    /// Seal every directory, children before parents, through `sink`, and
    /// return the root's reference.
    pub(super) fn finish<K: TreeSink>(mut self, sink: &mut K) -> Result<Ref<Tree>, DecodeError<K::Error>> {
        let mut sealed = Vec::with_capacity(self.dirs.len());
        for index in (1..self.dirs.len()).rev() {
            let tree = self.seal(index, &sealed, sink)?;
            sealed.push(tree);
        }
        self.seal(0, &sealed, sink)
    }

    /// Build and store directory `index`. `sealed` holds the references of
    /// every directory above it, highest index first.
    fn seal<K: TreeSink>(
        &mut self,
        index: usize,
        sealed: &[Ref<Tree>],
        sink: &mut K,
    ) -> Result<Ref<Tree>, DecodeError<K::Error>> {
        let last = self.dirs.len() - 1;
        let entries = mem::take(&mut self.dirs[index].entries)
            .into_iter()
            .map(|(name, pending)| match pending {
                Pending::Node(node) => (name, node),
                Pending::Dir(child) => (name, Node::Directory(sealed[last - child])),
            })
            .collect();
        sink.put_tree(&Tree::new(entries)).map_err(DecodeError::Sink)
    }

    /// Walk to the parent directory of `path`, creating missing parents.
    fn parent_of(&mut self, path: &EntryPath) -> Result<usize, Refusal> {
        let mut dir = 0;
        for segment in path.parents() {
            dir = match self.dirs[dir].entries.get(segment) {
                Some(&Pending::Dir(child)) => child,
                Some(Pending::Node(_)) => return Err(Refusal::ParentNotDirectory),
                None => self.push_dir(dir, segment.clone(), false),
            };
        }
        Ok(dir)
    }

    fn push_dir(&mut self, parent: usize, name: Name, explicit: bool) -> usize {
        let index = self.dirs.len();
        self.dirs[parent].entries.insert(name, Pending::Dir(index));
        self.dirs.push(PendingDir { entries: BTreeMap::new(), explicit });
        index
    }
}
