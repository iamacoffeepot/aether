//! Turn a commit's listing into journal artifacts: one opaque blob per
//! distinct file, one [`Tree`] per directory, the root last.
//!
//! `git ls-tree -r -t` lists every tree before its descendants, so walking the
//! listing backwards reaches each directory after all of its children. Each
//! directory is sealed the moment its own entry comes up, which needs neither
//! recursion nor an arena, and every artifact is emitted after everything it
//! cites.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::str;

use aether_bloomery_kinds::{Digest, EncodedArtifact, Name, NameError, Node, Path, PathError, Ref, Tree};
use anyhow::{Context, Result};

use super::read::Listing;

/// The label the root tree carries in a refusal or a budget error.
const ROOT_PATH: &str = ".";

/// The artifacts one commit stages, in emission order.
pub(super) struct Built {
    /// Digest of the root [`Tree`].
    pub(super) root: Digest,
    /// Every distinct artifact, each after everything it cites; the root last.
    pub(super) artifacts: Vec<Staged>,
}

/// One artifact and the path that first produced it.
pub(super) struct Staged {
    /// The listing path, or [`ROOT_PATH`] for the root tree.
    pub(super) path: String,
    pub(super) artifact: EncodedArtifact,
}

/// An entry the tree cannot hold. The import stops rather than drop it.
#[derive(Debug)]
pub(super) struct Refusal {
    pub(super) path: String,
    pub(super) rule: Rule,
}

/// Which rule refused the entry.
#[derive(Debug)]
pub(super) enum Rule {
    /// A mode other than `100644`, `100755`, `120000`, or `040000`, such as a
    /// `160000` gitlink.
    Mode(String),
    /// A path segment [`Name::new`] refuses.
    Name(NameError),
    /// A symlink target that is not UTF-8.
    TargetNotUtf8,
    /// A symlink target [`Path::new`] refuses.
    Target(PathError),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = &self.path;
        match &self.rule {
            Rule::Mode(mode) => write!(
                f,
                "`{path}`: mode {mode} is not a file (100644), executable (100755), symlink (120000), or directory (040000)"
            ),
            Rule::Name(error) => write!(f, "`{path}`: the entry name is refused ({error})"),
            Rule::TargetNotUtf8 => write!(f, "`{path}`: the symlink target is not UTF-8"),
            Rule::Target(error) => write!(f, "`{path}`: the symlink target is refused ({error})"),
        }
    }
}

impl Error for Refusal {}

/// Build every artifact the listing needs, emitting each distinct digest once.
///
/// # Errors
/// A [`Refusal`] for an entry the tree cannot hold, or a failure to encode a
/// directory.
pub(super) fn build(listing: &Listing) -> Result<Built> {
    let mut pending: HashMap<&str, BTreeMap<Name, Node>> = HashMap::new();
    let mut emitted = HashSet::new();
    let mut artifacts = Vec::new();
    let mut stage = |path: &str, artifact: EncodedArtifact| {
        let digest = artifact.digest();
        if emitted.insert(digest) {
            artifacts.push(Staged { path: path.to_owned(), artifact });
        }
        digest
    };

    for entry in listing.entries.iter().rev() {
        let path = entry.path.as_str();
        let refuse = |rule| Refusal { path: path.to_owned(), rule };
        let blob =
            || listing.blobs.get(&entry.oid).with_context(|| format!("`{path}`: blob {} was not read", entry.oid));

        let node = match entry.mode.as_str() {
            "100644" => Node::File(Ref::from_digest(stage(path, EncodedArtifact::opaque_bytes(blob()?)))),
            "100755" => Node::Executable(Ref::from_digest(stage(path, EncodedArtifact::opaque_bytes(blob()?)))),
            "120000" => {
                let target = str::from_utf8(blob()?).map_err(|_| refuse(Rule::TargetNotUtf8))?;
                Node::Symlink(Path::new(target).map_err(|error| refuse(Rule::Target(error)))?)
            }
            "040000" => {
                let tree = Tree::new(pending.remove(path).unwrap_or_default());
                let artifact = EncodedArtifact::new(&tree).with_context(|| format!("`{path}`: encoding the tree"))?;
                Node::Directory(Ref::from_digest(stage(path, artifact)))
            }
            mode => return Err(refuse(Rule::Mode(mode.to_owned())).into()),
        };

        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        pending.entry(parent).or_default().insert(Name::new(name).map_err(|error| refuse(Rule::Name(error)))?, node);
    }

    let root = Tree::new(pending.remove("").unwrap_or_default());
    let root = stage(ROOT_PATH, EncodedArtifact::new(&root).context("encoding the root tree")?);
    Ok(Built { root, artifacts })
}
