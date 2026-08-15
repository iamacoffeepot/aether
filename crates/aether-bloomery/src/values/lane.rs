//! The construct-lane workpiece identity header (#4984, #4985).
//!
//! Sibling members of one bloom share a sealed work-order body. The chassis
//! fan-out pins the member onto `--task` so each dispatch has its own prompt
//! and session-pool key; the lane then peels that line back off so the shared
//! body can sit in the prompt-cache prefix. Both sides compose through
//! [`LANE_WORKPIECE_HEADER`] so a rename on the producer cannot leave the
//! consumer matching a spelling that is no longer written.

use alloc::string::String;

/// The leading token of the per-member identity line the chassis pins onto a
/// construct-lane `--task` and the lane peels back off.
///
/// One spelling. A producer that writes a different token leaves member
/// identity at the head of the shared body, and the prompt-cache prefix
/// #4985 bought disappears with no other gate failing.
pub const LANE_WORKPIECE_HEADER: &str = "Workpiece:";

/// Pin a member's workpiece id onto the shared work-order body.
///
/// The body stays the sealed shared order; the header is the first line the
/// lane can trust. [`split_lane_identity`] is the matching peel.
#[must_use]
pub fn pin_workpiece_description(workpiece: &str, body: &str) -> String {
    format!("{LANE_WORKPIECE_HEADER} {workpiece}\n\n{body}")
}

/// Peel a leading workpiece identity line off the work-order text so member
/// identity can sit in the prompt's variant tail.
///
/// Only a first line that starts with [`LANE_WORKPIECE_HEADER`] is the pin.
/// A mention buried in the body stays put. [`pin_workpiece_description`] is
/// the matching write.
#[must_use]
pub fn split_lane_identity(task: &str) -> (&str, Option<&str>) {
    let Some((first, rest)) = task.split_once('\n') else {
        return if task.starts_with(LANE_WORKPIECE_HEADER) {
            ("", Some(task))
        } else {
            (task, None)
        };
    };
    if !first.starts_with(LANE_WORKPIECE_HEADER) {
        return (task, None);
    }
    (rest.strip_prefix('\n').unwrap_or(rest), Some(first))
}

#[cfg(test)]
mod tests {
    use super::{LANE_WORKPIECE_HEADER, pin_workpiece_description, split_lane_identity};

    // Tripwire: the chassis fan-out pins the member; the xtask lane peels it.
    // A rename or reformat on one side only leaves identity at the head of the
    // shared body, and the prompt-cache prefix #4985 bought disappears with
    // no other gate failing.
    #[test]
    fn pin_then_split_recovers_the_body_and_the_identity() {
        let body = "# Wave-4 member work order\n\nImplement the sealed plan.\n";
        let pinned = pin_workpiece_description("issue-1111", body);
        let (peeled, header) = split_lane_identity(&pinned);
        let header = header.expect("what pin writes must be a peelable leading identity");
        assert!(header.starts_with(LANE_WORKPIECE_HEADER), "the pin must write the const the peel matches");
        assert!(header.ends_with("issue-1111"), "the peeled line still names the member");
        assert_eq!(peeled, body, "the shared body must come back intact");

        let header_only = pin_workpiece_description("issue-1111", "");
        let (empty, header) = split_lane_identity(&header_only);
        assert!(header.is_some(), "a header-only pin still peels");
        assert_eq!(empty, "", "a header-only task leaves an empty body");
    }

    // Tripwire: only a leading identity line is the per-lane pin. A mention
    // buried in the work order must stay put, or a work order that names the
    // pin would lose its first paragraph into the tail.
    #[test]
    fn split_leaves_a_non_leading_header_in_the_body() {
        let intact = format!("Implement it.\n\n{LANE_WORKPIECE_HEADER} issue-1111 is named in the order.");
        assert_eq!(
            split_lane_identity(&intact),
            (intact.as_str(), None),
            "a header token that is not first is part of the shared body",
        );
    }
}
