//! The run's output tree: the last step's `/work`, minus `scratch`.
//!
//! `GET /containers/{last}/archive?path=/work` answers a tar whose one
//! top-level entry is `work`. It decodes under the canonical rules and the
//! output bounds straight into the run's batch, so a FIFO, a device, an
//! absolute symlink, or a name the kinds refuse fails the run, as does an
//! output over the bounds. The `work` entry's subtree is the output.
//!
//! Each scratch path was a tmpfs, which the archive holds as an empty
//! directory, so removing it rebuilds only its ancestors. The decode keeps in
//! memory just the directories that can be those ancestors: the archive root
//! and every directory holding an entry named like a scratch segment. Mount
//! paths are outside `/work` and never read back.

use std::collections::{BTreeSet, HashMap};
use std::mem;

use aether_bloomery_journal::{ArtifactBatch, JournalError};
use aether_bloomery_kinds::{Digest, Name, Node, Ref, Tree};
use aether_bloomery_tar::{Limits, Rules, TreeSink, decode};

use super::volumes::Volumes;
use super::{RunError, engine_failed};
use crate::Scratch;
use crate::runtime::engine::{ContainerId, Engine};
use crate::runtime::journal::{JournalBlob, JournalSink};

/// The archive's top-level entry: the last segment of [`Volumes::WORK_PATH`].
const WORK: &str = "work";

/// Decode the container's `/work` into the batch and return it minus
/// `scratch`.
pub fn collect(
    engine: &Engine,
    batch: &mut ArtifactBatch,
    container: &ContainerId,
    scratch: &Scratch,
    limits: Limits,
) -> Result<Ref<Tree>, RunError> {
    let archive = engine
        .get_archive(container, Volumes::WORK_PATH)
        .map_err(engine_failed(format!("reading /work from container {container}")))?;
    let segments = scratch_segments(scratch);
    let mut sink = Capture { inner: JournalSink::new(batch), names: &segments, kept: HashMap::new() };
    let root = decode(archive, &mut sink, &Rules::canonical(limits)).map_err(RunError::Output)?;
    let mut kept = sink.kept;

    let work = name(WORK)?;
    let work = match kept.get(&root.digest()).and_then(|tree| tree.entries().get(&work)) {
        Some(Node::Directory(work)) => *work,
        _ => return Err(RunError::Shape("the output archive holds no /work directory".to_owned())),
    };
    scratch.as_slice().iter().try_fold(work, |tree, path| without(batch, &mut kept, tree, path.as_str()))
}

/// `WORK` plus every segment of every scratch path: the entry names a
/// directory must hold to be the archive root or a scratch path's ancestor.
fn scratch_segments(scratch: &Scratch) -> BTreeSet<String> {
    scratch.as_slice().iter().flat_map(|path| path.as_str().split('/')).chain([WORK]).map(str::to_owned).collect()
}

/// `tree` with the entry at `path` removed, restaging only the directories
/// above it, one per segment and without recursion. A path that does not
/// resolve to an entry leaves `tree` as it is.
fn without(
    batch: &mut ArtifactBatch,
    kept: &mut HashMap<Digest, Tree>,
    tree: Ref<Tree>,
    path: &str,
) -> Result<Ref<Tree>, RunError> {
    let mut ancestors: Vec<(Tree, Name)> = Vec::new();
    let mut current = kept_tree(kept, &tree)?;
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        let segment = name(segment)?;
        if segments.peek().is_none() {
            if current.entries().get(&segment).is_none() {
                return Ok(tree);
            }
            let mut entries = current.entries().clone();
            entries.remove(&segment);
            current = Tree::new(entries);
            break;
        }
        let Some(Node::Directory(child)) = current.entries().get(&segment).cloned() else {
            return Ok(tree);
        };
        let parent = mem::replace(&mut current, kept_tree(kept, &child)?);
        ancestors.push((parent, segment));
    }

    let mut rebuilt = stage(batch, kept, current)?;
    while let Some((parent, segment)) = ancestors.pop() {
        let mut entries = parent.entries().clone();
        entries.insert(segment, Node::Directory(rebuilt));
        rebuilt = stage(batch, kept, Tree::new(entries))?;
    }
    Ok(rebuilt)
}

/// Stage a rebuilt directory and keep it for the next scratch path.
fn stage(batch: &mut ArtifactBatch, kept: &mut HashMap<Digest, Tree>, tree: Tree) -> Result<Ref<Tree>, RunError> {
    let staged = batch
        .stage_encoded(&tree)
        .map_err(|error| RunError::Journal { during: "storing the output tree without scratch", error })?;
    kept.insert(staged.digest(), tree);
    Ok(staged)
}

fn kept_tree(kept: &HashMap<Digest, Tree>, tree: &Ref<Tree>) -> Result<Tree, RunError> {
    kept.get(&tree.digest()).cloned().ok_or_else(|| {
        RunError::Shape(format!("the output directory {} was not kept for scratch removal", tree.digest()))
    })
}

fn name(segment: &str) -> Result<Name, RunError> {
    Name::new(segment).map_err(|error| RunError::Shape(format!("{segment:?} is not a tree name: {error}")))
}

/// A [`JournalSink`] that also keeps, in memory, each directory holding an
/// entry named in `names`.
struct Capture<'batch, 'names> {
    inner: JournalSink<'batch>,
    names: &'names BTreeSet<String>,
    kept: HashMap<Digest, Tree>,
}

impl TreeSink for Capture<'_, '_> {
    type Error = JournalError;
    type Blob<'a>
        = JournalBlob<'a>
    where
        Self: 'a;

    fn begin_blob(&mut self, len: u64) -> Result<Self::Blob<'_>, JournalError> {
        self.inner.begin_blob(len)
    }

    fn put_tree(&mut self, tree: &Tree) -> Result<Ref<Tree>, JournalError> {
        let stored = self.inner.put_tree(tree)?;
        if tree.entries().keys().any(|entry| self.names.contains(entry.as_str())) {
            self.kept.insert(stored.digest(), tree.clone());
        }
        Ok(stored)
    }
}
