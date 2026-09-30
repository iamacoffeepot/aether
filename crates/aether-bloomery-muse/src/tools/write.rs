//! `tree.write`: create a file or replace its whole text.

use aether_bloomery_kinds::{Mode, Node, Ref, Refusal};
use aether_bloomery_program::{Async, Edited, Env, Program, Tooled, program};
use aether_bloomery_workspace::TreePath;

use crate::tools::spine::{Blocked, leaf, place};
use crate::tools::{MAX_TEXT_BYTES, read_args};

/// What `tree.write` is asked to write.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.tree.write.args")]
pub struct WriteArgs {
    /// The file to write, relative to the tree's root, `/`-separated, with no
    /// `.` or `..` segment. Missing directories on the way are created.
    path: TreePath,
    /// The file's whole new text. At most 1 MiB.
    text: String,
}

impl WriteArgs {
    /// Write `text` as the whole file at `path`.
    #[must_use]
    pub fn new(path: TreePath, text: impl Into<String>) -> Self {
        Self { path, text: text.into() }
    }
}

/// The `tree.write` program.
pub struct TreeWrite;

/// Writes a file of the tree: creates it, with any missing directories on its
/// path, or replaces its whole text.
///
/// An executable file stays executable. A path that names a directory or a
/// symlink, or runs through a file, is not written: the tree is returned
/// unchanged and the summary says why.
#[program]
impl Program for TreeWrite {
    const NAME: &'static str = "tree.write";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Create or overwrite one file of the tree.";
    type Input = Tooled<WriteArgs>;
    type Result = Edited;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        let tree = input.tree();
        let unchanged = |summary: String| Ok(Edited::new(tree, summary));
        let args = match read_args(&mut env, input.args()).await? {
            Ok(args) => args,
            Err(summary) => return unchanged(summary),
        };
        let path = args.path.as_str();
        if args.text.len() > MAX_TEXT_BYTES {
            return unchanged("The text is longer than 1 MiB, so nothing changed.".into());
        }

        let blob = Ref::of_bytes(args.text.as_bytes());
        let node = match leaf(&mut env, tree, &args.path).await? {
            Ok(Node::File(_)) | Err(Blocked::Missing { .. }) => Node::File(blob),
            Ok(Node::Executable(_)) => Node::Executable(blob),
            Ok(Node::Directory(_)) => return unchanged(format!("{path} is a directory, so nothing changed.")),
            Ok(Node::Symlink(_)) => return unchanged(format!("{path} is a symlink, so nothing changed.")),
            Err(blocked @ Blocked::NotADirectory { .. }) => return unchanged(blocked.summary()),
        };
        let root = match place(&mut env, tree, &args.path, node, true).await? {
            Ok(root) => root,
            Err(blocked) => return unchanged(blocked.summary()),
        };
        env.stage_bytes(args.text.as_bytes());
        Ok(Edited::new(root, format!("Wrote {path}.")))
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Node, Ref, Tree};
    use aether_bloomery_program::Edited;

    use super::{TreeWrite, WriteArgs};
    use crate::session::fixture::{SmallTree, name, path, run_async};

    #[test]
    fn a_write_overwrites_a_file_and_keeps_its_executable_bit() {
        // Catches an overwrite that drops the executable bit or leaves the old text in place.
        let small = SmallTree::new();
        for (at, node) in
            [("run", Node::Executable(Ref::of_bytes(b"new"))), ("README", Node::File(Ref::of_bytes(b"new")))]
        {
            let (input, closure) = small.call(&WriteArgs::new(path(at), "new"));
            let (edited, store) = run_async::<TreeWrite>(&input, closure).expect("writes");
            assert_eq!(edited.summary(), format!("Wrote {at}."));
            let root: Tree = store.value(edited.tree());
            assert_eq!(root.entries().get(&name(at)), Some(&node));
        }
    }

    #[test]
    fn a_write_it_cannot_make_is_a_result_that_leaves_the_tree_unchanged() {
        // Catches a directory or symlink replaced by a file, and an over-cap text written or refused as a fault.
        let small = SmallTree::new();
        let over_cap = "a".repeat(super::MAX_TEXT_BYTES + 1);
        for (args, summary) in [
            (WriteArgs::new(path("src"), "x"), "src is a directory, so nothing changed."),
            (WriteArgs::new(path("link"), "x"), "link is a symlink, so nothing changed."),
            (WriteArgs::new(path("README"), over_cap), "The text is longer than 1 MiB, so nothing changed."),
        ] {
            let (input, closure) = small.call(&args);
            let edited = run_async::<TreeWrite>(&input, closure).map(|(edited, _)| edited);
            assert_eq!(edited, Ok(Edited::new(small.tree(), summary)));
        }
    }
}
