//! The spine of a path in a tree: the directories from the root down to the
//! path's parent, read on the way down and rebuilt on the way up.
//!
//! Both walks are iterative, one frame per segment; a [`TreePath`] has at
//! most 512. Every entry off the spine keeps its citation, so a placed or
//! removed node restages only the directories it passes through, and every
//! artifact it stages is reachable from the root it returns.

use aether_bloomery_kinds::{Name, Node, Ref, Refusal, Tree};
use aether_bloomery_program::{Async, Env};
use aether_bloomery_workspace::TreePath;

/// Why a path names no entry the tool can use: the first prefix of the path
/// that is missing or is not a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// Nothing is at this prefix.
    Missing { at: String },
    /// This prefix names a file, an executable, or a symlink, and the path
    /// goes on below it.
    NotADirectory { at: String },
}

impl Blocked {
    /// The sentence a read-only tool returns when the path is blocked.
    pub fn describe(&self) -> String {
        match self {
            Self::Missing { at } => format!("Nothing is at {at}."),
            Self::NotADirectory { at } => format!("{at} is not a directory."),
        }
    }

    /// The sentence a tool returns when the path is blocked and nothing
    /// changed.
    pub fn summary(&self) -> String {
        let described = self.describe();
        format!("{}, so nothing changed.", described.strip_suffix('.').unwrap_or(&described))
    }
}

/// The directories on a path's spine: `dirs[i]` holds `names[i]`.
struct Spine {
    names: Vec<Name>,
    dirs: Vec<Tree>,
}

impl Spine {
    /// The path up to and including segment `index`.
    fn prefix(&self, index: usize) -> String {
        self.names[..=index].iter().map(Name::as_str).collect::<Vec<_>>().join("/")
    }

    /// The directory holding the path's last segment, and that segment.
    fn parent(&self) -> (&Tree, &Name) {
        let (Some(dir), Some(name)) = (self.dirs.last(), self.names.last()) else {
            unreachable!("a tree path has at least one segment, and the walk starts from the root");
        };
        (dir, name)
    }
}

/// Read the directories from `root` down to `path`'s parent. With
/// `create_dirs`, a missing directory reads as an empty one.
async fn descend(
    env: &mut Env<Async>,
    root: Ref<Tree>,
    path: &TreePath,
    create_dirs: bool,
) -> Result<Result<Spine, Blocked>, Refusal> {
    let names: Vec<Name> =
        path.as_str().split('/').map(|segment| Name::new(segment).expect("a tree path's segments are names")).collect();
    let mut spine = Spine { dirs: vec![env.read(root).await?], names };
    for index in 0..spine.names.len() - 1 {
        let entry = spine.dirs[index].entries().get(&spine.names[index]).cloned();
        let dir = match entry {
            Some(Node::Directory(dir)) => env.read(dir).await?,
            None if create_dirs => Tree::empty(),
            None => return Ok(Err(Blocked::Missing { at: spine.prefix(index) })),
            Some(Node::File(_) | Node::Executable(_) | Node::Symlink(_)) => {
                return Ok(Err(Blocked::NotADirectory { at: spine.prefix(index) }));
            }
        };
        spine.dirs.push(dir);
    }
    Ok(Ok(spine))
}

/// The entry `path` names in `root`.
///
/// # Errors
///
/// The [`Refusal`] of a directory on the spine that the store cannot read.
pub async fn leaf(env: &mut Env<Async>, root: Ref<Tree>, path: &TreePath) -> Result<Result<Node, Blocked>, Refusal> {
    Ok(descend(env, root, path, false).await?.and_then(|spine| {
        let (dir, name) = spine.parent();
        dir.entries().get(name).cloned().ok_or_else(|| Blocked::Missing { at: path.as_str().into() })
    }))
}

/// The directory `path` names in `root`, or `root` itself when `path` is
/// `None`, with the path it is shown under: empty for the root. A path that
/// is blocked or names no directory is a message.
///
/// # Errors
///
/// The [`Refusal`] of a directory on the way that the store cannot read.
pub async fn directory(
    env: &mut Env<Async>,
    root: Ref<Tree>,
    path: Option<&TreePath>,
) -> Result<Result<(String, Tree), String>, Refusal> {
    let Some(path) = path else {
        return Ok(Ok((String::new(), env.read(root).await?)));
    };
    let shown = path.as_str();
    Ok(match leaf(env, root, path).await? {
        Ok(Node::Directory(dir)) => Ok((shown.into(), env.read(dir).await?)),
        Ok(Node::File(_) | Node::Executable(_)) => Err(format!("{shown} is a file; use tree.read.")),
        Ok(Node::Symlink(target)) => Err(format!("{shown} is a symlink to {}.", target.as_str())),
        Err(blocked) => Err(blocked.describe()),
    })
}

/// The root of `root` with `node` at `path`, replacing any entry there,
/// staging every directory rebuilt on the way. With `create_dirs`, a missing
/// directory on the spine is created empty. Nothing is staged when the path
/// is blocked.
///
/// # Errors
///
/// The [`Refusal`] of a directory on the spine that the store cannot read,
/// or of a rebuilt directory that does not encode.
pub async fn place(
    env: &mut Env<Async>,
    root: Ref<Tree>,
    path: &TreePath,
    node: Node,
    create_dirs: bool,
) -> Result<Result<Ref<Tree>, Blocked>, Refusal> {
    let spine = match descend(env, root, path, create_dirs).await? {
        Ok(spine) => spine,
        Err(blocked) => return Ok(Err(blocked)),
    };

    Ok(Ok(rebuild(env, spine, Some(node))?))
}

/// The root of `root` without the entry at `path`, whatever it is, a
/// directory with its whole subtree: the inverse of [`place`]. A directory
/// left empty stays. Nothing is staged when the path is blocked or names
/// nothing.
///
/// # Errors
///
/// The [`Refusal`] of a directory on the spine that the store cannot read,
/// or of a rebuilt directory that does not encode.
pub async fn remove(
    env: &mut Env<Async>,
    root: Ref<Tree>,
    path: &TreePath,
) -> Result<Result<Ref<Tree>, Blocked>, Refusal> {
    let spine = match descend(env, root, path, false).await? {
        Ok(spine) => spine,
        Err(blocked) => return Ok(Err(blocked)),
    };

    let (dir, name) = spine.parent();
    let present = dir.entries().contains_key(name);
    if !present {
        return Ok(Err(Blocked::Missing { at: path.as_str().into() }));
    }

    Ok(Ok(rebuild(env, spine, None)?))
}

/// The root of the spine with its last entry set to `last`, or removed when
/// `last` is `None`, staging every directory rebuilt on the way up.
fn rebuild(env: &mut Env<Async>, spine: Spine, last: Option<Node>) -> Result<Ref<Tree>, Refusal> {
    let Spine { names, dirs } = spine;
    let mut child = last;
    let mut root = None;
    for (name, dir) in names.into_iter().zip(dirs).rev() {
        let mut entries = dir.entries().clone();
        match child {
            Some(node) => entries.insert(name, node),
            None => entries.remove(&name),
        };

        let staged = env.stage_encoded(&Tree::new(entries))?;
        child = Some(Node::Directory(staged));
        root = Some(staged);
    }
    Ok(root.expect("a spine has at least the root directory"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use aether_bloomery_kinds::{Node, OpaqueBytes, Ref, Tree};
    use aether_bloomery_program::Edited;

    use crate::session::fixture::{SmallTree, name, no_detail, path, run_async};
    use crate::tools::edit::{EditArgs, TreeEdit};
    use crate::tools::remove::{RemoveArgs, TreeRemove};
    use crate::tools::write::{TreeWrite, WriteArgs};

    #[test]
    fn a_placed_node_rebuilds_only_its_spine_and_creates_missing_directories() {
        // Catches a rebuild that restages or drops an entry off the spine, and a missing directory on the spine
        // that is not created.
        let small = SmallTree::new();
        let (input, closure) = small.call(&WriteArgs::new(path("src/new/deep.rs"), "fn deep() {}"));
        let (edited, store) = run_async::<TreeWrite>(&input, closure).expect("writes");

        let root: Tree = store.value(edited.tree());
        assert_eq!(root.entries().get(&name("README")), small.root().entries().get(&name("README")), "README kept");
        let Some(Node::Directory(src)) = root.entries().get(&name("src")) else {
            panic!("src stays a directory");
        };
        let src: Tree = store.value(*src);
        assert_eq!(src.entries().get(&name("lib.rs")), Some(&Node::File(Ref::of_bytes(SmallTree::LIB))), "lib kept");
        let Some(Node::Directory(new)) = src.entries().get(&name("new")) else {
            panic!("src/new is created");
        };
        let new: Tree = store.value(*new);
        let deep = BTreeMap::from([(name("deep.rs"), Node::File(Ref::<OpaqueBytes>::of_bytes(b"fn deep() {}")))]);
        assert_eq!(new, Tree::new(deep));
    }

    #[test]
    fn a_blocked_path_names_its_first_bad_prefix_and_changes_nothing() {
        // Catches a blocked walk that names the wrong segment, and one that stages a partial rebuild.
        let small = SmallTree::new();
        for (at, summary) in [
            ("README/x", "README is not a directory, so nothing changed."),
            ("src/lib.rs/x/y", "src/lib.rs is not a directory, so nothing changed."),
            ("src/missing/x", "Nothing is at src/missing, so nothing changed."),
        ] {
            let (input, closure) = small.call(&EditArgs::new(path(at), "a", "b"));
            let (edited, _) = run_async::<TreeEdit>(&input, closure).expect("a result");
            assert_eq!(edited, Edited::new(small.tree(), summary, no_detail()), "{at}");
        }
    }

    #[test]
    fn a_removed_node_rebuilds_only_its_spine_and_keeps_every_other_entry() {
        // Catches a removal that drops or restages an entry off the spine, and a rebuilt directory the returned
        // root does not reach.
        let small = SmallTree::new();
        let (input, closure) = small.call(&RemoveArgs::new(path("src/lib.rs")));
        let (edited, store) = run_async::<TreeRemove>(&input, closure).expect("removes");

        let root: Tree = store.value(edited.tree());
        for kept in ["README", "run", "link", "blob.bin"] {
            assert_eq!(root.entries().get(&name(kept)), small.root().entries().get(&name(kept)), "{kept} kept");
        }
        let Some(Node::Directory(src)) = root.entries().get(&name("src")) else {
            panic!("src stays a directory");
        };
        let src: Tree = store.value(*src);
        assert_eq!(src, Tree::new(BTreeMap::new()), "an emptied directory stays");
    }

    #[test]
    fn a_blocked_removal_names_its_first_bad_prefix_and_changes_nothing() {
        // Catches a removal that treats a missing entry or a path through a file as a fault, or stages a partial
        // rebuild.
        let small = SmallTree::new();
        for (at, summary) in [
            ("missing", "Nothing is at missing, so nothing changed."),
            ("src/missing", "Nothing is at src/missing, so nothing changed."),
            ("README/x", "README is not a directory, so nothing changed."),
            ("nope/x/y", "Nothing is at nope, so nothing changed."),
        ] {
            let (input, closure) = small.call(&RemoveArgs::new(path(at)));
            let (edited, _) = run_async::<TreeRemove>(&input, closure).expect("a result");
            assert_eq!(edited, Edited::new(small.tree(), summary, no_detail()), "{at}");
        }
    }
}
