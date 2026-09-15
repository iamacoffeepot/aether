//! Who owns the paths a failing gate's findings name (ADR-0218, amended
//! 2026-09-15 "attribution reads the findings first").
//!
//! A diagnostic states a location before it states anything else, and the
//! composition already knows which member wrote each location. That pairing is
//! an answer the bisection in [`batch`](super::batch) buys one four-to-six
//! minute `verify.check` at a time, so it is read here first and the probe
//! walk is kept as the fallback for the cases where it genuinely does not
//! discriminate: a finding that names no path at all, a path no member of the
//! composition touched (the base or an inherited head carries it), and a path
//! several members touched.
//!
//! Nothing here decides a verdict. It reads text into paths, paths into owners,
//! and reports which of the three inconclusive shapes a gate fell into so the
//! caller can bisect exactly that gate and nothing else.

use aether_bloomery::WorkpieceId;

use crate::bloomery::triage::named_surface;

use super::containment::path_in_surface;

/// The prefix a verification findings text heads each gate's section with —
/// `### verify.suppress`, as `verify_findings` in the xtask verify transform
/// assembles it.
const GATE_HEADING: &str = "### ";

/// The lines a failed build closes with rather than a finding: rustc's abort
/// and explain notices, cargo's per-crate tally and build-failed line, and the
/// verify lane's own exit report.
///
/// They name no path because they are counts and pointers, not diagnostics.
/// Read as findings they say "a finding under this gate names no path", which
/// sends every red rustc gate to the probe walk however precisely its real
/// diagnostics located themselves — bloom 9680c483, where two single-member
/// runs bought probes until their deadline over sections whose every
/// diagnostic carried a `-->`.
const CLOSING_NOTICES: [&str; 7] = [
    "error: aborting due to",
    "error: could not compile",
    "error: command ",
    "warning: build failed",
    "For more information about this",
    "Some errors have detailed explanations",
    "Command exited with non-zero status",
];

/// One failing gate's findings, read for the paths they name.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GateFindings {
    /// The gate identity the section is headed with (`verify.suppress`).
    pub gate: String,
    /// Every path the section names, in first-named order and deduplicated.
    pub paths: Vec<String>,
    /// Whether every finding in the section named at least one path.
    ///
    /// A section carrying a finding that names none — a nextest record with no
    /// panic location, a distiller's "… 2 further diagnostics omitted" notice —
    /// says nothing about *that* finding's owner, and the paths the rest of the
    /// section happens to name are not a licence to charge their owners with
    /// the whole gate. Such a section bisects.
    pub complete: bool,
}

/// Read a verification findings text into one entry per gate section.
///
/// Text before the first `### ` heading is the prose the findings open with and
/// belongs to no gate. A section's findings are its unindented non-blank lines:
/// every diagnostic shape this repository produces — rustc/clippy/rustdoc's
/// `error:` opener, nextest's `binary-id test_name` record head, the suppression
/// scanner's `path:line — …` line, rustfmt's `Diff in …` — starts a record at
/// column zero and indents whatever continues it — save for rustc's
/// source-snippet gutter and its `...` elision marker, which sit at column zero
/// and still belong to the diagnostic above them.
#[must_use]
pub fn gate_findings(findings: &str) -> Vec<GateFindings> {
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    for line in findings.lines() {
        if let Some(gate) = line.strip_prefix(GATE_HEADING) {
            sections.push((gate.trim().to_owned(), Vec::new()));
            continue;
        }
        let Some((_, records)) = sections.last_mut() else {
            continue;
        };
        match records.last_mut() {
            Some(record) if continues_a_record(line) => {
                record.push('\n');
                record.push_str(line);
            }
            // A section whose first content line is already indented has no
            // record for that line to continue, and dropping it drops the
            // diagnostic's `-->` with it. Opening a record on it reads the
            // block; a blank line before any record is still nothing.
            _ if !line.trim().is_empty() => records.push(line.to_owned()),
            _ => {}
        }
    }

    sections.iter().map(|(gate, records)| read_records(gate, records)).collect()
}

/// Whether `line` belongs to the record above it rather than opening its own.
///
/// Blank and indented lines do, which is the ordinary case. Two column-zero
/// shapes do as well:
///
/// - **rustc's source-snippet gutter.** The line number is right-aligned in a
///   gutter sized to the widest number in the block, so the widest one starts
///   at column zero — `110 |         let recorded = …`. It is the source the
///   diagnostic's own `-->` already located, and reading it as a finding of its
///   own manufactures a finding that names no path out of every snippet rustc
///   prints.
/// - **The elision marker** rustc writes in that same gutter — `...   |` —
///   where a multi-line span skips the middle of a function.
fn continues_a_record(line: &str) -> bool {
    if line.trim().is_empty() || line.starts_with(char::is_whitespace) {
        return true;
    }
    line.split_once('|')
        .is_some_and(|(gutter, _)| gutter.trim_end().chars().all(|column| column.is_ascii_digit() || column == '.'))
}

/// One section's records folded into the paths they name.
///
/// A [closing notice](CLOSING_NOTICES) is neither: it contributes no path and
/// does not count against the section's completeness, because it is the
/// compilation's own summary rather than a finding whose owner is in question.
fn read_records(gate: &str, records: &[String]) -> GateFindings {
    let stated: Vec<&String> = records.iter().filter(|record| !closes_a_compilation(record)).collect();
    let mut paths: Vec<String> = Vec::new();
    let mut complete = !stated.is_empty();
    for record in stated {
        let named = named_surface(record).paths;
        complete &= !named.is_empty();
        for path in named {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    GateFindings { gate: gate.to_owned(), paths, complete }
}

/// Whether `record` is one of the notices a failed compilation closes with.
fn closes_a_compilation(record: &str) -> bool {
    let head = record.lines().next().unwrap_or_default();
    CLOSING_NOTICES.iter().any(|notice| head.starts_with(notice)) || tallies_warnings(head)
}

/// Whether `head` is cargo's per-crate tally rather than a warning of its own —
/// `warning: `aether-chassis-bloomery` (lib) generated 3 warnings`.
fn tallies_warnings(head: &str) -> bool {
    head.starts_with("warning: ")
        && head.contains(" generated ")
        && (head.ends_with(" warning") || head.ends_with(" warnings"))
}

/// What a member's ownership of a named path is decided against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExtentSource {
    /// The exact repository-relative paths each candidate changed against the
    /// composition base. The preferred reading: it is what the member actually
    /// wrote, so a member whose surface merely *covers* a path it never touched
    /// is not charged with it.
    ChangedPaths,
    /// Each member's declared-surface globs, read when no candidate delta is
    /// available. Coarser — a surface is a permission, not a record — so it
    /// names more owners and therefore attributes less often.
    DeclaredSurface,
}

impl ExtentSource {
    /// How the evidence spells this source.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::ChangedPaths => "changed path",
            Self::DeclaredSurface => "declared surface",
        }
    }
}

/// Which member a named path belongs to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PathOwner {
    /// Exactly one member of the composition owns it.
    One(WorkpieceId),
    /// No member owns it: the base, an inherited head, or a path the extents
    /// could not see carries it.
    Unowned,
    /// Several members own it, so the path does not discriminate between them.
    Several,
}

/// One composition's members, each with the extent its ownership is read from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MemberExtents {
    /// What the extents are, which is also what the evidence says was used.
    pub source: ExtentSource,
    /// Each member and its extent, in the composition's own order.
    pub members: Vec<(WorkpieceId, Vec<String>)>,
}

impl MemberExtents {
    /// Who owns `named`.
    #[must_use]
    pub fn owner(&self, named: &str) -> PathOwner {
        let mut owners = self.members.iter().filter(|(_, extent)| self.covers(extent, named)).map(|(id, _)| id);
        match (owners.next(), owners.next()) {
            (Some(owner), None) => PathOwner::One(owner.clone()),
            (Some(_), Some(_)) => PathOwner::Several,
            _ => PathOwner::Unowned,
        }
    }

    /// Whether one member's extent covers the path a finding named.
    ///
    /// A findings path is matched by suffix as well as exactly, because a
    /// diagnostic spells a location however the tool that produced it does —
    /// `golden_decisions.rs` as readily as the repository-relative path. The
    /// suffix has to start at a segment boundary, so `a/b/lib.rs` is not
    /// covered by a member that changed `a/sub-lib.rs`.
    fn covers(&self, extent: &[String], named: &str) -> bool {
        match self.source {
            ExtentSource::ChangedPaths => extent
                .iter()
                .any(|changed| changed == named || changed.strip_suffix(named).is_some_and(|head| head.ends_with('/'))),
            ExtentSource::DeclaredSurface => path_in_surface(extent, named),
        }
    }
}

/// How a failing gate is attributed before any probe is bought.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GateAttribution {
    /// Every path the gate's findings name belongs to exactly one member. Each
    /// named member is charged with the gate, in composition order.
    Attributed(Vec<WorkpieceId>),
    /// The findings do not discriminate. The caller bisects this gate, and the
    /// string says why so the run's evidence can carry the reason.
    Inconclusive(String),
}

/// Attribute one failing gate from the paths its findings name.
#[must_use]
pub fn attribute_gate(findings: &GateFindings, extents: &MemberExtents) -> GateAttribution {
    if !findings.complete || findings.paths.is_empty() {
        return GateAttribution::Inconclusive(format!("a finding under {} names no path", findings.gate));
    }

    let mut owners: Vec<WorkpieceId> = Vec::new();
    for path in &findings.paths {
        match extents.owner(path) {
            PathOwner::One(owner) if !owners.contains(&owner) => owners.push(owner),
            PathOwner::One(_) => {}
            PathOwner::Unowned => {
                return GateAttribution::Inconclusive(format!(
                    "{path} under {} is outside every member's {}",
                    findings.gate,
                    extents.source.describe(),
                ));
            }
            PathOwner::Several => {
                return GateAttribution::Inconclusive(format!(
                    "{path} under {} is inside several members' {}",
                    findings.gate,
                    extents.source.describe(),
                ));
            }
        }
    }
    GateAttribution::Attributed(
        extents.members.iter().map(|(id, _)| id).filter(|id| owners.contains(id)).cloned().collect(),
    )
}

#[cfg(test)]
mod tests;
