//! `muse export`: write the difference between two trees into a directory.
//!
//! Both trees are walked together with an explicit list of directory pairs,
//! one level at a time with one batched `read_artifacts` per level, and only
//! directories whose digests differ are entered. Every deleted file and
//! symlink is removed, then every directory only `from` holds, deepest first,
//! then every added or changed file is written with its execute bit and every
//! added or changed symlink is made. Run on a checkout of the `from` tree,
//! `git diff` is then the patch.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path as HostPath, PathBuf};

use aether_bloomery_kinds::{Name, Node, Tree};
use aether_data::{Digest, Kind, OpaqueBytes};
use anyhow::{Context, Result, bail};
use clap::Args;

use super::EngineArgs;
use crate::bloomery::{Reads, decode, load, parse_digest, read_each};

/// Arguments for `cargo xtask muse export`.
#[derive(Args, Debug)]
pub(super) struct ExportArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// The tree the directory holds now, by its digest.
    #[arg(long)]
    from: String,
    /// The tree to bring the directory to, by its digest.
    #[arg(long)]
    to: String,
    /// The directory to write into: a checkout of `--from`.
    #[arg(long)]
    into: PathBuf,
}

/// Export the difference and print `A|M|D <path>` for each change.
pub(super) fn run(args: &ExportArgs) -> Result<()> {
    let (from, to) = (parse_digest(&args.from)?, parse_digest(&args.to)?);
    let mut engine = args.engine.connect()?;

    for change in export(&mut engine, from, to, &args.into)? {
        println!("{} {}", change.action.letter(), change.path);
    }
    Ok(())
}

/// One entry the export changed.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Change {
    /// The entry's path under the directory, `/`-separated.
    pub(super) path: String,
    pub(super) action: Action,
}

/// What the export did to one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Added,
    Modified,
    Deleted,
}

impl Action {
    const fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
        }
    }
}

/// One file or symlink to write or remove: the node `to` holds at `path`,
/// or `None` for a deletion.
struct Edit {
    path: String,
    action: Action,
    node: Option<Node>,
}

/// Every edit between two trees, and the directories only `from` holds,
/// deepest first.
struct Diff {
    edits: Vec<Edit>,
    removed: Vec<String>,
}

/// Bring the directory `into` from tree `from` to tree `to`, and return
/// every changed file and symlink in path order.
///
/// # Errors
/// A tree or blob read failed, or the directory could not be written.
pub(super) fn export(reads: &mut impl Reads, from: Digest, to: Digest, into: &HostPath) -> Result<Vec<Change>> {
    let Diff { edits, removed } = diff(reads, from, to)?;

    for edit in edits.iter().filter(|edit| edit.node.is_none()) {
        remove_entry(&into.join(&edit.path))?;
    }
    for directory in &removed {
        match fs::remove_dir(into.join(directory)) {
            Err(error) if !matches!(error.kind(), ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty) => {
                return Err(error).with_context(|| format!("removing the directory `{directory}`"));
            }
            _ => {}
        }
    }
    write(reads, &edits, into)?;

    let mut changes: Vec<_> = edits.into_iter().map(|edit| Change { path: edit.path, action: edit.action }).collect();
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

/// Walk `from` and `to` together, level by level, reading each level's
/// unread trees in one batch.
fn diff(reads: &mut impl Reads, from: Digest, to: Digest) -> Result<Diff> {
    let mut trees: HashMap<Digest, Tree> = HashMap::new();
    let mut found = Diff { edits: Vec::new(), removed: Vec::new() };
    let mut level = vec![(String::new(), Some(from), Some(to))];

    while !level.is_empty() {
        let unread: BTreeSet<Digest> = level
            .iter()
            .flat_map(|(_, old, new)| [*old, *new])
            .flatten()
            .filter(|digest| !trees.contains_key(digest))
            .collect();
        read_each(reads, &unread.into_iter().collect::<Vec<_>>(), |digest, artifact| {
            trees.insert(digest, decode::<Tree>(artifact, digest)?);
            Ok(())
        })?;

        let mut next = Vec::new();
        for (prefix, old, new) in level {
            let (old, new) = (entries(&trees, old)?, entries(&trees, new)?);
            for name in old.keys().chain(new.keys().filter(|name| !old.contains_key(*name))) {
                let path = if prefix.is_empty() {
                    name.as_str().to_owned()
                } else {
                    format!("{prefix}/{}", name.as_str())
                };
                found.entry(path, old.get(name), new.get(name), &mut next);
            }
        }
        level = next;
    }

    found.removed.sort_by_key(|path| Reverse(path.matches('/').count()));
    Ok(found)
}

impl Diff {
    /// Record how the entry at `path` changed from `old` to `new`, queueing
    /// the directory pairs to walk next on `next`.
    fn entry(
        &mut self,
        path: String,
        old: Option<&Node>,
        new: Option<&Node>,
        next: &mut Vec<(String, Option<Digest>, Option<Digest>)>,
    ) {
        let edit = |path, action, node: Option<&Node>| Edit { path, action, node: node.cloned() };
        match (old, new) {
            (Some(old), Some(new)) if old == new => {}
            (Some(Node::Directory(old)), Some(Node::Directory(new))) => {
                next.push((path, Some(old.digest()), Some(new.digest())));
            }
            (Some(Node::Directory(old)), new) => {
                next.push((path.clone(), Some(old.digest()), None));
                self.removed.push(path.clone());
                if new.is_some() {
                    self.edits.push(edit(path, Action::Added, new));
                }
            }
            (old, Some(Node::Directory(new))) => {
                if old.is_some() {
                    self.edits.push(edit(path.clone(), Action::Deleted, None));
                }
                next.push((path, None, Some(new.digest())));
            }
            (Some(_), Some(_)) => self.edits.push(edit(path, Action::Modified, new)),
            (None, Some(_)) => self.edits.push(edit(path, Action::Added, new)),
            (Some(_), None) => self.edits.push(edit(path, Action::Deleted, None)),
            (None, None) => {}
        }
    }
}

/// The entries of the tree `digest` names, or none for `None`.
fn entries(trees: &HashMap<Digest, Tree>, digest: Option<Digest>) -> Result<&BTreeMap<Name, Node>> {
    static EMPTY: BTreeMap<Name, Node> = BTreeMap::new();
    match digest {
        Some(digest) => Ok(trees.get(&digest).with_context(|| format!("tree {digest} was not read"))?.entries()),
        None => Ok(&EMPTY),
    }
}

/// Write every added or changed file and symlink under `into`, reading the
/// file blobs in batches.
fn write(reads: &mut impl Reads, edits: &[Edit], into: &HostPath) -> Result<()> {
    let mut blobs: BTreeMap<Digest, Vec<(&str, bool)>> = BTreeMap::new();
    for edit in edits {
        match &edit.node {
            Some(Node::File(blob)) => blobs.entry(blob.digest()).or_default().push((&edit.path, false)),
            Some(Node::Executable(blob)) => blobs.entry(blob.digest()).or_default().push((&edit.path, true)),
            Some(Node::Symlink(target)) => symlink(target.as_str(), &prepare(into, &edit.path)?)?,
            Some(Node::Directory(_)) | None => {}
        }
    }

    let digests: Vec<Digest> = blobs.keys().copied().collect();
    read_each(reads, &digests, |digest, artifact| {
        if artifact.kind() != OpaqueBytes::ID {
            bail!("artifact {digest} is not a file's bytes");
        }
        let bytes = load(artifact, digest)?;
        for (path, executable) in blobs.get(&digest).map_or(&[][..], Vec::as_slice) {
            let host = prepare(into, path)?;
            fs::write(&host, &bytes).with_context(|| format!("writing `{path}`"))?;
            set_executable(&host, *executable).with_context(|| format!("setting the mode of `{path}`"))?;
        }
        Ok(())
    })
}

/// The host path of `path` under `into`, with its parent directories made and
/// any file or symlink already there removed, so a write never follows a link.
fn prepare(into: &HostPath, path: &str) -> Result<PathBuf> {
    let host = into.join(path);
    if let Some(parent) = host.parent() {
        fs::create_dir_all(parent).with_context(|| format!("making the parent directories of `{path}`"))?;
    }
    remove_entry(&host)?;
    Ok(host)
}

/// Remove the file or symlink at `host`, if there is one.
///
/// # Errors
/// A directory stands at `host`, or the removal failed.
fn remove_entry(host: &HostPath) -> Result<()> {
    match fs::symlink_metadata(host) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("reading {}", host.display())),
        Ok(metadata) if metadata.is_dir() => bail!("{} is a directory, not a file or symlink", host.display()),
        Ok(_) => fs::remove_file(host).with_context(|| format!("removing {}", host.display())),
    }
}

#[cfg(unix)]
fn set_executable(host: &HostPath, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(
        host,
        fs::Permissions::from_mode(if executable {
            0o755
        } else {
            0o644
        }),
    )?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_host: &HostPath, _executable: bool) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &str, host: &HostPath) -> Result<()> {
    use std::os::unix::fs::symlink as link;

    link(target, host).with_context(|| format!("linking {} to {target}", host.display()))
}

#[cfg(not(unix))]
fn symlink(target: &str, host: &HostPath) -> Result<()> {
    bail!("{} links to {target}, and this host cannot make a symlink", host.display())
}
