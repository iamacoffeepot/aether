//! Forward Myers line diff, bounded by the caller's edit cap.
//!
//! The search interns each distinct line to a `u32` id, trims the common
//! prefix and suffix into `Keep`s, then runs the greedy forward search over
//! the middle, keeping one `V` array per round for the backtrack. Past the
//! caller's cap it gives up with `None`, so time and memory stay bounded by
//! the cap. The search and the backtrack below are both plain loops.

use std::collections::HashMap;

/// One step of the line edit script between two line slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Edit {
    /// A line kept on both sides, at these old and new indices.
    Keep {
        /// The index into the old slice.
        old: usize,
        /// The index into the new slice.
        new: usize,
    },
    /// A line removed from the old slice, at this old index.
    Delete {
        /// The index into the old slice.
        old: usize,
    },
    /// A line added from the new slice, at this new index.
    Insert {
        /// The index into the new slice.
        new: usize,
    },
}

/// The edit script turning `old` into `new`, or `None` once the deletions
/// plus insertions pass `max_edits`.
///
/// Each `Keep` and `Delete` consumes the next old line and each `Insert`
/// emits its new line, so running the script over `old` rebuilds `new`.
#[must_use]
pub(super) fn edits(old: &[&str], new: &[&str], max_edits: usize) -> Option<Vec<Edit>> {
    let prefix = common_prefix(old, new);
    let suffix = common_suffix(old, new, prefix);
    let middle_old = &old[prefix..old.len() - suffix];
    let middle_new = &new[prefix..new.len() - suffix];
    let (interned_old, interned_new) = interned(middle_old, middle_new);
    let middle = search(&interned_old, &interned_new, max_edits)?;

    let mut script = Vec::with_capacity(old.len().saturating_add(new.len()));
    for index in 0..prefix {
        script.push(Edit::Keep { old: index, new: index });
    }
    for edit in middle {
        let shifted = match edit {
            Edit::Keep { old, new } => Edit::Keep { old: old + prefix, new: new + prefix },
            Edit::Delete { old } => Edit::Delete { old: old + prefix },
            Edit::Insert { new } => Edit::Insert { new: new + prefix },
        };
        script.push(shifted);
    }
    for position in 0..suffix {
        script.push(Edit::Keep { old: old.len() - suffix + position, new: new.len() - suffix + position });
    }
    Some(script)
}

/// How many leading lines `old` and `new` share.
fn common_prefix(old: &[&str], new: &[&str]) -> usize {
    old.iter().zip(new.iter()).take_while(|pair| pair.0 == pair.1).count()
}

/// How many trailing lines `old` and `new` share past their common `prefix`.
fn common_suffix(old: &[&str], new: &[&str], prefix: usize) -> usize {
    old[prefix..].iter().rev().zip(new[prefix..].iter().rev()).take_while(|pair| pair.0 == pair.1).count()
}

/// `old` and `new` with each distinct line mapped to a `u32` id, so the
/// search below compares integers.
fn interned<'line>(old: &[&'line str], new: &[&'line str]) -> (Vec<u32>, Vec<u32>) {
    let mut ids: HashMap<&'line str, u32> = HashMap::new();
    let mut next: u32 = 0;
    let interned_old = intern_all(&mut ids, &mut next, old);
    let interned_new = intern_all(&mut ids, &mut next, new);
    (interned_old, interned_new)
}

/// `lines` with each distinct line mapped to a `u32` id, minting fresh ids
/// through `next` on first sight.
fn intern_all<'line>(ids: &mut HashMap<&'line str, u32>, next: &mut u32, lines: &[&'line str]) -> Vec<u32> {
    let mut interned = Vec::with_capacity(lines.len());
    for &line in lines {
        let id = *ids.entry(line).or_insert_with(|| {
            let id = *next;
            *next += 1;
            id
        });
        interned.push(id);
    }
    interned
}

/// The edit script turning the interned `old` into the interned `new`, or
/// `None` once the deletions plus insertions pass `max_edits`.
///
/// Each round of the greedy forward search walks its diagonals, every
/// diagonal stepping right on a deletion or down on an insertion, whichever
/// reached further last round, then sliding along matching lines. `reach`
/// holds the furthest old index per diagonal, and `trace` keeps every
/// round's copy for the backtrack.
fn search(old: &[u32], new: &[u32], max_edits: usize) -> Option<Vec<Edit>> {
    let (count_old, count_new) = (old.len(), new.len());
    let max_edits = max_edits.min(count_old.saturating_add(count_new));
    let need = count_old.abs_diff(count_new);
    if need > max_edits {
        return None;
    }

    let offset = max_edits + 1;
    let end = if count_old >= count_new {
        offset + (count_old - count_new)
    } else {
        offset - (count_new - count_old)
    };
    let mut reach = vec![0; 2 * max_edits + 3];
    let mut trace: Vec<Vec<usize>> = Vec::with_capacity(max_edits + 1);
    for round in 0..=max_edits {
        for diagonal in (offset - round..=offset + round).step_by(2) {
            let down = go_down(&reach, diagonal, offset, round);
            let mut old_index = if down {
                reach[diagonal + 1]
            } else {
                reach[diagonal - 1] + 1
            };
            let mut new_index = counterpart(old_index, diagonal, offset);
            while lines_match(old, new, old_index, new_index) {
                old_index += 1;
                new_index += 1;
            }
            reach[diagonal] = old_index;
        }
        trace.push(reach.clone());

        // Before round `need` the end diagonal is unvisited and still 0, which
        // an empty old side would read as reached. `==` is exact: in the
        // first round that reaches the end, no path on the end diagonal runs
        // past the grid, since one that did would have reached the end a
        // round earlier.
        let past_need = round >= need;
        let reached_end = past_need && reach[end] == count_old;
        if reached_end {
            return Some(backtrack(&trace, count_old, count_new, offset, end, round));
        }
    }
    None
}

/// The edit script for a search that reached the end in `rounds` rounds: the
/// trace walked from the last round down to round 0 with a loop, snakes
/// emitted as `Keep` and each step as `Delete` or `Insert`, then reversed
/// once.
///
/// The walk tracks both ends itself, so it never recomputes a new-side
/// index from a diagonal: after the keeps, the next decrement is the edit,
/// which lands on the round's head.
fn backtrack(
    trace: &[Vec<usize>],
    count_old: usize,
    count_new: usize,
    offset: usize,
    end: usize,
    rounds: usize,
) -> Vec<Edit> {
    let mut reversed = Vec::with_capacity(count_old.saturating_add(count_new));
    let mut old_index = count_old;
    let mut new_index = count_new;
    let mut diagonal = end;

    for round in (1..=rounds).rev() {
        let previous = &trace[round - 1];
        let down = go_down(previous, diagonal, offset, round);
        let head_diagonal = if down {
            diagonal + 1
        } else {
            diagonal - 1
        };
        let head_old = previous[head_diagonal];
        let snake_old = if down {
            head_old
        } else {
            head_old + 1
        };
        while old_index > snake_old {
            old_index -= 1;
            new_index -= 1;
            reversed.push(Edit::Keep { old: old_index, new: new_index });
        }
        if down {
            new_index -= 1;
            reversed.push(Edit::Insert { new: new_index });
        } else {
            old_index -= 1;
            reversed.push(Edit::Delete { old: old_index });
        }
        diagonal = head_diagonal;
    }

    while old_index > 0 {
        old_index -= 1;
        new_index -= 1;
        reversed.push(Edit::Keep { old: old_index, new: new_index });
    }
    reversed.reverse();
    reversed
}

/// Whether the search steps down (an insertion) from `diagonal` in `round`,
/// reading the previous round's `reach`: from the top edge it must, from the
/// bottom edge it cannot, otherwise whichever neighbor reached further wins.
fn go_down(reach: &[usize], diagonal: usize, offset: usize, round: usize) -> bool {
    let at_top = diagonal == offset - round;
    let at_bottom = diagonal == offset + round;
    let next_reaches_farther = reach[diagonal - 1] < reach[diagonal + 1];
    at_top || (!at_bottom && next_reaches_farther)
}

/// The new-side index on `diagonal` for `old_index`. Every point the search
/// reaches lies on a path from the origin, so the index is never negative.
fn counterpart(old_index: usize, diagonal: usize, offset: usize) -> usize {
    old_index + offset - diagonal
}

/// Whether the lines at these indices match: both sides still have one, and
/// the two ids agree.
fn lines_match(old: &[u32], new: &[u32], old_index: usize, new_index: usize) -> bool {
    let in_old = old_index < old.len();
    let in_new = new_index < new.len();
    let in_both = in_old && in_new;
    let same_line = old.get(old_index) == new.get(new_index);
    in_both && same_line
}

#[cfg(test)]
mod tests {
    use super::{Edit, edits};

    /// `new` rebuilt from `old` by running `script`: each `Keep` and
    /// `Delete` consumes the next old line, each `Insert` emits its new line.
    fn applied<'line>(old: &[&'line str], new: &[&'line str], script: &[Edit]) -> Vec<&'line str> {
        let mut rebuilt = Vec::with_capacity(new.len());
        for &edit in script {
            match edit {
                Edit::Keep { old: index, .. } => rebuilt.push(old[index]),
                Edit::Delete { .. } => {}
                Edit::Insert { new: index } => rebuilt.push(new[index]),
            }
        }
        rebuilt
    }

    #[test]
    fn equal_inputs_give_only_keeps() {
        // Catches a trim that drops lines instead of keeping them.
        let old = ["a", "b", "c"];
        let new = ["a", "b", "c"];
        let script = edits(&old, &new, 10).expect("equal inputs are within the cap");
        assert_eq!(
            script,
            vec![Edit::Keep { old: 0, new: 0 }, Edit::Keep { old: 1, new: 1 }, Edit::Keep { old: 2, new: 2 },]
        );
    }

    #[test]
    fn an_insertion_gives_the_exact_script() {
        // Catches an off-by-one in the backtrack that shifts the insert.
        let old = ["a", "c"];
        let new = ["a", "b", "c"];
        let script = edits(&old, &new, 10).expect("one insertion is within the cap");
        assert_eq!(
            script,
            vec![Edit::Keep { old: 0, new: 0 }, Edit::Insert { new: 1 }, Edit::Keep { old: 1, new: 2 },]
        );
    }

    #[test]
    fn a_deletion_gives_the_exact_script() {
        // Catches an off-by-one in the backtrack that shifts the delete.
        let old = ["a", "b", "c"];
        let new = ["a", "c"];
        let script = edits(&old, &new, 10).expect("one deletion is within the cap");
        assert_eq!(
            script,
            vec![Edit::Keep { old: 0, new: 0 }, Edit::Delete { old: 1 }, Edit::Keep { old: 2, new: 1 },]
        );
    }

    #[test]
    fn a_replaced_middle_line_is_a_delete_then_an_insert() {
        // Catches a backtrack that emits the two steps in the wrong order.
        let old = ["a", "b", "c"];
        let new = ["a", "x", "c"];
        let script = edits(&old, &new, 10).expect("a replacement is within the cap");
        assert_eq!(
            script,
            vec![
                Edit::Keep { old: 0, new: 0 },
                Edit::Delete { old: 1 },
                Edit::Insert { new: 1 },
                Edit::Keep { old: 2, new: 2 },
            ]
        );
    }

    #[test]
    fn empty_sides_give_only_inserts_or_only_deletes() {
        // Catches a zero-length `V` index when one side starts empty.
        let empty: [&str; 0] = [];
        let script = edits(&empty, &["a", "b"], 10).expect("two insertions are within the cap");
        assert_eq!(script, vec![Edit::Insert { new: 0 }, Edit::Insert { new: 1 }]);

        let script = edits(&["a", "b"], &empty, 10).expect("two deletions are within the cap");
        assert_eq!(script, vec![Edit::Delete { old: 0 }, Edit::Delete { old: 1 }]);

        let script = edits(&empty, &empty, 10).expect("empty inputs are within the cap");
        assert!(script.is_empty());
    }

    #[test]
    fn a_script_costing_exactly_the_cap_succeeds_and_one_more_returns_none() {
        // Catches a cap off by one that rejects a script at exactly `max_edits`.
        let old = ["a", "b", "c"];
        let new = ["a", "x", "c"];
        assert!(edits(&old, &new, 2).is_some());
        assert!(edits(&old, &new, 1).is_none());

        let old = ["a"];
        let new = ["a", "b"];
        assert!(edits(&old, &new, 1).is_some());
        assert!(edits(&old, &new, 0).is_none());

        let old = ["a"];
        let new = ["a"];
        assert!(edits(&old, &new, 0).is_some());
    }

    #[test]
    fn applied_scripts_rebuild_the_new_side() {
        // Catches a backtrack that emits steps out of order: the script runs but rebuilds the wrong text.
        let old: &[&str] = &["a", "b", "c", "d"];
        let new: &[&str] = &["a", "x", "c", "y", "d"];
        let script = edits(old, new, 10).expect("mixed edits are within the cap");
        assert_eq!(applied(old, new, &script), new.to_vec());

        let old: &[&str] = &["a", "a", "b"];
        let new: &[&str] = &["a", "b", "b"];
        let script = edits(old, new, 10).expect("repeated lines are within the cap");
        assert_eq!(applied(old, new, &script), new.to_vec());

        let old: &[&str] = &["a", "b"];
        let new: &[&str] = &["x", "y", "z"];
        let script = edits(old, new, 10).expect("disjoint inputs are within the cap");
        assert_eq!(applied(old, new, &script), new.to_vec());
    }
}
