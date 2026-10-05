//! The diff `tree.diff` renders: a line edit script, and its rendering as unified hunks.

#[cfg_attr(not(test), expect(dead_code, reason = "tree.diff (#7352) is its first caller"))]
mod hunks;
#[cfg_attr(not(test), expect(dead_code, reason = "tree.diff (#7352) is its first caller"))]
mod myers;
