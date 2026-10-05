//! `tree.list`: one directory level of the tree.

use aether_bloomery_kinds::{Mode, Name, Node, Refusal, Tree};
use aether_bloomery_program::{Async, Env, Program, Tooled, program};
use aether_bloomery_workspace::TreePath;
use aether_data::Ref;

use crate::tools::read_args;
use crate::tools::spine::directory;
use crate::tools::view::{Family, Lines, VIEW_MAX_BYTES, Viewed};

/// What `tree.list` is asked to list.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.tree.list.args")]
pub struct ListArgs {
    /// The directory to list, relative to the tree's root, `/`-separated,
    /// with no `.` or `..` segment. Leave it out to list the root.
    path: Option<TreePath>,
}

impl ListArgs {
    /// List the directory at `path`, or the root when `path` is `None`.
    #[must_use]
    pub const fn new(path: Option<TreePath>) -> Self {
        Self { path }
    }
}

/// The `tree.list` program.
pub struct TreeList;

/// Lists one directory of the tree: a line per entry, `<kind>\t<name>`, in
/// name order, where kind is `dir`, `file`, `exec`, or `link`, and a symlink
/// shows its target as `link\t<name> -> <target>`.
///
/// Only the directory is read, never a file. A path that names no directory
/// returns a sentence saying why.
#[program]
impl Program for TreeList {
    const NAME: &'static str = "tree.list";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "List the entries of one directory of the tree.";
    type Input = Tooled<ListArgs>;
    type Result = Viewed;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        list(&mut env, input.tree(), input.args(), Family::Tree).await
    }
}

/// `args`' directory of `root` as a listing, with hints naming `family`'s
/// tools.
///
/// # Errors
///
/// The [`Refusal`] of a directory the store cannot give.
pub(super) async fn list(
    env: &mut Env<Async>,
    root: Ref<Tree>,
    args: Ref<ListArgs>,
    family: Family,
) -> Result<Viewed, Refusal> {
    let args = match read_args(env, args).await? {
        Ok(args) => args,
        Err(invalid) => return Ok(Viewed::new(format!("{invalid}."))),
    };
    let (shown, dir) = match directory(env, root, args.path.as_ref(), family).await? {
        Ok(found) => found,
        Err(message) => return Ok(Viewed::new(message)),
    };
    if dir.entries().is_empty() {
        return Ok(Viewed::new(if shown.is_empty() {
            "The tree is empty.".into()
        } else {
            format!("{shown} is empty.")
        }));
    }

    let total = dir.entries().len();
    let mut lines = Lines::new(VIEW_MAX_BYTES);
    let shown_entries = dir.entries().iter().take_while(|(name, node)| lines.push(&entry(name, node))).count();
    Ok(lines.finish(|| format!("[showing {shown_entries} of {total} entries; list a subdirectory or grep]")))
}

/// One listing line: the entry's kind and name.
fn entry(name: &Name, node: &Node) -> String {
    let name = name.as_str();
    match node {
        Node::Directory(_) => format!("dir\t{name}"),
        Node::File(_) => format!("file\t{name}"),
        Node::Executable(_) => format!("exec\t{name}"),
        Node::Symlink(target) => format!("link\t{name} -> {}", target.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ListArgs, TreeList};
    use crate::session::fixture::{SmallTree, path, run_async};

    /// The text `tree.list` returns for `args` over the small tree.
    fn listed(small: &SmallTree, args: &ListArgs) -> String {
        let (input, closure) = small.call(args);
        let (viewed, _) = run_async::<TreeList>(&input, closure).expect("a listing is a result");
        viewed.text().to_owned()
    }

    #[test]
    fn a_listing_shows_each_entry_kind_and_a_symlink_target() {
        // Catches a wrong kind column, a symlink shown without its target, and entries out of name order.
        let small = SmallTree::new();
        assert_eq!(
            listed(&small, &ListArgs::new(None)),
            "file\tREADME\nfile\tblob.bin\nlink\tlink -> README\nexec\trun\ndir\tsrc"
        );
        assert_eq!(listed(&small, &ListArgs::new(Some(path("src")))), "file\tlib.rs");
    }

    #[test]
    fn a_path_that_names_no_directory_is_a_result_saying_why() {
        // Catches a model's mistake refused as a fault, which would end the session.
        let small = SmallTree::new();
        for (at, text) in [
            ("README", "README is a file; use tree.read."),
            ("link", "link is a symlink to README."),
            ("src/missing", "Nothing is at src/missing."),
            ("README/x", "README is not a directory."),
        ] {
            assert_eq!(listed(&small, &ListArgs::new(Some(path(at)))), text, "{at}");
        }

        let (input, closure) = small.call_json::<ListArgs>(&json!({ "path": "../x" }));
        let (viewed, _) = run_async::<TreeList>(&input, closure).expect("invalid arguments are a result");
        assert!(viewed.text().starts_with("The arguments are invalid"), "{}", viewed.text());
    }
}
