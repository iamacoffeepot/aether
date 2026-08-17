//! The ref bloomery treats as mainline (ADR-0186).
//!
//! Which ref that is is boot configuration, not a constant: bloomery operates on
//! a branch cut per day and repoints at the roll, so the name arrives from the
//! host's resolved config and this type is the one place it is normalized into
//! the two spellings the GitHub surfaces want — the Git Data `heads/…` short
//! form and the bare branch name a pull request's base takes. Keeping both
//! derived from one stored value is what keeps a repoint from moving the
//! observation and leaving the landing proposal aimed at yesterday's branch.
//!
//! Nothing here is sealed into a bloom. A sealed base pins the exact commit a
//! bloom builds on, so the ref name is free to move underneath the journal
//! without making a replay ambiguous.

use core::fmt;

use crate::client::strip_heads;

/// The branch bloomery observes, seals against, and proposes landings onto.
///
/// Constructed from whatever the operator configured and forgiving about the
/// prefix: `refs/heads/main`, `heads/main`, and `main` all name the same branch,
/// and a caller holds one form or the other depending on which side of the port
/// it came from. A prefix left on would not fail loudly — it would address a ref
/// that is not there — so the normalization happens once, here.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MainlineRef {
    /// The Git Data short form, always `heads/<branch>`.
    git_ref: String,
}

impl MainlineRef {
    /// The branch bloomery operates on with nothing configured — the repository's
    /// own default branch, which is what the whole pipeline ran on before the day
    /// branch existed.
    pub const DEFAULT_BRANCH: &'static str = "main";

    /// Normalize a configured ref name. An empty value (a knob cleared rather
    /// than set) resolves to [`DEFAULT_BRANCH`](Self::DEFAULT_BRANCH), the same
    /// way the coordinator's other numeric knobs resolve a zero to their floor.
    #[must_use]
    pub fn new(configured: &str) -> Self {
        let branch = match strip_heads(configured.trim()) {
            "" => Self::DEFAULT_BRANCH,
            named => named,
        };
        Self { git_ref: format!("heads/{branch}") }
    }

    /// The `heads/…` short form the Git Data ref surface takes.
    #[must_use]
    pub fn git_ref(&self) -> &str {
        &self.git_ref
    }

    /// The bare branch name the repository-level surfaces take — a pull
    /// request's `base` above all.
    #[must_use]
    pub fn branch(&self) -> &str {
        strip_heads(&self.git_ref)
    }
}

impl Default for MainlineRef {
    fn default() -> Self {
        Self::new(Self::DEFAULT_BRANCH)
    }
}

/// The fully-qualified form, which is how an operator spells the knob and so how
/// a log naming the operating ref should read it back.
impl fmt::Display for MainlineRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "refs/{}", self.git_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::MainlineRef;

    // Tripwire: the three spellings an operator or a call site can arrive with
    // must land on one ref. A `refs/` left on the Git Data form reads a ref that
    // does not exist, and a `heads/` left on a pull request's base names a
    // branch that does not exist — neither fails loudly, so the normalization is
    // what makes a repoint total rather than partial.
    #[test]
    fn every_configured_spelling_normalizes_to_one_ref_and_one_branch() {
        let day = "bloomery/daily/2026-08-13";
        for configured in [format!("refs/heads/{day}"), format!("heads/{day}"), day.to_owned(), format!("  {day} ")] {
            let mainline = MainlineRef::new(&configured);

            assert_eq!(mainline.git_ref(), format!("heads/{day}"), "`{configured}` addresses the day branch");
            assert_eq!(mainline.branch(), day, "`{configured}` proposes onto the day branch");
            assert_eq!(mainline.to_string(), format!("refs/heads/{day}"), "`{configured}` logs qualified");
        }
    }

    // Tripwire: a cleared knob resolves to the branch the pipeline ran on before
    // the ref was configurable at all, rather than addressing `refs/heads/`.
    #[test]
    fn a_cleared_knob_resolves_to_the_default_branch() {
        assert_eq!(MainlineRef::new(""), MainlineRef::default());
        assert_eq!(MainlineRef::default().git_ref(), "heads/main");
        assert_eq!(MainlineRef::default().branch(), "main");
    }
}
