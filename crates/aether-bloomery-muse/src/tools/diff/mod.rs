//! The diff `tree.diff` renders: a line edit script, and its rendering as unified hunks.

mod file;
mod hunks;
mod myers;
mod walk;

use aether_bloomery_kinds::{Mode, Node, Refusal, Tree};
use aether_bloomery_program::{Async, Env, Program, Tooled, program};
use aether_bloomery_workspace::TreePath;
use aether_data::Ref;

use crate::tools::MAX_TEXT_BYTES;
use crate::tools::read_args;
use crate::tools::spine::leaf;
use crate::tools::view::{Lines, VIEW_MAX_BYTES, Viewed};

use file::block;
use walk::{Pair, changes};

/// The most edits one file's line diff keeps: past it the file is too
/// different to show.
pub const DIFF_MAX_EDITS: usize = 1000;

/// The most bytes one side of a file may hold for its line diff to be shown;
/// a longer file is named as too large.
pub const DIFF_MAX_FILE_BYTES: usize = MAX_TEXT_BYTES;

/// The most entries one diff compares, directories included.
pub const DIFF_MAX_ENTRIES: usize = 5000;

/// The most bytes of file text one diff reads: 16 MiB.
pub const DIFF_MAX_SCANNED_BYTES: usize = 16 << 20;

/// What `tree.diff` is asked to show.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.tree.diff.args")]
pub struct DiffArgs {
    /// The file or directory to diff, relative to the tree's root,
    /// `/`-separated, with no `.` or `..` segment. Leave it out to diff
    /// the whole tree.
    path: Option<TreePath>,
}

impl DiffArgs {
    /// Diff `path` (or the whole tree).
    #[must_use]
    pub const fn new(path: Option<TreePath>) -> Self {
        Self { path }
    }
}

/// The `tree.diff` program.
pub struct TreeDiff;

/// Compares the current tree with the tree the session opened on, listing
/// changed files as `M` / `A` / `D` then showing unified hunks in path order.
///
/// `path` narrows the diff to one file or directory. The diff stops at the
/// 64 KiB output cap, or after 5000 entries or 16 MiB of file text, and a
/// closing `[...]` line says which. A path that names nothing to diff returns
/// a sentence saying why.
#[program]
impl Program for TreeDiff {
    const NAME: &'static str = "tree.diff";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Show what changed in the tree since the session opened, as a unified diff.";
    type Input = Tooled<DiffArgs, Tree>;
    type Result = Viewed;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        diff(&mut env, input.bound(), input.tree(), input.args(), Budget::FULL).await
    }
}

/// How much of the trees one diff may visit.
#[derive(Debug, Clone, Copy)]
pub(super) struct Budget {
    /// The most entries compared, directories included.
    entries: usize,
    /// The most bytes of file text read.
    bytes: usize,
    /// The most bytes of text returned.
    output: usize,
}

impl Budget {
    /// What a diff over a whole tree may visit.
    pub(super) const FULL: Self =
        Self { entries: DIFF_MAX_ENTRIES, bytes: DIFF_MAX_SCANNED_BYTES, output: VIEW_MAX_BYTES };
}

/// The diff of `current` against `base` for `args` within `budget`.
///
/// # Errors
///
/// The [`Refusal`] of a directory or file the store cannot give.
async fn diff(
    env: &mut Env<Async>,
    base: Ref<Tree>,
    current: Ref<Tree>,
    args: Ref<DiffArgs>,
    budget: Budget,
) -> Result<Viewed, Refusal> {
    let args = match read_args(env, args).await? {
        Ok(args) => args,
        Err(invalid) => return Ok(Viewed::new(format!("{invalid}."))),
    };
    let scope = args.path.as_ref().map_or("the tree", TreePath::as_str).to_owned();

    let start = match &args.path {
        None => Pair { path: String::new(), old: Some(Node::Directory(base)), new: Some(Node::Directory(current)) },
        Some(path) => {
            let old = leaf(env, base, path).await?;
            let new = leaf(env, current, path).await?;
            match (old, new) {
                (Err(_), Err(blocked)) => return Ok(Viewed::new(blocked.describe())),
                (old, new) => Pair { path: path.as_str().to_owned(), old: old.ok(), new: new.ok() },
            }
        }
    };

    let walked = changes(env, start, budget.entries).await?;
    let empty = walked.changed.is_empty();
    let settled = !walked.stopped;
    let no_change = empty && settled;
    if no_change {
        return Ok(Viewed::new(format!("No change in {scope} since the session opened.")));
    }

    let total = walked.changed.len();
    let mut lines = Lines::new(budget.output);
    let mut stop: Option<Stop> = None;
    for change in &walked.changed {
        let status = match (&change.old, &change.new) {
            (Some(_), Some(_)) => format!("M {}", change.path),
            (None, Some(_)) => format!("A {}", change.path),
            (Some(_), None) => format!("D {}", change.path),
            (None, None) => continue,
        };
        let pushed = lines.push(&status);
        if !pushed {
            stop = Some(Stop::Output { shown: 0, changed: total });
            break;
        }
    }
    let still_open = stop.is_none();
    if still_open {
        let pushed = lines.push("");
        if !pushed {
            stop = Some(Stop::Output { shown: 0, changed: total });
        }
    }

    let mut scanned = 0;
    let mut shown = 0;
    let blocks_open = stop.is_none();
    if blocks_open {
        for change in &walked.changed {
            let over_bytes = scanned >= budget.bytes;
            if over_bytes {
                stop = Some(Stop::Bytes { scanned, shown, changed: total });
                break;
            }
            let outcome = block(env, change, DIFF_MAX_EDITS, &mut lines).await?;
            scanned += outcome.read_bytes;
            let fits = outcome.fit;
            if !fits {
                stop = Some(Stop::Output { shown, changed: total });
                break;
            }
            shown += 1;
        }
    }

    let walk_stopped = walked.stopped;
    let stop = match stop {
        Some(marked) => Some(marked),
        None if walk_stopped => Some(Stop::Entries(walked.visited)),
        None => None,
    };

    match stop {
        None => Ok(lines.finish(String::new)),
        Some(marked) => {
            lines.stop();
            Ok(lines.finish(|| marked.marker()))
        }
    }
}

/// Why a diff stopped before it showed every changed file.
enum Stop {
    Output { shown: usize, changed: usize },
    Bytes { scanned: usize, shown: usize, changed: usize },
    Entries(usize),
}

impl Stop {
    /// The closing line of a diff that stopped.
    fn marker(&self) -> String {
        match self {
            Self::Output { shown, changed } => {
                format!("[stopped at the 64 KiB output cap after {shown} of {changed} files; narrow path]")
            }
            Self::Bytes { scanned, shown, changed } => {
                format!("[stopped after reading {scanned} bytes, {shown} of {changed} files shown; narrow path]")
            }
            Self::Entries(visited) => {
                format!("[stopped after comparing {visited} entries; more may have changed; narrow path]")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use aether_bloomery_kinds::{Mode, Node, Path, Refusal, Tree};
    use aether_bloomery_program::{Async, Env, Program, Tooled, program};
    use aether_data::{OpaqueBytes, Ref};

    use super::{Budget, DIFF_MAX_ENTRIES, DIFF_MAX_SCANNED_BYTES, DiffArgs, TreeDiff, diff};
    use crate::session::fixture::{SmallTree, name, path, run_async, stored, stored_bytes};
    use crate::tools::view::Viewed;

    /// `tree.diff` over a capped output.
    struct CappedDiff;

    /// Diffs like `tree.diff`, stopping at an output that fits the status
    /// list and its blank line but no block.
    #[program]
    impl Program for CappedDiff {
        const NAME: &'static str = "test.tree.diff.capped";
        const MODE: Mode = Mode::Pure;
        const INTENT: &'static str = "Diff within a capped output.";
        type Input = Tooled<DiffArgs, Tree>;
        type Result = Viewed;

        async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
            diff(
                &mut env,
                input.bound(),
                input.tree(),
                input.args(),
                Budget { entries: DIFF_MAX_ENTRIES, bytes: DIFF_MAX_SCANNED_BYTES, output: 48 + 1 + 256 },
            )
            .await
        }
    }

    /// `tree.diff` over a two-entry budget.
    struct TwoEntryDiff;

    /// Diffs like `tree.diff`, visiting at most two entries.
    #[program]
    impl Program for TwoEntryDiff {
        const NAME: &'static str = "test.tree.diff.two";
        const MODE: Mode = Mode::Pure;
        const INTENT: &'static str = "Diff within a two-entry budget.";
        type Input = Tooled<DiffArgs, Tree>;
        type Result = Viewed;

        async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
            diff(
                &mut env,
                input.bound(),
                input.tree(),
                input.args(),
                Budget { entries: 2, bytes: usize::MAX, output: usize::MAX },
            )
            .await
        }
    }

    /// The current tree with `src/lib.rs` holding one more line, and the
    /// artifacts it adds.
    fn edited_current(small: &SmallTree) -> (Ref<Tree>, Vec<aether_bloomery_kinds::ClosureArtifact>) {
        let changed_text = b"pub fn smelt() {}\npub fn bloom() {}\n";
        let blob = stored_bytes(changed_text);
        let blob_ref = Ref::<OpaqueBytes>::of_bytes(changed_text);
        let src = Tree::new(BTreeMap::from([(name("lib.rs"), Node::File(blob_ref))]));
        let src_artifact = stored(&src);
        let src_ref = Ref::of_encoded(&src).expect("a changed src encodes");
        let (current, root_artifact) =
            small.changed(|entries| drop(entries.insert(name("src"), Node::Directory(src_ref))));
        (current, vec![blob, src_artifact, root_artifact])
    }

    /// The current tree of the mixed-changes test, with `README` removed, a
    /// new `docs/notes.md`, `run` as a plain file, `link` retargeted, and
    /// `blob.bin` holding other non-UTF-8 bytes, and the artifacts it adds.
    fn mixed_current(small: &SmallTree) -> (Ref<Tree>, Vec<aether_bloomery_kinds::ClosureArtifact>) {
        let notes = b"Iron blooms.\n";
        let notes_blob = stored_bytes(notes);
        let notes_ref = Ref::<OpaqueBytes>::of_bytes(notes);
        let docs = Tree::new(BTreeMap::from([(name("notes.md"), Node::File(notes_ref))]));
        let docs_artifact = stored(&docs);
        let docs_ref = Ref::of_encoded(&docs).expect("a docs tree encodes");
        let other_bin: &[u8] = &[0xff, 0xfd];
        let bin_blob = stored_bytes(other_bin);
        let bin_ref = Ref::<OpaqueBytes>::of_bytes(other_bin);
        let run_ref = Ref::<OpaqueBytes>::of_bytes(b"#!/bin/sh\nsmelt\n");
        let target = Path::new("src/lib.rs").expect("a symlink target");
        let (current, root_artifact) = small.changed(|entries| {
            entries.remove(&name("README"));
            entries.insert(name("docs"), Node::Directory(docs_ref));
            entries.insert(name("run"), Node::File(run_ref));
            entries.insert(name("link"), Node::Symlink(target.clone()));
            entries.insert(name("blob.bin"), Node::File(bin_ref));
        });
        (current, vec![notes_blob, docs_artifact, bin_blob, root_artifact])
    }

    #[test]
    fn an_edit_shows_its_status_and_hunk_and_reads_nothing_unchanged() {
        // Catches a broken Merkle skip, the two trees swapped (the line would be `-`), and a wrong status letter.
        let small = SmallTree::new();
        let (current, extra) = edited_current(&small);
        let args = DiffArgs::new(None);
        let (input, mut closure) = small.diff_call(current, &args, extra);
        let unwanted = [
            Ref::<OpaqueBytes>::of_bytes(b"# Bloomery\n").digest(),
            Ref::<OpaqueBytes>::of_bytes(b"#!/bin/sh\nsmelt\n").digest(),
            Ref::<OpaqueBytes>::of_bytes(&[0xff, 0xfe]).digest(),
        ];
        closure.retain(|artifact| {
            let digest = artifact.claimed().unverified();
            let hits_unwanted = unwanted.contains(&digest);
            !hits_unwanted
        });
        let (viewed, _) = run_async::<TreeDiff>(&input, closure).expect("a diff is a result");
        assert_eq!(
            viewed.text(),
            "M src/lib.rs\n\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,2 @@\n pub fn smelt() {}\n+pub fn bloom() {}"
        );
    }

    #[test]
    fn an_unchanged_tree_reads_nothing() {
        // Catches a walk that reads the root before comparing it.
        let small = SmallTree::new();
        let args = DiffArgs::new(None);
        let (input, closure) = small.diff_call(small.tree(), &args, Vec::new());
        let args_digest = Ref::of_encoded(&args).expect("diff arguments encode").digest();
        let base_digest = small.tree().digest();
        let kept: Vec<_> = closure
            .into_iter()
            .filter(|artifact| {
                let digest = artifact.claimed().unverified();
                let is_args = digest == args_digest;
                let is_base = digest == base_digest;
                is_args || is_base
            })
            .collect();
        let (viewed, _) = run_async::<TreeDiff>(&input, kept).expect("a diff is a result");
        assert_eq!(viewed.text(), "No change in the tree since the session opened.");
    }

    #[test]
    fn added_removed_mode_kind_and_binary_changes_show() {
        // Catches an added directory shown as one entry, a mode-only change that reads or diffs the blob, a symlink
        // followed, and a binary file refused.
        let small = SmallTree::new();
        let (current, extra) = mixed_current(&small);
        let args = DiffArgs::new(None);
        let (input, closure) = small.diff_call(current, &args, extra);
        let (viewed, _) = run_async::<TreeDiff>(&input, closure).expect("a diff is a result");
        assert_eq!(
            viewed.text(),
            "D README\nM blob.bin\nA docs/notes.md\nM link\nM run\n\n--- a/README\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-# Bloomery\n--- a/blob.bin\n+++ b/blob.bin\nBinary files a/blob.bin and b/blob.bin differ\n--- /dev/null\n+++ b/docs/notes.md\n@@ -0,0 +1,1 @@\n+Iron blooms.\n--- a/link\n+++ b/link\n@@ -1,1 +1,1 @@\n-README\n+src/lib.rs\nold mode 100755\nnew mode 100644\n--- a/run\n+++ b/run"
        );
    }

    #[test]
    fn replaced_symlink_and_directory_show_as_removal_then_addition() {
        // Catches a file-for-symlink swap shown as one `M` with the target diffed against file text, and a directory
        // replaced by a file that drops the removed children or the new file.
        let small = SmallTree::new();

        let link_text = b"file text\n";
        let link_blob = stored_bytes(link_text);
        let link_ref = Ref::<OpaqueBytes>::of_bytes(link_text);
        let (current, root_artifact) =
            small.changed(|entries| drop(entries.insert(name("link"), Node::File(link_ref))));
        let (input, closure) = small.diff_call(current, &DiffArgs::new(None), vec![link_blob, root_artifact]);
        let (viewed, _) = run_async::<TreeDiff>(&input, closure).expect("a diff is a result");
        assert_eq!(
            viewed.text(),
            "D link\nA link\n\n--- a/link\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-README\n--- /dev/null\n+++ b/link\n@@ -0,0 +1,1 @@\n+file text"
        );

        let src_text = b"src file\n";
        let src_blob = stored_bytes(src_text);
        let src_ref = Ref::<OpaqueBytes>::of_bytes(src_text);
        let (current, root_artifact) = small.changed(|entries| drop(entries.insert(name("src"), Node::File(src_ref))));
        let (input, closure) = small.diff_call(current, &DiffArgs::new(None), vec![src_blob, root_artifact]);
        let (viewed, _) = run_async::<TreeDiff>(&input, closure).expect("a diff is a result");
        assert_eq!(
            viewed.text(),
            "A src\nD src/lib.rs\n\n--- /dev/null\n+++ b/src\n@@ -0,0 +1,1 @@\n+src file\n--- a/src/lib.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-pub fn smelt() {}"
        );
    }

    #[test]
    fn a_path_narrows_and_names_what_it_cannot_diff() {
        // Catches a path resolved in one tree only and a path missing on one side reported as blocked.
        let small = SmallTree::new();
        let (current, extra) = mixed_current(&small);
        let (input, closure) = small.diff_call(current, &DiffArgs::new(Some(path("docs"))), extra.clone());
        let (viewed, _) = run_async::<TreeDiff>(&input, closure).expect("a diff is a result");
        assert_eq!(
            viewed.text(),
            "A docs/notes.md\n\n--- /dev/null\n+++ b/docs/notes.md\n@@ -0,0 +1,1 @@\n+Iron blooms."
        );

        let (input, closure) = small.diff_call(current, &DiffArgs::new(Some(path("src"))), extra.clone());
        let (viewed, _) = run_async::<TreeDiff>(&input, closure).expect("a diff is a result");
        assert_eq!(viewed.text(), "No change in src since the session opened.");

        let (input, closure) = small.diff_call(current, &DiffArgs::new(Some(path("missing"))), extra);
        let (viewed, _) = run_async::<TreeDiff>(&input, closure).expect("a diff is a result");
        assert_eq!(viewed.text(), "Nothing is at missing.");
    }

    #[test]
    fn stops_are_marked_and_the_status_list_survives() {
        // Catches a cut that drops the status list and a stop left unmarked.
        let small = SmallTree::new();
        let (current, extra) = mixed_current(&small);
        let args = DiffArgs::new(None);
        let (input, closure) = small.diff_call(current, &args, extra.clone());
        let (viewed, _) = run_async::<CappedDiff>(&input, closure).expect("a diff is a result");
        assert_eq!(
            viewed.text(),
            "D README\nM blob.bin\nA docs/notes.md\nM link\nM run\n\n[stopped at the 64 KiB output cap after 0 of 5 files; narrow path]"
        );

        let (input, closure) = small.diff_call(current, &args, extra);
        let (viewed, _) = run_async::<TwoEntryDiff>(&input, closure).expect("a diff is a result");
        let marker = "[stopped after comparing 2 entries; more may have changed; narrow path]";
        assert!(viewed.text().ends_with(marker), "{}", viewed.text());
        assert!(viewed.text().starts_with("D README\n"), "{}", viewed.text());
    }
}
