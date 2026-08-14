//! What class a review finding is, stated by the reviewer on the finding
//! itself (#4961).
//!
//! Every finding used to weigh the same: landing-blocking. That gave a
//! decidable-but-unenforced property the same weight as a correctness bug, and
//! it would let a genuinely subjective one — naming, taste, a spec reading —
//! stall a bloom through repair rounds it can never satisfy. The owner's
//! requirement is that subjective findings must not break blooms, so the
//! reviewer states which of two things each finding is.
//!
//! - **Mechanical.** The finding asserts a decidable property, and it names the
//!   check that would have caught it. It blocks, and its repair is accepted only
//!   when the diff contains the named check — so the finding retires itself:
//!   next time it is a red gate rather than a review round.
//! - **Judgment.** Spec reading, naming, architecture taste. Advisory by
//!   default — recorded on the composition's findings channel, adjudicable by an
//!   operator, blocking nothing. It blocks only when the reviewer marks it
//!   correctness- or safety-critical *and* says in one sentence why.
//!
//! The vocabulary and its parser live here, in the domain crate, for the reason
//! `Harness` and [`REVIEW_CRITIC_COMMAND`](super::REVIEW_CRITIC_COMMAND) do: two
//! consumers read the same prose — the review lane, which derives what the lane
//! reports from the classes, and the chassis, which records the advisories and
//! holds a mechanical repair to its named check — and a second spelling of the
//! format is a second format.
//!
//! Deliberately shallow, like the repair triage's own extraction. The parser
//! reads one finding per line, recognizes exactly two tags and exactly two
//! parentheticals, and files everything it does not recognize as prose. A line
//! it cannot read is not a finding, and the fail-closed rules downstream are
//! what keep that safe.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

/// The tag that opens a mechanical finding.
pub const MECHANICAL_TAG: &str = "MECHANICAL";

/// The tag that opens a judgment finding.
pub const JUDGMENT_TAG: &str = "JUDGMENT";

/// The parenthetical key a mechanical finding names its check under.
pub const CHECK_KEY: &str = "check";

/// The parenthetical key a judgment finding marks itself blocking under.
pub const CRITICAL_KEY: &str = "critical";

/// Characters a finding line may open with before its tag — the list markers a
/// critic writes prose in.
const LIST_MARKERS: &[char] = &['-', '*', '+', '>', '#', ' ', '\t'];

/// Which of the two classes a finding is, with the one datum its class carries.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FindingClass {
    /// A decidable property, naming the check that would have caught it.
    ///
    /// `check` is empty when the reviewer stated the class but named no check.
    /// That finding is **malformed and still blocks**: it named a decidable
    /// defect, and dropping it would land the defect while bouncing it back to
    /// the reviewer would buy a whole judge round to restate prose. What the
    /// omission costs is the retirement — with no check named there is nothing
    /// for the repair triage to hold the lap to, so the lap is judged by the
    /// ordinary rules instead.
    Mechanical {
        /// The named check — a test, a lint, a CI gate — spelled as the symbol
        /// or path the repair adds or changes.
        check: String,
    },
    /// Spec reading, naming, architecture taste.
    ///
    /// `critical` is the reviewer's one-sentence justification for marking it
    /// correctness- or safety-critical, and its presence is the whole
    /// difference between a finding that blocks and one that is recorded.
    Judgment {
        /// Why this judgment call is correctness- or safety-critical, when the
        /// reviewer said so. `None` is the ordinary advisory.
        critical: Option<String>,
    },
}

/// One classified finding: what the reviewer said, and the line it said it on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ClassifiedFinding {
    /// The class the reviewer stated.
    pub class: FindingClass,
    /// The finding's own line, tag and all, so a reader of the recorded
    /// advisories sees what the reviewer wrote rather than a reconstruction.
    pub line: String,
}

impl ClassifiedFinding {
    /// Whether this finding blocks the composition.
    ///
    /// Mechanical always does — including the malformed shape, fail-closed. A
    /// judgment finding does only when the reviewer marked it critical and said
    /// why; an unjustified marker is an unmarked finding, because the
    /// justification sentence *is* the mark.
    #[must_use]
    pub fn blocks(&self) -> bool {
        match &self.class {
            FindingClass::Mechanical { .. } => true,
            FindingClass::Judgment { critical } => critical.is_some(),
        }
    }

    /// Whether this finding states a class it did not complete — a mechanical
    /// finding naming no check. Reported rather than refused; see
    /// [`FindingClass::Mechanical`].
    #[must_use]
    pub fn is_malformed(&self) -> bool {
        matches!(&self.class, FindingClass::Mechanical { check } if check.is_empty())
    }

    /// The check this finding named, for a mechanical finding that named one.
    #[must_use]
    pub fn named_check(&self) -> Option<&str> {
        match &self.class {
            FindingClass::Mechanical { check } if !check.is_empty() => Some(check),
            _ => None,
        }
    }
}

/// Every classified finding a review's prose carries, in the order it stated
/// them.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ClassifiedFindings {
    /// The findings, in prose order.
    pub findings: Vec<ClassifiedFinding>,
}

impl ClassifiedFindings {
    /// Whether the review classified anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }

    /// Whether any classified finding blocks.
    #[must_use]
    pub fn any_blocking(&self) -> bool {
        self.findings.iter().any(ClassifiedFinding::blocks)
    }

    /// The non-blocking findings — the advisories a passing review records.
    pub fn advisories(&self) -> impl Iterator<Item = &ClassifiedFinding> {
        self.findings.iter().filter(|finding| !finding.blocks())
    }

    /// Every check a mechanical finding named, in prose order.
    ///
    /// The repair-acceptance surface: a lap dispatched against these is held to
    /// containing one of them.
    pub fn named_checks(&self) -> impl Iterator<Item = &str> {
        self.findings.iter().filter_map(ClassifiedFinding::named_check)
    }
}

/// Read the classified findings out of a review's findings prose.
///
/// One finding per line. A line is a finding when — after its list marker — it
/// opens with [`MECHANICAL_TAG`] or [`JUDGMENT_TAG`] followed by a word break;
/// anything else is prose and is skipped. An optional parenthetical directly
/// after the tag carries the class's datum: `(check: …)` for mechanical,
/// `(critical: …)` for judgment. A parenthetical under the wrong key, or one
/// whose value is blank, is read as absent — the reviewer wrote something the
/// format does not define, and guessing at intent here is how a subjective
/// finding would acquire blocking weight nobody stated.
#[must_use]
pub fn classify_findings(prose: &str) -> ClassifiedFindings {
    ClassifiedFindings { findings: prose.lines().filter_map(classify_line).collect() }
}

/// The classified finding one line states, or `None` for prose.
fn classify_line(line: &str) -> Option<ClassifiedFinding> {
    let body = line.trim_start_matches(LIST_MARKERS);
    let (tag, rest) = tagged(body)?;
    let stated = parenthetical(rest);
    let class = if tag == MECHANICAL_TAG {
        FindingClass::Mechanical { check: value_under(stated, CHECK_KEY).unwrap_or_default() }
    } else {
        FindingClass::Judgment { critical: value_under(stated, CRITICAL_KEY).filter(|value| !value.is_empty()) }
    };

    Some(ClassifiedFinding { class, line: line.trim().to_owned() })
}

/// The class tag `body` opens with and the text after it, or `None` when it
/// opens with neither.
///
/// The tag has to end at a word break: a line about `MECHANICALLY` derived
/// state is prose, not a mechanical finding.
fn tagged(body: &str) -> Option<(&'static str, &str)> {
    [MECHANICAL_TAG, JUDGMENT_TAG].into_iter().find_map(|tag| {
        let rest = body.strip_prefix(tag)?;
        (!rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')).then_some((tag, rest))
    })
}

/// The contents of a parenthetical opening `rest`, if one does.
///
/// Directly after the tag on purpose: a finding's own prose routinely carries
/// parentheses, and scanning the whole line for one would let an aside read as
/// a class marker.
fn parenthetical(rest: &str) -> Option<&str> {
    rest.trim_start().strip_prefix('(')?.split_once(')').map(|(inside, _)| inside)
}

/// The value a parenthetical states under `key`, or `None` when it states
/// another key or nothing after the colon.
fn value_under(stated: Option<&str>, key: &str) -> Option<String> {
    let (stated_key, value) = stated?.split_once(':')?;
    (stated_key.trim().eq_ignore_ascii_case(key)).then(|| value.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::{ClassifiedFinding, FindingClass, classify_findings};

    #[test]
    fn a_judgment_finding_blocks_only_with_its_justification_sentence() {
        // Tripwire (#4961): this line is the owner's requirement. A judgment
        // finding that blocks by default is the behaviour the split exists to
        // end; one that blocks on a bare `(critical)` marker re-opens the same
        // door through a word, because the justification sentence is the whole
        // cost of marking something critical.
        let classified = classify_findings(
            "- JUDGMENT — src/lib.rs: `weave` reads better as `composition`.\n\
             - JUDGMENT (critical: the seam drops the retry budget, so a wedge lands silently) — src/reduce.rs: …\n\
             - JUDGMENT (critical:) — src/reduce.rs: marked, unjustified.\n\
             - JUDGMENT (critical) — src/reduce.rs: marked, no sentence at all.",
        );

        let blocking: Vec<bool> = classified.findings.iter().map(ClassifiedFinding::blocks).collect();
        assert_eq!(blocking, [false, true, false, false], "{classified:?}");
        assert_eq!(classified.advisories().count(), 3);
    }

    #[test]
    fn a_mechanical_finding_blocks_and_carries_the_check_it_named() {
        // Tripwire (#4961): a mechanical finding without a check still blocks.
        // Dropping it would land a decidable defect on a formatting slip, and
        // that is the one direction this split must never fail in.
        let classified = classify_findings(
            "MECHANICAL (check: `representative_covers_every_decision` in `tests/golden_decisions/completeness.rs`) \
             — the fixture omits the appended variant.\n\
             MECHANICAL — src/lib.rs: the guard is unexercised.",
        );

        assert!(classified.findings.iter().all(ClassifiedFinding::blocks), "{classified:?}");
        assert!(!classified.findings[0].is_malformed());
        assert!(classified.findings[1].is_malformed(), "a mechanical finding naming no check is malformed");
        assert_eq!(
            classified.named_checks().collect::<Vec<_>>(),
            ["`representative_covers_every_decision` in `tests/golden_decisions/completeness.rs`"],
            "only the well-formed finding contributes a check",
        );
    }

    #[test]
    fn only_a_tagged_line_is_a_finding() {
        // Tripwire (#4961): the downgrade to a passing report is gated on the
        // classified set, so prose that reads as a finding must not become one
        // and a real finding must not be lost to decoration. `MECHANICALLY` is
        // the near-miss that matters — a tag match without the word break would
        // classify a sentence about derived state.
        let classified = classify_findings(
            "I read every changed file and the ADR it cites.\n\
             MECHANICALLY derived state is fine here.\n\
               * JUDGMENT — deeply indented, still a finding.\n\
             > JUDGMENT (critical: a panic on the empty roster) — blockquoted, still a finding.",
        );

        assert_eq!(classified.findings.len(), 2, "{classified:?}");
        assert!(classified.any_blocking());
        assert_eq!(classified.findings[0].line, "* JUDGMENT — deeply indented, still a finding.");
    }

    #[test]
    fn a_parenthetical_under_another_key_states_nothing() {
        // Tripwire (#4961): `(note: …)` must not read as `(critical: …)`, and a
        // mechanical `(critical: …)` must not read as a named check — otherwise
        // a reviewer's aside decides whether a bloom stalls.
        let classified = classify_findings(
            "JUDGMENT (note: worth a follow-up) — taste.\n\
             MECHANICAL (critical: not the key this class takes) — decidable.",
        );

        assert_eq!(classified.findings[0].class, FindingClass::Judgment { critical: None });
        assert!(classified.findings[1].is_malformed());
        assert_eq!(classified.named_checks().count(), 0);
    }
}
