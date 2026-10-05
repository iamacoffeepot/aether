//! `tree.remove`: delete a file or a directory with its whole subtree.

use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Edited, Env, NoDetail, Program, Tooled, program};
use aether_bloomery_workspace::TreePath;

use crate::tools::read_args;
use crate::tools::spine::remove;

/// What `tree.remove` is asked to remove.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.tree.remove.args")]
pub struct RemoveArgs {
    /// The entry to remove, relative to the tree's root, `/`-separated, with no
    /// `.` or `..` segment.
    path: TreePath,
}

impl RemoveArgs {
    /// Remove the entry at `path`.
    #[must_use]
    pub fn new(path: TreePath) -> Self {
        Self { path }
    }
}

/// The `tree.remove` program.
pub struct TreeRemove;

/// Removes an entry of the tree: a file, an executable, a symlink, or a
/// directory with everything under it.
///
/// A directory the removal leaves empty stays. A path that names nothing, or
/// runs through a file, removes nothing: the tree is returned unchanged and
/// the summary says why.
#[program]
impl Program for TreeRemove {
    const NAME: &'static str = "tree.remove";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Remove one file, symlink, or directory with its contents from the tree.";
    type Input = Tooled<RemoveArgs>;
    type Result = Edited;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        let (tree, detail) = (input.tree(), env.stage_encoded(&NoDetail)?);
        let unchanged = |summary: String| Ok(Edited::new(tree, summary, detail));
        let args = match read_args(&mut env, input.args()).await? {
            Ok(args) => args,
            Err(invalid) => return unchanged(format!("{invalid}, so nothing changed.")),
        };

        let root = match remove(&mut env, tree, &args.path).await? {
            Ok(root) => root,
            Err(blocked) => return unchanged(blocked.summary()),
        };
        Ok(Edited::new(root, format!("Removed {}.", args.path.as_str()), detail))
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::Tree;
    use aether_bloomery_program::Edited;
    use serde_json::json;

    use super::{RemoveArgs, TreeRemove};
    use crate::session::fixture::{SmallTree, name, no_detail, path, run_async};

    #[test]
    fn a_remove_drops_a_file_an_executable_a_symlink_or_a_directory_and_nothing_else() {
        // Catches a remove that leaves its entry in place, drops a sibling, or refuses a kind of node.
        let small = SmallTree::new();
        for at in ["README", "run", "link", "src"] {
            let (input, closure) = small.call(&RemoveArgs::new(path(at)));
            let (edited, store) = run_async::<TreeRemove>(&input, closure).expect("removes");
            assert_eq!(edited.summary(), format!("Removed {at}."));

            let mut expected = small.root().entries().clone();
            expected.remove(&name(at));
            let root: Tree = store.value(edited.tree());
            assert_eq!(root, Tree::new(expected), "{at}");
        }
    }

    #[test]
    fn a_remove_it_cannot_make_is_a_result_that_leaves_the_tree_unchanged() {
        // Catches a missing path or a path through a file refused as a fault, and invalid arguments that refuse.
        let small = SmallTree::new();
        for (at, summary) in [
            ("missing", "Nothing is at missing, so nothing changed."),
            ("README/x", "README is not a directory, so nothing changed."),
        ] {
            let (input, closure) = small.call(&RemoveArgs::new(path(at)));
            let edited = run_async::<TreeRemove>(&input, closure).map(|(edited, _)| edited);
            assert_eq!(edited, Ok(Edited::new(small.tree(), summary, no_detail())), "{at}");
        }

        let (input, closure) = small.call_json::<RemoveArgs>(&json!({ "path": "../x" }));
        let (edited, _) = run_async::<TreeRemove>(&input, closure).expect("invalid arguments are a result");
        assert_eq!(edited.tree(), small.tree());
        assert!(edited.summary().starts_with("The arguments are invalid"), "{}", edited.summary());
    }
}
