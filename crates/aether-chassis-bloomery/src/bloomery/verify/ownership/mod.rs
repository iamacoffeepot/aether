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
/// column zero and indents whatever continues it.
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
        if line.trim().is_empty() || line.starts_with(char::is_whitespace) {
            if let Some(record) = records.last_mut() {
                record.push('\n');
                record.push_str(line);
            }
        } else {
            records.push(line.to_owned());
        }
    }

    sections.iter().map(|(gate, records)| read_records(gate, records)).collect()
}

/// One section's records folded into the paths they name.
fn read_records(gate: &str, records: &[String]) -> GateFindings {
    let mut paths: Vec<String> = Vec::new();
    let mut complete = !records.is_empty();
    for record in records {
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
