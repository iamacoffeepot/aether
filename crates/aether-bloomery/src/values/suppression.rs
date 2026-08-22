//! A lane's stated suppression request and the reviewer's answer to it
//! (ADR-0193).
//!
//! The gate that refuses a new `#[allow]` has always been half a mechanism: a
//! member that legitimately needs one — and the repository's own `clippy.toml`
//! blesses several — could buy a refine lap that removed it, or write the same
//! read in a form the ban does not enumerate. The second outcome is the one in
//! the record, and it is worse than the suppression it replaced, because an
//! unenumerated read is invisible to the audit the lint exists to make
//! possible.
//!
//! So the lane states, and a reviewer grants. [`SuppressionRequest`] is what
//! the lane wrote on the suppression line itself; [`SuppressionDisposition`] is
//! the answer, carrying who gave it. Both are content-addressed, because a
//! disposition closes requests **by digest** — an answer that named its
//! subjects by position would drift the moment the candidate moved.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::surface::truncated;
use crate::digest::{ContentAddressed, Digest};

/// One suppression a lane declined to remove, with the case it stated for it.
///
/// The line number is for reading. The request rides the physical line the
/// attribute sits on, so an unrelated edit that shifts the line shifts both
/// together and there is nothing to rebind; and changing which lint is allowed
/// or which file it sits in edits that same line, producing a different request
/// the reviewer sees fresh. Nothing here is bound to a line number.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct SuppressionRequest {
    /// The repository-relative path the suppression sits in.
    pub path: String,
    /// The line the scanner reported it on.
    pub line: u32,
    /// The lint the attribute allows, as the scanner tokenized it —
    /// `allow(clippy::disallowed_methods)`, `ignore`, and so on.
    pub lint: String,
    /// One line: why the repository's policy blesses this write at this site.
    /// The lane's own words, which is the whole reason a reviewer reads the
    /// code rather than the reason.
    pub reason: String,
}

impl ContentAddressed for SuppressionRequest {
    const DOMAIN: &'static str = "aether.bloomery.suppression_request";
}

impl SuppressionRequest {
    /// The per-candidate request ceiling. A candidate proposing more new
    /// suppressions than this is not making a case a reviewer can read.
    pub const MAX_REQUESTS: usize = 32;
    /// The per-reason byte ceiling; a longer reason is truncated, never
    /// refused. A truncated case is still a case, and dropping the request
    /// would refuse a candidate for writing too much prose.
    pub const MAX_REASON_BYTES: usize = 240;
    /// The lint token's byte ceiling, clipped the same way.
    pub const MAX_LINT_BYTES: usize = 120;

    /// Build the standing request set from a lane's raw claim.
    ///
    /// The trust boundary, the way [`SurfaceRequest::normalize`] is: the lane
    /// is an untrusted worker, and everything it says about paths and prose
    /// stops being a claim here. Drops any entry whose path is not one literal
    /// repository-relative file, or whose reason is blank once trimmed — a
    /// blank reason states nothing for a reviewer to grant against, so it is
    /// the bare suppression it looks like. Deduplicates by `(path, line,
    /// lint)`, sorts, and caps at [`Self::MAX_REQUESTS`].
    ///
    /// [`SurfaceRequest::normalize`]: super::SurfaceRequest::normalize
    #[must_use]
    pub fn normalize(claimed: impl IntoIterator<Item = (String, u32, String, String)>) -> Vec<Self> {
        let mut requests: Vec<Self> = Vec::new();
        for (path, line, lint, reason) in claimed {
            let path = path.trim().to_owned();
            let reason = reason.trim();
            if !super::surface::literal_repo_path(&path) || reason.is_empty() {
                continue;
            }
            let lint = truncated(lint.trim(), Self::MAX_LINT_BYTES);
            if requests.iter().any(|kept| kept.path == path && kept.line == line && kept.lint == lint) {
                continue;
            }
            requests.push(Self { path, line, lint, reason: truncated(reason, Self::MAX_REASON_BYTES) });
        }
        requests.sort();
        requests.truncate(Self::MAX_REQUESTS);
        requests
    }
}

/// What the reviewer answered.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SuppressionVerdict {
    /// The suppressions may stand. The candidate keeps them and continues.
    Granted,
    /// They may not. The member re-opens at `Refine` with the denial's reason
    /// on its findings channel, so the repair lap is told what was refused and
    /// why rather than rediscovering the gate.
    Denied,
}

/// One reviewer's answer to a member's standing suppression requests
/// (ADR-0193 §5).
///
/// Mirrors [`Adjudication`](super::Adjudication) field for field in intent: the
/// subjects named by digest, the decision, the reason in the decider's words,
/// and who decided. `operator` is an unsigned identity for the same reason
/// `Adjudication`'s is — it records *who*, and it is not and cannot become the
/// signed authority an above-`auto` approval needs.
///
/// A grant is observed rather than posted: the coordinator reads the
/// owner-edited marker off its own landing proposal, and takes the granter from
/// the editor login the marker check already trusts. A denial has no marker to
/// place, so it comes through a REST door of its own.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SuppressionDisposition {
    /// The requests being closed, each named by its
    /// [`SuppressionRequest`] digest. Empty is refused at both doors: an answer
    /// that closes nothing is not an answer.
    pub requests: Vec<Digest>,
    /// Granted, or denied.
    pub verdict: SuppressionVerdict,
    /// Why, in the reviewer's own words.
    pub reason: String,
    /// Who answered. For a grant, the pull-request body's last editor login —
    /// the same identity the marker check itself trusts.
    pub operator: String,
}

impl ContentAddressed for SuppressionDisposition {
    const DOMAIN: &'static str = "aether.bloomery.suppression_disposition";
}

impl SuppressionDisposition {
    /// Whether this disposition is well-formed enough to admit: it closes at
    /// least one request, and it names both a reason and a decider.
    ///
    /// Both blanks are refused rather than defaulted, for the reason
    /// [`Adjudication`](super::Adjudication)'s are: a default reason is a
    /// waiver nobody signed, and a blank operator is a decision nobody made.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        !self.requests.is_empty() && !self.reason.trim().is_empty() && !self.operator.trim().is_empty()
    }

    /// Whether this answer re-opens the member it names.
    #[must_use]
    pub fn reopens(&self) -> bool {
        self.verdict == SuppressionVerdict::Denied
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString as _};
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{SuppressionDisposition, SuppressionRequest, SuppressionVerdict};
    use crate::digest::Digest;

    fn claim(entries: &[(&str, u32, &str, &str)]) -> Vec<(String, u32, String, String)> {
        entries
            .iter()
            .map(|(path, line, lint, reason)| ((*path).to_string(), *line, (*lint).to_string(), (*reason).to_string()))
            .collect()
    }

    #[test]
    fn a_blank_reason_states_no_case_and_is_dropped() {
        // The scanner already refuses a marker with no reason, but the reducer
        // must not depend on that: the lane's evidence is an untrusted claim,
        // and a request with nothing written on it would surface to a reviewer
        // as a blank row to rubber-stamp.
        let requests = SuppressionRequest::normalize(claim(&[
            ("crates/a/src/lib.rs", 4, "allow(dead_code)", "   "),
            ("crates/a/src/other.rs", 9, "allow(dead_code)", "operator tooling"),
        ]));

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "crates/a/src/other.rs");
    }

    #[test]
    fn a_path_that_is_not_one_repository_file_is_dropped() {
        // Same boundary the surface request draws: a lane naming an absolute
        // path, an escaping one, or a glob is not describing a file in this
        // tree, and rendering it to a reviewer would put the lane's words where
        // a repository path belongs.
        let requests = SuppressionRequest::normalize(claim(&[
            ("/etc/passwd", 1, "allow(dead_code)", "a reason"),
            ("../outside/src/lib.rs", 1, "allow(dead_code)", "a reason"),
            ("crates/**", 1, "allow(dead_code)", "a reason"),
        ]));

        assert!(requests.is_empty());
    }

    #[test]
    fn the_same_site_stated_twice_is_one_request() {
        let requests = SuppressionRequest::normalize(claim(&[
            ("crates/a/src/lib.rs", 4, "allow(dead_code)", "first"),
            ("crates/a/src/lib.rs", 4, "allow(dead_code)", "second"),
        ]));

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].reason, "first");
    }

    #[test]
    fn a_long_reason_is_clipped_rather_than_losing_the_request() {
        let long = "x".repeat(SuppressionRequest::MAX_REASON_BYTES + 40);
        let requests = SuppressionRequest::normalize(claim(&[("crates/a/src/lib.rs", 4, "allow(dead_code)", &long)]));

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].reason.len(), SuppressionRequest::MAX_REASON_BYTES);
    }

    #[test]
    fn an_answer_that_closes_nothing_or_names_nobody_is_not_well_formed() {
        let closes_nothing = SuppressionDisposition {
            requests: Vec::new(),
            verdict: SuppressionVerdict::Granted,
            reason: "fine".to_string(),
            operator: "owner".to_string(),
        };
        let anonymous = SuppressionDisposition {
            requests: vec![Digest::from_bytes([3; 32])],
            verdict: SuppressionVerdict::Denied,
            reason: "no".to_string(),
            operator: "  ".to_string(),
        };
        let complete = SuppressionDisposition {
            requests: vec![Digest::from_bytes([3; 32])],
            verdict: SuppressionVerdict::Denied,
            reason: "remove it".to_string(),
            operator: "owner".to_string(),
        };

        assert!(!closes_nothing.is_well_formed());
        assert!(!anonymous.is_well_formed());
        assert!(complete.is_well_formed());
        assert!(complete.reopens());
    }
}
