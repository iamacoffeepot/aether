//! Findings decomposition for the whole-bloom aggregate review (ADR-0153).
//!
//! A failing aggregate verdict's findings prose is frozen bloom-scoped at the
//! moment of the verdict, then decomposed into per-member slices and routed to
//! the members that own them. Ownership rides an in-band convention the
//! dispatch prompt instructs: each finding block opens with the owning task's
//! workpiece id in square brackets (`[wp-auth] the retry loop …`), the id
//! vocabulary being exactly the `## Task — {workpiece}` sections the critic was
//! shown. The decomposition is fail-closed: only a *complete* attribution — every
//! block tagged with a known member — narrows the implication; any untagged or
//! unknown-tagged remainder leaves the implication empty, which the reducer
//! expands to every member, and every re-opened member then reads the full
//! frozen bloom row instead of a slice.

/// The outcome of decomposing a findings text against a member roster.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct FindingsDecomposition {
    /// Per-member slices in first-tagged order: each member's owned blocks,
    /// joined in input order. Verbatim — the tag stays in the slice, so the
    /// slice is a faithful excerpt of the frozen set.
    pub slices: Vec<(String, String)>,
    /// Blocks no known member owns (untagged, or tagged with an id outside the
    /// roster), joined in input order. `None` when attribution is complete.
    pub unattributed: Option<String>,
}

impl FindingsDecomposition {
    /// Whether every block was attributed to a known member and at least one
    /// member was named — the condition under which the implication narrows to
    /// the owners and the member rows are sliced.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unattributed.is_none() && !self.slices.is_empty()
    }

    /// The owning workpiece ids, in first-tagged order.
    #[must_use]
    pub fn owners(&self) -> Vec<String> {
        self.slices.iter().map(|(workpiece, _)| workpiece.clone()).collect()
    }
}

/// Decompose a findings text into per-member slices against a member roster.
///
/// Blocks are runs of non-blank lines separated by one or more blank lines. A
/// block belongs to the member whose workpiece id opens the block's first line
/// in square brackets; a tag on a continuation line never attributes (a quoted
/// code line mentioning `[something]` mid-block must not re-route the block).
#[must_use]
pub fn decompose_findings(findings: &str, members: &[String]) -> FindingsDecomposition {
    let mut decomposition = FindingsDecomposition::default();
    let mut unattributed: Vec<String> = Vec::new();
    for block in blocks(findings) {
        match block_owner(&block, members) {
            Some(owner) => match decomposition.slices.iter_mut().find(|(workpiece, _)| *workpiece == owner) {
                Some((_, slice)) => {
                    slice.push_str("\n\n");
                    slice.push_str(&block);
                }
                None => decomposition.slices.push((owner, block)),
            },
            None => unattributed.push(block),
        }
    }
    if !unattributed.is_empty() {
        decomposition.unattributed = Some(unattributed.join("\n\n"));
    }
    decomposition
}

/// Split a findings text into blocks: runs of non-blank lines, joined verbatim.
fn blocks(findings: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in findings.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current.join("\n"));
    }
    blocks
}

/// The member owning a block: the roster id its first line opens with in
/// square brackets, or `None` for an untagged or unknown-tagged block.
fn block_owner(block: &str, members: &[String]) -> Option<String> {
    let first = block.lines().next()?.trim_start();
    let tag = first.strip_prefix('[')?.split_once(']')?.0;
    members.iter().find(|member| member.as_str() == tag).cloned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
