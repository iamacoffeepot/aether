//! `tree.edit`: replace one exact occurrence of a text in a file.

mod miss;

use aether_bloomery_kinds::{Mode, Node, Refusal};
use aether_bloomery_program::{Async, Edited, Env, NoDetail, Program, Tooled, program};
use aether_bloomery_workspace::TreePath;
use aether_data::Ref;

use crate::tools::edit::miss::{Matches, hint, occurrence};
use crate::tools::spine::{leaf, place};
use crate::tools::{MAX_TEXT_BYTES, read_args};

/// What `tree.edit` is asked to change.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.tree.edit.args")]
pub struct EditArgs {
    /// The file to edit, relative to the tree's root, `/`-separated, with no
    /// `.` or `..` segment.
    path: TreePath,
    /// The exact text to replace. It must occur exactly once in the file and
    /// may not be empty. At most 1 MiB.
    old: String,
    /// The text to put in its place. At most 1 MiB.
    new: String,
}

impl EditArgs {
    /// Replace the one occurrence of `old` in the file at `path` with `new`.
    #[must_use]
    pub fn new(path: TreePath, old: impl Into<String>, new: impl Into<String>) -> Self {
        Self { path, old: old.into(), new: new.into() }
    }
}

/// The `tree.edit` program.
pub struct TreeEdit;

/// Replaces one exact occurrence of a text in a UTF-8 file of the tree.
///
/// The old text is the file's own text, as `tree.read` shows it after each
/// line's number and tab. It must occur exactly once in the file, counting
/// occurrences that overlap. The file keeps its executable bit. When the edit
/// cannot be made, the tree is returned unchanged and the summary says why;
/// when the old text does not occur, the summary says where it stops matching
/// the file if one place is unambiguous.
#[program]
impl Program for TreeEdit {
    const NAME: &'static str = "tree.edit";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Replace one exact occurrence of a text in a file of the tree.";
    type Input = Tooled<EditArgs>;
    type Result = Edited;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        let (tree, detail) = (input.tree(), env.stage_encoded(&NoDetail)?);
        let unchanged = |summary: String| Ok(Edited::new(tree, summary, detail));
        let args = match read_args(&mut env, input.args()).await? {
            Ok(args) => args,
            Err(invalid) => return unchanged(format!("{invalid}, so nothing changed.")),
        };
        let path = args.path.as_str();
        if args.old.len() > MAX_TEXT_BYTES || args.new.len() > MAX_TEXT_BYTES {
            return unchanged("The old or the new text is longer than 1 MiB, so nothing changed.".into());
        }
        if args.old.is_empty() {
            return unchanged("The old text is empty, so nothing changed.".into());
        }

        let (blob, executable) = match leaf(&mut env, tree, &args.path).await? {
            Ok(Node::File(blob)) => (blob, false),
            Ok(Node::Executable(blob)) => (blob, true),
            Ok(Node::Directory(_)) => return unchanged(format!("{path} is a directory, so nothing changed.")),
            Ok(Node::Symlink(_)) => return unchanged(format!("{path} is a symlink, so nothing changed.")),
            Err(blocked) => return unchanged(blocked.summary()),
        };
        let Ok(text) = String::from_utf8(env.read_payload(blob.erase()).await?) else {
            return unchanged(format!("{path} is not UTF-8 text, so nothing changed."));
        };
        let edited = match replace_once(&text, &args.old, &args.new) {
            Ok(edited) => edited,
            Err(Matches::None) => {
                let missed = format!("The old text does not occur in {path}, so nothing changed.");
                let summary = match hint(&text, &args.old) {
                    Some(hint) => format!("{missed} {hint}"),
                    None => missed,
                };
                return unchanged(summary);
            }
            Err(Matches::Several) => {
                return unchanged(format!("The old text occurs more than once in {path}, so nothing changed."));
            }
        };

        let blob = Ref::of_bytes(edited.as_bytes());
        let node = if executable {
            Node::Executable(blob)
        } else {
            Node::File(blob)
        };
        let root = match place(&mut env, tree, &args.path, node, false).await? {
            Ok(root) => root,
            Err(blocked) => return unchanged(blocked.summary()),
        };
        env.stage_bytes(edited.as_bytes());
        Ok(Edited::new(root, format!("Edited {path}."), detail))
    }
}

/// `text` with its one occurrence of the non-empty `old` replaced by `new`.
fn replace_once(text: &str, old: &str, new: &str) -> Result<String, Matches> {
    let first = occurrence(text, old)?;
    Ok([&text[..first], new, &text[first + old.len()..]].concat())
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Node, Tree};
    use aether_bloomery_program::Edited;
    use aether_data::Ref;
    use serde_json::json;

    use super::{EditArgs, Matches, TreeEdit, replace_once};
    use crate::session::fixture::{SmallTree, name, no_detail, path, run_async};

    #[test]
    fn an_old_text_is_replaced_only_when_it_occurs_exactly_once() {
        // Catches a count that misses an overlapping second occurrence, and a replacement at the wrong offset.
        assert_eq!(replace_once("a smelted bloom", "bloom", "ingot"), Ok("a smelted ingot".into()));
        assert_eq!(replace_once("é bloom", "bloom", "x"), Ok("é x".into()));
        assert_eq!(replace_once("bloom", "iron", "x"), Err(Matches::None));
        assert_eq!(replace_once("bloom bloom", "bloom", "x"), Err(Matches::Several));
        assert_eq!(replace_once("aaa", "aa", "b"), Err(Matches::Several));
        assert_eq!(replace_once("éé", "é", "e"), Err(Matches::Several));
    }

    #[test]
    fn an_edit_keeps_the_executable_bit_and_rebuilds_the_tree() {
        // Catches an edit that drops the executable bit, or returns a tree without the edited file.
        let small = SmallTree::new();
        let (input, closure) = small.call(&EditArgs::new(path("run"), "smelt", "smelt --hot"));
        let (edited, store) = run_async::<TreeEdit>(&input, closure).expect("edits");

        assert_eq!(edited.summary(), "Edited run.");
        let root: Tree = store.value(edited.tree());
        let run = Node::Executable(Ref::of_bytes(b"#!/bin/sh\nsmelt --hot\n"));
        assert_eq!(root.entries().get(&name("run")), Some(&run));
    }

    #[test]
    fn a_failed_edit_is_a_result_that_leaves_the_tree_unchanged() {
        // Catches a model's mistake refused as a fault, which would end the session, and one that changes the tree.
        let small = SmallTree::new();
        let cases = [
            (EditArgs::new(path("README"), "iron", "x"), "The old text does not occur in README, so nothing changed."),
            (
                EditArgs::new(path("README"), "# Bloomery iron", "x"),
                "The old text does not occur in README, so nothing changed. Its first 10 bytes occur once, ending on \
                 line 1; there the file continues with:\n(end of line)\nbut the old text continues with:\n iron",
            ),
            (EditArgs::new(path("blob.bin"), "a", "b"), "blob.bin is not UTF-8 text, so nothing changed."),
            (EditArgs::new(path("src"), "a", "b"), "src is a directory, so nothing changed."),
            (EditArgs::new(path("link"), "a", "b"), "link is a symlink, so nothing changed."),
            (EditArgs::new(path("README"), "", "b"), "The old text is empty, so nothing changed."),
        ];
        for (args, summary) in cases {
            let (input, closure) = small.call(&args);
            let edited = run_async::<TreeEdit>(&input, closure).map(|(edited, _)| edited);
            assert_eq!(edited, Ok(Edited::new(small.tree(), summary, no_detail())));
        }

        let raw = json!({ "path": "../x", "old": "a", "new": "b" });
        let (input, closure) = small.call_json::<EditArgs>(&raw);
        let (edited, _) = run_async::<TreeEdit>(&input, closure).expect("invalid arguments are a result");
        assert_eq!(edited.tree(), small.tree());
        assert!(edited.summary().starts_with("The arguments are invalid"), "{}", edited.summary());
    }
}
