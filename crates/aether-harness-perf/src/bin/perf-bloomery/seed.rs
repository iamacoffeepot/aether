//! The seed: one batch holding the Muse bundle under its head, the reactor set
//! that runs its session loop, the offered tools' artifacts, the tree, and one
//! open input per session.
//!
//! The tree is staged first, because the stub's replies need its probe paths
//! and the open inputs need the stub's endpoint.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write;
use std::fs;
use std::path::{Path as HostPath, PathBuf};
use std::str;

use aether_bloomery_journal::Batch;
use aether_bloomery_kinds::{
    Call, Name, NativeOrigin, Node, Path, ProgramName, ReactorSet, RecordedHead, RecordedHeadMove, Ref, Tree,
};
use aether_bloomery_muse::{
    Endpoint, MUSE, ModelName, OpenInput, OutputBudget, ReasoningEffort, TurnLimit, TurnSettings, offered,
};

use crate::knobs::{Knobs, TreeSpec};

/// How many generated files share one directory.
const FILES_PER_DIRECTORY: usize = 16;

/// The text every generated file carries once, and the pattern `tree.grep`
/// looks for.
pub const GREP_TARGET: &str = "bloomery";

/// Host entries a directory tree never stages, whatever their type: a
/// worktree's `.git` is a file, and `.git` is no valid tree entry name.
const SKIPPED: [&str; 2] = [".git", "target"];

/// The origin every session's open names.
const ORIGIN: &str = "perf.bloomery";

/// The paths and pattern the stub's calls name.
#[derive(Debug, Clone)]
pub struct Probe {
    /// The file `tree.read` reads.
    pub read: String,
    /// The pattern `tree.grep` looks for.
    pub grep: String,
}

/// A batch with the bundle, the reactor set, and the tree staged.
pub struct Seed {
    batch: Batch,
    set: Ref<ReactorSet>,
    tree: Ref<Tree>,
    pub probe: Probe,
}

/// The whole seed: the batch to append, the reactor set to move the root head
/// to, and one open `Call` per session.
pub struct Seeded {
    pub batch: Batch,
    pub set: Ref<ReactorSet>,
    pub calls: Vec<Call>,
}

impl Seed {
    /// Stage the bundle `wasm` under the `muse` head, the reactor set, and the
    /// tree the knobs name.
    pub fn new(knobs: &Knobs, wasm: &[u8]) -> Result<Self, Box<dyn Error>> {
        let mut batch = Batch::new();
        let bundle = batch.stage_bytes(wasm).digest();
        batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&MUSE), bundle), None)?;
        let set = batch.stage_encoded(&ReactorSet::new(vec![MUSE])?)?;
        let (tree, probe) = match &knobs.tree {
            TreeSpec::Synthetic { files, bytes } => synthetic(&mut batch, *files, *bytes)?,
            TreeSpec::Directory(root) => directory(&mut batch, root)?,
        };
        Ok(Self { batch, set, tree, probe })
    }

    /// Stage the offered tools and one open input per session, each posting
    /// its turns to `endpoint`.
    pub fn open(self, knobs: &Knobs, endpoint: &str) -> Result<Seeded, Box<dyn Error>> {
        let Self { mut batch, set, tree, .. } = self;
        let (tools, artifacts) = offered();
        for artifact in artifacts {
            batch.stage_artifact(artifact);
        }
        let settings = TurnSettings::new(
            Endpoint::new(endpoint)?,
            ModelName::new("muse-bench")?,
            tools,
            OutputBudget::new(512)?,
            ReasoningEffort::Low,
        );
        let limit = TurnLimit::new(knobs.turns)?;
        let name = ProgramName::new("muse.session.open")?;
        let origin = NativeOrigin::new(ORIGIN)?;

        let calls = (1..=u64::from(knobs.sessions))
            .map(|key| {
                let user = batch.stage_text(&format!("Session {key}: work through the tree."));
                let open = batch.stage_encoded(&OpenInput::new(settings.clone(), user, limit, tree))?;
                Ok(Call { program: MUSE, name: name.clone(), input: open.digest(), origin: origin.clone(), key })
            })
            .collect::<Result<_, Box<dyn Error>>>()?;
        Ok(Seeded { batch, set, calls })
    }
}

/// `files` generated files of `bytes` bytes, 16 to a directory under the
/// root, each with distinct line text carrying [`GREP_TARGET`] once.
fn synthetic(batch: &mut Batch, files: usize, bytes: usize) -> Result<(Ref<Tree>, Probe), Box<dyn Error>> {
    let mut root = BTreeMap::new();
    for (index, first) in (0..files).step_by(FILES_PER_DIRECTORY).enumerate() {
        let entries = (first..files.min(first + FILES_PER_DIRECTORY))
            .map(|file| Ok((Name::new(file_name(file))?, Node::File(batch.stage_bytes(&file_text(file, bytes))))))
            .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;
        root.insert(Name::new(directory_name(index))?, Node::Directory(batch.stage_encoded(&Tree::new(entries))?));
    }
    let tree = batch.stage_encoded(&Tree::new(root))?;
    let read = format!("{}/{}", directory_name(0), file_name(0));
    Ok((tree, Probe { read, grep: GREP_TARGET.to_owned() }))
}

fn directory_name(index: usize) -> String {
    format!("d{index:04}")
}

fn file_name(file: usize) -> String {
    format!("f{file:05}.txt")
}

/// `bytes` bytes of ASCII text for generated file `file`: a first line naming
/// [`GREP_TARGET`], then numbered lines, cut at `bytes`.
fn file_text(file: usize, bytes: usize) -> Vec<u8> {
    let mut text = format!("// {GREP_TARGET} file {file}\n");
    let mut line = 0;
    while text.len() < bytes {
        line += 1;
        let _ = writeln!(text, "file {file} line {line}: iron ore and charcoal go in at the top");
    }
    text.truncate(bytes);
    text.into_bytes()
}

/// One directory the host walk has entered and not yet staged.
struct Frame {
    /// The entry name it takes in its parent; `None` for the root.
    name: Option<Name>,
    /// Its path relative to the root, `/`-separated; empty for the root.
    relative: String,
    /// The entries not yet visited, in reverse name order so `pop` walks
    /// them in order.
    pending: Vec<PathBuf>,
    entries: BTreeMap<Name, Node>,
}

impl Frame {
    fn enter(name: Option<Name>, relative: String, path: &HostPath) -> Result<Self, Box<dyn Error>> {
        let mut pending = fs::read_dir(path)
            .and_then(|listing| listing.map(|entry| entry.map(|entry| entry.path())).collect::<Result<Vec<_>, _>>())
            .map_err(|error| format!("list {}: {error}", path.display()))?;
        pending.sort_unstable_by(|left, right| right.cmp(left));
        Ok(Self { name, relative, pending, entries: BTreeMap::new() })
    }

    fn child(&self, name: &Name) -> String {
        if self.relative.is_empty() {
            name.as_str().to_owned()
        } else {
            format!("{}/{}", self.relative, name.as_str())
        }
    }
}

/// The host directory `root` as a tree, walked with an explicit stack and
/// staged bottom-up: `.git` and `target` are skipped, a symlink is a
/// `Node::Symlink`, and a file with an execute bit is a `Node::Executable`.
/// The first UTF-8 file walked is the read probe.
fn directory(batch: &mut Batch, root: &HostPath) -> Result<(Ref<Tree>, Probe), Box<dyn Error>> {
    let mut read = None;
    let mut stack = vec![Frame::enter(None, String::new(), root)?];
    while let Some(top) = stack.last_mut() {
        let Some(path) = top.pending.pop() else {
            let Frame { name, entries, .. } = stack.pop().ok_or("the walk's stack is not empty")?;
            let tree = batch.stage_encoded(&Tree::new(entries))?;
            if let (Some(parent), Some(name)) = (stack.last_mut(), name) {
                parent.entries.insert(name, Node::Directory(tree));
            } else {
                let read = read.ok_or_else(|| format!("{} holds no UTF-8 file to read", root.display()))?;
                return Ok((tree, Probe { read, grep: GREP_TARGET.to_owned() }));
            }
            continue;
        };
        let raw = path.file_name().and_then(|name| name.to_str());
        if raw.is_some_and(|raw| SKIPPED.contains(&raw)) {
            continue;
        }
        let name = raw
            .and_then(|name| Name::new(name).ok())
            .ok_or_else(|| format!("{} is not a valid tree entry name", path.display()))?;
        let metadata = fs::symlink_metadata(&path).map_err(|error| format!("stat {}: {error}", path.display()))?;
        let kind = metadata.file_type();
        if kind.is_dir() {
            let relative = top.child(&name);
            stack.push(Frame::enter(Some(name), relative, &path)?);
            continue;
        }
        let node = if kind.is_symlink() {
            let target = fs::read_link(&path).map_err(|error| format!("read link {}: {error}", path.display()))?;
            let target = target
                .to_str()
                .and_then(|target| Path::new(target).ok())
                .ok_or_else(|| format!("{} links to a target a tree cannot hold", path.display()))?;
            Node::Symlink(target)
        } else if kind.is_file() {
            let bytes = fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
            if read.is_none() && str::from_utf8(&bytes).is_ok() {
                read = Some(top.child(&name));
            }
            let blob = batch.stage_bytes(&bytes);
            if executable(&metadata) {
                Node::Executable(blob)
            } else {
                Node::File(blob)
            }
        } else {
            return Err(format!("{} is neither a file, a directory, nor a symlink", path.display()).into());
        };
        top.entries.insert(name, node);
    }
    Err("the walk ended without staging the root".into())
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &fs::Metadata) -> bool {
    false
}
