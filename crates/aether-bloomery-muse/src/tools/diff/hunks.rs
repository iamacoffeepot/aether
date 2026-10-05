//! Unified hunks grouped from a line edit script, and their rendering.
//!
//! `myers::edits` gives the flat script; this module groups its changes into
//! hunks with context and renders them with file headers and stand-ins for
//! `tree.diff`.

use std::iter;

use super::myers::Edit;
use crate::tools::view::{Lines, cut};

/// Context lines kept on each side of a change.
pub(super) const CONTEXT_LINES: usize = 3;

/// The most bytes one shown diff line keeps; the rest is cut and marked `…`.
pub(super) const DIFF_MAX_LINE_BYTES: usize = 300;

/// One unified hunk: the script slice it covers and its old and new ranges.
pub(super) struct Hunk {
    script_start: usize,
    script_end: usize,
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
}

/// One maximal run of non-`Keep` steps with its script and line positions.
struct Change {
    start: usize,
    end: usize,
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
}

/// The hunks covering `script`'s changes, each with context on both sides.
#[must_use]
pub(super) fn hunks(script: &[Edit]) -> Vec<Hunk> {
    merge_changes(script.len(), &collect_changes(script))
}

/// Each maximal non-`Keep` run with the old and new positions around it.
fn collect_changes(script: &[Edit]) -> Vec<Change> {
    let mut changes = Vec::new();
    let mut open: Option<Change> = None;
    let (mut old_pos, mut new_pos) = (0, 0);
    for (index, &edit) in script.iter().enumerate() {
        if let Edit::Keep { .. } = edit {
            changes.extend(open.take());
            old_pos += 1;
            new_pos += 1;
            continue;
        }
        let change = open.get_or_insert(Change {
            start: index,
            end: index,
            old_start: old_pos,
            old_end: old_pos,
            new_start: new_pos,
            new_end: new_pos,
        });
        match edit {
            Edit::Delete { .. } => old_pos += 1,
            Edit::Insert { .. } => new_pos += 1,
            Edit::Keep { .. } => {}
        }
        change.end = index + 1;
        change.old_end = old_pos;
        change.new_end = new_pos;
    }
    changes.extend(open);
    changes
}

/// Changes merged into hunks: a separating `Keep` run of at most twice the
/// context keeps two changes in one hunk.
fn merge_changes(script_len: usize, changes: &[Change]) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut group_start = 0;
    for (index, pair) in changes.windows(2).enumerate() {
        let (before, after) = (&pair[0], &pair[1]);
        if gap_breaks(before, after) {
            hunks.push(spanning(script_len, &changes[group_start], before));
            group_start = index + 1;
        }
    }
    if let Some(last) = changes.last() {
        hunks.push(spanning(script_len, &changes[group_start], last));
    }
    hunks
}

/// Whether the `Keep` run between `before` and `after` is too long for them
/// to share a hunk.
fn gap_breaks(before: &Change, after: &Change) -> bool {
    after.start - before.end > 2 * CONTEXT_LINES
}

/// The hunk from `first` through `last`, with context clipped at the
/// script's ends. Only `Keep`s lie outside the changes, so the context moves
/// the script and line positions alike.
fn spanning(script_len: usize, first: &Change, last: &Change) -> Hunk {
    let lead = CONTEXT_LINES.min(first.start);
    let trail = CONTEXT_LINES.min(script_len - last.end);
    let old_start = first.old_start - lead;
    let new_start = first.new_start - lead;
    Hunk {
        script_start: first.start - lead,
        script_end: last.end + trail,
        old_start,
        old_count: last.old_end + trail - old_start,
        new_start,
        new_count: last.new_end + trail - new_start,
    }
}

/// A file's kind for the diff header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileMode {
    /// A regular file, shown as `100644`.
    Regular,
    /// An executable file, shown as `100755`.
    Executable,
}

impl FileMode {
    /// The git mode digits for this file kind.
    fn digits(self) -> &'static str {
        match self {
            Self::Regular => "100644",
            Self::Executable => "100755",
        }
    }
}

/// How a file changed, for its diff header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileChange {
    /// The file appears only on the new side.
    Added,
    /// The file appears only on the old side.
    Removed,
    /// The file is on both sides, with a mode change when `modes` is set.
    Modified {
        /// The old and new modes, set when the mode changed.
        modes: Option<(FileMode, FileMode)>,
    },
}

/// The 1-based start git prints for `count` lines from 0-based `start`: the
/// number of lines before an empty side, otherwise one past `start`.
fn display_start(start: usize, count: usize) -> usize {
    if count == 0 {
        start
    } else {
        start + 1
    }
}

/// The `@@ -a,b +c,d @@` header for `hunk`, with both counts always printed.
fn hunk_header(hunk: &Hunk) -> String {
    let old_start = display_start(hunk.old_start, hunk.old_count);
    let new_start = display_start(hunk.new_start, hunk.new_count);
    format!("@@ -{old_start},{} +{new_start},{} @@", hunk.old_count, hunk.new_count)
}

/// `hunk`'s header and content lines pushed into `lines`: `false` as soon as
/// the budget refuses one, so the caller stops.
#[must_use]
pub(super) fn render_hunk(hunk: &Hunk, script: &[Edit], old: &[&str], new: &[&str], lines: &mut Lines) -> bool {
    let steps = script[hunk.script_start..hunk.script_end].iter().map(|&edit| match edit {
        Edit::Keep { old: index, .. } => format!(" {}", cut(old[index], DIFF_MAX_LINE_BYTES)),
        Edit::Delete { old: index } => format!("-{}", cut(old[index], DIFF_MAX_LINE_BYTES)),
        Edit::Insert { new: index } => format!("+{}", cut(new[index], DIFF_MAX_LINE_BYTES)),
    });
    push_all(lines, iter::once(hunk_header(hunk)).chain(steps))
}

/// The `---`/`+++` header for `path` pushed into `lines`, after `old mode` /
/// `new mode` lines when a modified file's mode changed: `false` as soon as
/// the budget refuses one.
#[must_use]
pub(super) fn header(path: &str, change: FileChange, lines: &mut Lines) -> bool {
    let modes = match change {
        FileChange::Modified { modes: Some((old_mode, new_mode)) } if old_mode != new_mode => {
            vec![format!("old mode {}", old_mode.digits()), format!("new mode {}", new_mode.digits())]
        }
        FileChange::Modified { .. } | FileChange::Added | FileChange::Removed => Vec::new(),
    };
    let sides = match change {
        FileChange::Added => [String::from("--- /dev/null"), format!("+++ b/{path}")],
        FileChange::Removed => [format!("--- a/{path}"), String::from("+++ /dev/null")],
        FileChange::Modified { .. } => [format!("--- a/{path}"), format!("+++ b/{path}")],
    };
    push_all(lines, modes.into_iter().chain(sides))
}

/// Each of `rendered` pushed into `lines` in order: `false` at the first one
/// the budget refuses, with nothing after it pushed.
fn push_all(lines: &mut Lines, rendered: impl IntoIterator<Item = String>) -> bool {
    rendered.into_iter().all(|line| lines.push(&line))
}

/// The one-line stand-in for a binary `path` pushed into `lines`.
#[must_use]
pub(super) fn binary(path: &str, lines: &mut Lines) -> bool {
    lines.push(&format!("Binary files a/{path} and b/{path} differ"))
}

/// The whole-file stand-in pushed in place of hunks when the edit script gave
/// up: `false` when the budget refuses it.
#[must_use]
pub(super) fn too_different(old_lines: usize, new_lines: usize, lines: &mut Lines) -> bool {
    let old_start = display_start(0, old_lines);
    let new_start = display_start(0, new_lines);
    lines.push(&format!("@@ -{old_start},{old_lines} +{new_start},{new_lines} @@ too different to show"))
}

/// The one-line stand-in for a file too large to diff.
#[must_use]
pub(super) fn too_large(lines: &mut Lines) -> bool {
    lines.push("file changed; too large to diff")
}

#[cfg(test)]
mod tests {
    use super::super::myers::{Edit, edits};
    use super::{FileChange, FileMode, binary, header, hunks, render_hunk, too_different, too_large};
    use crate::tools::view::Lines;

    /// `old` turned into `new` as a real edit script, within a generous cap.
    fn script_for(old: &[&str], new: &[&str]) -> Vec<Edit> {
        edits(old, new, 100).expect("distinct lines stay within the cap")
    }

    /// Every hunk of `script` rendered with a generous budget.
    fn rendered(old: &[&str], new: &[&str], script: &[Edit]) -> String {
        let grouped = hunks(script);
        let mut lines = Lines::new(usize::MAX);
        for hunk in &grouped {
            let pushed = render_hunk(hunk, script, old, new, &mut lines);
            assert!(pushed, "a generous budget never refuses");
        }
        lines.finish(String::new).text().to_owned()
    }

    /// One stand-in or header pushed with a generous budget.
    fn stand_in(push: impl FnOnce(&mut Lines) -> bool) -> String {
        let mut lines = Lines::new(usize::MAX);
        let pushed = push(&mut lines);
        assert!(pushed, "a generous budget never refuses");
        lines.finish(String::new).text().to_owned()
    }

    #[test]
    fn middle_change_has_three_lines_of_context_and_exact_header() {
        // Catches context off by one.
        let old = ["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9"];
        let new = ["l0", "l1", "l2", "l3", "l4", "l5", "NEW", "l7", "l8", "l9"];
        let script = script_for(&old, &new);
        assert_eq!(hunks(&script).len(), 1);
        let text = rendered(&old, &new, &script);
        let first = text.lines().next().expect("a hunk has a header");
        assert_eq!(first, "@@ -4,7 +4,7 @@");
        let body: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(body, [" l3", " l4", " l5", "-l6", "+NEW", " l7", " l8", " l9"]);
    }

    #[test]
    fn edge_changes_clip_context_at_file_ends() {
        // Catches a start that underflows or runs past the end.
        let old = ["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9"];
        let new_first = ["NEW", "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9"];
        let script = script_for(&old, &new_first);
        assert_eq!(hunks(&script).len(), 1);
        let text = rendered(&old, &new_first, &script);
        let first = text.lines().next().expect("a hunk has a header");
        assert_eq!(first, "@@ -1,4 +1,4 @@");

        let new_last = ["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "NEW"];
        let script = script_for(&old, &new_last);
        assert_eq!(hunks(&script).len(), 1);
        let text = rendered(&old, &new_last, &script);
        let first = text.lines().next().expect("a hunk has a header");
        assert_eq!(first, "@@ -7,4 +7,4 @@");
    }

    #[test]
    fn six_keeps_share_one_hunk_and_seven_give_two() {
        // Catches the merge rule off by one.
        let old = ["a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9", "a10", "a11"];
        let new_six = ["a0", "B1", "a2", "a3", "a4", "a5", "a6", "a7", "B8", "a9", "a10", "a11"];
        let script_six = script_for(&old, &new_six);
        assert_eq!(hunks(&script_six).len(), 1);

        let new_seven = ["a0", "B1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "B9", "a10", "a11"];
        let script_seven = script_for(&old, &new_seven);
        assert_eq!(hunks(&script_seven).len(), 2);
    }

    #[test]
    fn empty_sides_print_before_counts_and_no_change_gives_no_hunks() {
        // Catches the empty-side convention and a hunk for an unchanged file.
        let empty: [&str; 0] = [];
        let script = script_for(&empty, &["a", "b"]);
        assert_eq!(hunks(&script).len(), 1);
        let text = rendered(&empty, &["a", "b"], &script);
        let first = text.lines().next().expect("a hunk has a header");
        assert_eq!(first, "@@ -0,0 +1,2 @@");

        let script = script_for(&["a", "b"], &empty);
        let text = rendered(&["a", "b"], &empty, &script);
        let first = text.lines().next().expect("a hunk has a header");
        assert_eq!(first, "@@ -1,2 +0,0 @@");

        let script = script_for(&["a", "b"], &["a", "b"]);
        let no_hunks = hunks(&script).is_empty();
        assert!(no_hunks);
    }

    #[test]
    fn rendered_space_and_plus_lines_rebuild_the_new_side() {
        // Catches out-of-order or mis-prefixed steps.
        let old = ["keep0", "gone", "keep1", "keep2"];
        let new = ["keep0", "keep1", "keep2", "fresh"];
        let script = script_for(&old, &new);
        assert_eq!(hunks(&script).len(), 1);
        let text = rendered(&old, &new, &script);
        let mut rebuilt = Vec::new();
        for line in text.lines().skip(1) {
            let is_context = line.starts_with(' ');
            let is_insert = line.starts_with('+');
            let hunk_new = is_context || is_insert;
            if hunk_new {
                rebuilt.push(line[1..].to_owned());
            }
        }
        assert_eq!(rebuilt, new.to_vec());
    }

    #[test]
    fn long_lines_are_cut_with_an_ellipsis() {
        // Catches a missing cut on long content lines.
        let varied: Vec<String> = (0..10).map(|index| format!("line-{index}")).collect();
        let old: Vec<&str> = varied.iter().map(String::as_str).collect();
        let mut new = varied.clone();
        let long = "x".repeat(400);
        new[5] = long;
        let new_refs: Vec<&str> = new.iter().map(String::as_str).collect();
        let script = script_for(&old, &new_refs);
        assert_eq!(hunks(&script).len(), 1);
        let text = rendered(&old, &new_refs, &script);
        let inserted = text.lines().find(|line| line.starts_with('+')).expect("an insertion renders");
        let cut_marked = inserted.ends_with('…');
        assert!(cut_marked);
        assert!(inserted.len() < 400);
    }

    #[test]
    fn header_and_stand_in_lines_have_their_git_sides() {
        // Catches a wrong side or label.
        let added = stand_in(|lines| header("note.txt", FileChange::Added, lines));
        assert_eq!(added, "--- /dev/null\n+++ b/note.txt");
        let removed = stand_in(|lines| header("note.txt", FileChange::Removed, lines));
        assert_eq!(removed, "--- a/note.txt\n+++ /dev/null");
        let modified = stand_in(|lines| header("note.txt", FileChange::Modified { modes: None }, lines));
        assert_eq!(modified, "--- a/note.txt\n+++ b/note.txt");
        let mode = stand_in(|lines| {
            header("run.sh", FileChange::Modified { modes: Some((FileMode::Regular, FileMode::Executable)) }, lines)
        });
        assert_eq!(mode, "old mode 100644\nnew mode 100755\n--- a/run.sh\n+++ b/run.sh");

        let binary_text = stand_in(|lines| binary("blob.bin", lines));
        assert_eq!(binary_text, "Binary files a/blob.bin and b/blob.bin differ");

        let different = stand_in(|lines| too_different(10, 12, lines));
        assert_eq!(different, "@@ -1,10 +1,12 @@ too different to show");

        let large = stand_in(too_large);
        assert_eq!(large, "file changed; too large to diff");
    }

    #[test]
    fn tiny_budget_makes_render_hunk_return_false() {
        // Catches a renderer that ignores the budget's refusal.
        let old = ["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9"];
        let new = ["l0", "l1", "l2", "l3", "l4", "l5", "NEW", "l7", "l8", "l9"];
        let script = script_for(&old, &new);
        let grouped = hunks(&script);
        assert_eq!(grouped.len(), 1);
        let mut lines = Lines::new(1);
        let pushed = render_hunk(&grouped[0], &script, &old, &new, &mut lines);
        assert!(!pushed);
    }
}
