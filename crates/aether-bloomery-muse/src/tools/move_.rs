//! `tree.move`: move an entry to a new path.

use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Edited, Env, NoDetail, Program, Tooled, program};
use aether_bloomery_workspace::TreePath;

use crate::tools::read_args;
use crate::tools::spine::relocate;

/// What `tree.move` is asked to move.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.tree.move.args")]
pub struct MoveArgs {
    /// The entry to move, relative to the tree's root, `/`-separated, with no
    /// `.` or `..` segment.
    from: TreePath,
    /// The new path, relative to the tree's root, `/`-separated, with no
    /// `.` or `..` segment. Missing directories on the way are created.
    to: TreePath,
}

impl MoveArgs {
    /// Move the entry at `from` to `to`.
    #[must_use]
    pub fn new(from: TreePath, to: TreePath) -> Self {
        Self { from, to }
    }
}

/// The `tree.move` program.
pub struct TreeMove;

/// Moves one entry of the tree to a new path: a file, an executable, a
/// symlink, or a directory with its whole subtree.
///
/// The moved node keeps its citation, so nothing below it is restaged. A
/// directory the move leaves empty stays. A path that names nothing, an
/// existing destination, a destination equal to or inside the source, a
/// source or destination spine that runs through a file, or invalid arguments
/// return the tree unchanged with a summary saying why.
#[program]
impl Program for TreeMove {
    const NAME: &'static str = "tree.move";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Move one file, symlink, or directory with its contents to a new path.";
    type Input = Tooled<MoveArgs>;
    type Result = Edited;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        let (tree, detail) = (input.tree(), env.stage_encoded(&NoDetail)?);
        let unchanged = |summary: String| Ok(Edited::new(tree, summary, detail));
        let args = match read_args(&mut env, input.args()).await? {
            Ok(args) => args,
            Err(invalid) => return unchanged(format!("{invalid}, so nothing changed.")),
        };

        let root = match relocate(&mut env, tree, &args.from, &args.to).await? {
            Ok(root) => root,
            Err(moved) => return unchanged(moved.summary()),
        };
        Ok(Edited::new(root, format!("Moved {} to {}.", args.from.as_str(), args.to.as_str()), detail))
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Node, Tree};
    use aether_bloomery_program::Edited;
    use serde_json::json;

    use super::{MoveArgs, TreeMove};
    use crate::session::fixture::{SmallTree, name, no_detail, path, run_async};

    #[test]
    fn a_moved_executable_stays_executable_and_a_symlink_keeps_its_target() {
        // Catches a move that drops the executable bit or rewrites a symlink target.
        let small = SmallTree::new();
        let (input, closure) = small.call(&MoveArgs::new(path("run"), path("bin/run")));
        let (edited, store) = run_async::<TreeMove>(&input, closure).expect("moves");
        assert_eq!(edited.summary(), "Moved run to bin/run.");
        let root: Tree = store.value(edited.tree());
        let run = small.root().entries().get(&name("run")).expect("the executable");
        let bin = root.entries().get(&name("bin")).expect("the new directory");
        let Node::Directory(bin) = bin else {
            panic!("bin is a directory");
        };
        let bin: Tree = store.value(*bin);
        assert_eq!(bin.entries().get(&name("run")), Some(run));

        let (input, closure) = small.call(&MoveArgs::new(path("link"), path("src/link")));
        let (edited, store) = run_async::<TreeMove>(&input, closure).expect("moves");
        let root: Tree = store.value(edited.tree());
        let link = small.root().entries().get(&name("link")).expect("the symlink");
        let Some(Node::Directory(src)) = root.entries().get(&name("src")) else {
            panic!("src stays a directory");
        };
        let src: Tree = store.value(*src);
        assert_eq!(src.entries().get(&name("link")), Some(link));
    }

    #[test]
    fn a_moved_directory_keeps_its_subtree_citation() {
        // Catches a directory move that restages its subtree instead of keeping its citation.
        let small = SmallTree::new();
        let (input, closure) = small.call(&MoveArgs::new(path("src"), path("dst")));
        let (edited, store) = run_async::<TreeMove>(&input, closure).expect("moves");
        assert_eq!(edited.summary(), "Moved src to dst.");

        let root: Tree = store.value(edited.tree());
        let src = small.root().entries().get(&name("src")).expect("the directory");
        assert_eq!(root.entries().get(&name("dst")), Some(src));
        assert!(!root.entries().contains_key(&name("src")));
    }

    #[test]
    fn a_move_it_cannot_make_is_a_result_that_leaves_the_tree_unchanged() {
        // Catches a model's mistake refused as a fault, which would end the session, and one that changes the tree.
        let small = SmallTree::new();
        for (from, to, summary) in [
            ("missing", "dst", "Nothing is at missing, so nothing changed."),
            ("README/x", "dst", "README is not a directory, so nothing changed."),
            ("README", "src/lib.rs", "src/lib.rs already exists, so nothing changed."),
            ("README", "README", "README is the same path, so nothing changed."),
            ("src", "src/nested/lib.rs", "src/nested/lib.rs is inside src, so nothing changed."),
            ("README", "run/x", "run is not a directory, so nothing changed."),
        ] {
            let args = MoveArgs::new(path(from), path(to));
            let (input, closure) = small.call(&args);
            let edited = run_async::<TreeMove>(&input, closure).map(|(edited, _)| edited);
            assert_eq!(edited, Ok(Edited::new(small.tree(), summary, no_detail())), "{from} to {to}");
        }

        let raw = json!({ "from": "../x", "to": "dst" });
        let (input, closure) = small.call_json::<MoveArgs>(&raw);
        let (edited, _) = run_async::<TreeMove>(&input, closure).expect("invalid arguments are a result");
        assert_eq!(edited.tree(), small.tree());
        assert!(edited.summary().starts_with("The arguments are invalid"), "{}", edited.summary());
    }
}
