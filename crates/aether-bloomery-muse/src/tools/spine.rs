//! The spine of a path in a tree: the directories from the root down to the
//! path's parent, read on the way down and rebuilt on the way up.
//!
//! Both walks are iterative, one frame per segment; a [`TreePath`] has at
//! most 512. Every entry off the spine keeps its citation, so a placed,
//! removed, or moved node restages only the directories it passes through,
//! and every artifact it stages is reachable from the root it returns.

use aether_bloomery_kinds::{Name, Node, Refusal, Tree};
use aether_bloomery_program::{Async, Env};
use aether_bloomery_workspace::TreePath;
use aether_data::Ref;

use crate::tools::view::Family;

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

/// Why a move names no new tree: the source or the destination cannot be
/// used, the destination already holds an entry, or the destination is the
/// source or lies inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Moved {
    /// The source spine is blocked or names nothing.
    Source(Blocked),
    /// The destination spine runs through a file, an executable, or a
    /// symlink.
    Destination(Blocked),
    /// An entry already lives at the destination.
    Exists { at: String },
    /// The source and the destination are the same path.
    Same { at: String },
    /// The destination lies inside the source.
    Inside { from: String, to: String },
}

impl Moved {
    /// The sentence a tool returns when the move cannot be made and nothing
    /// changed.
    pub fn summary(&self) -> String {
        match self {
            Self::Source(blocked) | Self::Destination(blocked) => blocked.summary(),
            Self::Exists { at } => format!("{at} already exists, so nothing changed."),
            Self::Same { at } => format!("{at} is the same path, so nothing changed."),
            Self::Inside { from, to } => format!("{to} is inside {from}, so nothing changed."),
        }
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
/// is blocked or names no directory is a message, whose hint names `family`'s
/// reading tool.
///
/// # Errors
///
/// The [`Refusal`] of a directory on the way that the store cannot read.
pub async fn directory(
    env: &mut Env<Async>,
    root: Ref<Tree>,
    path: Option<&TreePath>,
    family: Family,
) -> Result<Result<(String, Tree), String>, Refusal> {
    let Some(path) = path else {
        return Ok(Ok((String::new(), env.read(root).await?)));
    };
    let shown = path.as_str();
    Ok(match leaf(env, root, path).await? {
        Ok(Node::Directory(dir)) => Ok((shown.into(), env.read(dir).await?)),
        Ok(Node::File(_) | Node::Executable(_)) => Err(format!("{shown} is a file; use {}.", family.read())),
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

/// The root of `root` with the entry at `from` moved to `to`, keeping the
/// moved node's citation, so nothing below it is restaged. Missing
/// directories on `to`'s spine are created empty. A directory the move leaves
/// empty stays. Nothing is staged when the move cannot be made: the source
/// spine is blocked or names nothing, the destination spine runs through a
/// file, the destination already holds an entry, or the destination is the
/// source or lies inside it.
///
/// Both spines descend from the one original `root`, so only original
/// directories are read. The rebuild removes the entry from `from`'s spine
/// bottom-up up to the deepest directory both spines share, keeping that
/// directory in memory, then places the node down `to`'s spine bottom-up to
/// the root, so every directory staged is on the returned root's spine.
///
/// Both walks are iterative, one frame per segment.
///
/// # Errors
///
/// The [`Refusal`] of a directory on either spine that the store cannot read,
/// or of a rebuilt directory that does not encode.
pub async fn relocate(
    env: &mut Env<Async>,
    root: Ref<Tree>,
    from: &TreePath,
    to: &TreePath,
) -> Result<Result<Ref<Tree>, Moved>, Refusal> {
    if from.as_str() == to.as_str() {
        return Ok(Err(Moved::Same { at: from.as_str().into() }));
    }
    let from_split: Vec<&str> = from.as_str().split('/').collect();
    let to_split: Vec<&str> = to.as_str().split('/').collect();
    let mut shared_depth = 0;
    for (from_segment, to_segment) in from_split.iter().zip(to_split.iter()) {
        let same = from_segment == to_segment;
        if !same {
            break;
        }
        shared_depth += 1;
    }
    if shared_depth == from_split.len() {
        return Ok(Err(Moved::Inside { from: from.as_str().into(), to: to.as_str().into() }));
    }

    let from_spine = match descend(env, root, from, false).await? {
        Ok(spine) => spine,
        Err(blocked) => return Ok(Err(Moved::Source(blocked))),
    };
    let to_spine = match descend(env, root, to, true).await? {
        Ok(spine) => spine,
        Err(blocked) => return Ok(Err(Moved::Destination(blocked))),
    };

    let node = {
        let (dir, name) = from_spine.parent();
        match dir.entries().get(name).cloned() {
            Some(node) => node,
            None => return Ok(Err(Moved::Source(Blocked::Missing { at: from.as_str().into() }))),
        }
    };
    {
        let (dir, name) = to_spine.parent();
        if dir.entries().contains_key(name) {
            return Ok(Err(Moved::Exists { at: to.as_str().into() }));
        }
    }

    let Spine { names: from_names, dirs: from_dirs } = from_spine;
    let Spine { names: to_names, dirs: to_dirs } = to_spine;

    let mut child: Option<Node> = None;
    let mut shared: Option<Tree> = None;
    let mut index = from_names.len();
    while index > shared_depth {
        index -= 1;
        let mut entries = from_dirs[index].entries().clone();
        match child.take() {
            None => {
                entries.remove(&from_names[index]);
            }
            Some(node) => {
                entries.insert(from_names[index].clone(), node);
            }
        }
        let tree = Tree::new(entries);
        if index == shared_depth {
            shared = Some(tree);
        } else {
            let staged = env.stage_encoded(&tree)?;
            child = Some(Node::Directory(staged));
        }
    }
    let shared = shared.expect("the shared directory is on the source spine");

    let mut child = Some(node);
    let mut root = None;
    let mut index = to_names.len();
    while index > 0 {
        index -= 1;
        let dir = if index == shared_depth {
            &shared
        } else {
            &to_dirs[index]
        };
        let mut entries = dir.entries().clone();
        entries.insert(to_names[index].clone(), child.take().expect("a placed child is always staged"));
        let staged = env.stage_encoded(&Tree::new(entries))?;
        if index == 0 {
            root = Some(staged);
        } else {
            child = Some(Node::Directory(staged));
        }
    }
    let root = root.expect("a tree path has at least one segment");

    Ok(Ok(root))
}

/// The root of the spine with its last entry set to `last`, or removed when
/// `last` is `None`, staging every directory rebuilt on the way up.
fn rebuild(env: &mut Env<Async>, spine: Spine, last: Option<Node>) -> Result<Ref<Tree>, Refusal> {
    let Spine { names, dirs } = spine;
    let mut child = last;
    for (name, dir) in names.into_iter().zip(dirs).rev() {
        let mut entries = dir.entries().clone();
        match child {
            Some(node) => entries.insert(name, node),
            None => entries.remove(&name),
        };
        child = Some(Node::Directory(env.stage_encoded(&Tree::new(entries))?));
    }

    let Some(Node::Directory(root)) = child else {
        unreachable!("the last rebuilt node is the root directory");
    };
    Ok(root)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use aether_bloomery_kinds::{Node, Tree};
    use aether_bloomery_program::Edited;
    use aether_data::{OpaqueBytes, Ref};

    use crate::session::fixture::{SmallTree, name, no_detail, path, run_async};
    use crate::tools::edit::{EditArgs, TreeEdit};
    use crate::tools::move_::{MoveArgs, TreeMove};
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

    #[test]
    fn a_move_within_one_directory_keeps_the_moved_citation() {
        // Catches a rename that restages the file's bytes or drops a sibling.
        let small = SmallTree::new();
        let (input, closure) = small.call(&MoveArgs::new(path("README"), path("NOTES")));
        let (edited, store) = run_async::<TreeMove>(&input, closure).expect("moves");
        assert_eq!(edited.summary(), "Moved README to NOTES.");

        let root: Tree = store.value(edited.tree());
        let readme = small.root().entries().get(&name("README")).expect("the file");
        assert_eq!(root.entries().get(&name("NOTES")), Some(readme));
        assert!(!root.entries().contains_key(&name("README")));
        for kept in ["run", "link", "blob.bin", "src"] {
            assert_eq!(root.entries().get(&name(kept)), small.root().entries().get(&name(kept)), "{kept} kept");
        }
    }

    #[test]
    fn a_move_between_sibling_directories_rebuilds_both_spines() {
        // Catches a move that drops the destination sibling or leaves the source entry behind, and a rebuilt
        // directory the returned root does not reach.
        let small = SmallTree::new();
        let (input, closure) = small.call(&MoveArgs::new(path("src/lib.rs"), path("dst/lib.rs")));
        let (edited, store) = run_async::<TreeMove>(&input, closure).expect("moves");
        assert_eq!(edited.summary(), "Moved src/lib.rs to dst/lib.rs.");

        let root: Tree = store.value(edited.tree());
        let lib = Node::File(Ref::of_bytes(SmallTree::LIB));
        let Some(Node::Directory(src)) = root.entries().get(&name("src")) else {
            panic!("src stays a directory");
        };
        let src: Tree = store.value(*src);
        assert_eq!(src, Tree::new(BTreeMap::new()), "an emptied directory stays");
        let Some(Node::Directory(dst)) = root.entries().get(&name("dst")) else {
            panic!("dst is created");
        };
        let dst: Tree = store.value(*dst);
        assert_eq!(dst.entries().get(&name("lib.rs")), Some(&lib), "the moved citation is kept");
    }

    #[test]
    fn a_move_out_of_a_nested_directory_to_the_root_rebuilds_both_spines() {
        // Catches a move that leaves the nested entry behind or drops a root sibling.
        let small = SmallTree::new();
        let (input, closure) = small.call(&MoveArgs::new(path("src/lib.rs"), path("lib.rs")));
        let (edited, store) = run_async::<TreeMove>(&input, closure).expect("moves");
        assert_eq!(edited.summary(), "Moved src/lib.rs to lib.rs.");

        let root: Tree = store.value(edited.tree());
        let lib = Node::File(Ref::of_bytes(SmallTree::LIB));
        assert_eq!(root.entries().get(&name("lib.rs")), Some(&lib));
        let Some(Node::Directory(src)) = root.entries().get(&name("src")) else {
            panic!("src stays a directory");
        };
        let src: Tree = store.value(*src);
        assert_eq!(src, Tree::new(BTreeMap::new()), "an emptied directory stays");
    }

    #[test]
    fn a_move_into_a_new_nested_directory_creates_missing_directories() {
        // Catches missing directories on the destination spine that are not created, and a moved file whose bytes
        // are restaged.
        let small = SmallTree::new();
        let (input, closure) = small.call(&MoveArgs::new(path("README"), path("new/deep/README")));
        let (edited, store) = run_async::<TreeMove>(&input, closure).expect("moves");
        assert_eq!(edited.summary(), "Moved README to new/deep/README.");

        let root: Tree = store.value(edited.tree());
        assert!(!root.entries().contains_key(&name("README")));
        let readme = small.root().entries().get(&name("README")).expect("the file");
        let Some(Node::Directory(new)) = root.entries().get(&name("new")) else {
            panic!("new is created");
        };
        let new: Tree = store.value(*new);
        let Some(Node::Directory(deep)) = new.entries().get(&name("deep")) else {
            panic!("new/deep is created");
        };
        let deep: Tree = store.value(*deep);
        assert_eq!(deep.entries().get(&name("README")), Some(readme));
    }

    #[test]
    fn a_blocked_move_leaves_the_tree_unchanged() {
        // Catches a move that treats a model's mistake as a fault, changes the tree, or stages a partial rebuild.
        let small = SmallTree::new();
        for (from, to, summary) in [
            ("missing", "dst", "Nothing is at missing, so nothing changed."),
            ("src/missing", "dst", "Nothing is at src/missing, so nothing changed."),
            ("README/x", "dst", "README is not a directory, so nothing changed."),
            ("README", "src/lib.rs", "src/lib.rs already exists, so nothing changed."),
            ("README", "README", "README is the same path, so nothing changed."),
            ("src/lib.rs", "README/x", "README is not a directory, so nothing changed."),
            ("src", "src/new/lib.rs", "src/new/lib.rs is inside src, so nothing changed."),
            ("README", "run/x", "run is not a directory, so nothing changed."),
        ] {
            let args = MoveArgs::new(path(from), path(to));
            let (input, closure) = small.call(&args);
            let edited = run_async::<TreeMove>(&input, closure).map(|(edited, _)| edited);
            assert_eq!(edited, Ok(Edited::new(small.tree(), summary, no_detail())), "{from} to {to}");
        }
    }
}
