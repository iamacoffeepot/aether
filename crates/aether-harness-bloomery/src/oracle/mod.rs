//! One oracle, checked after every tick: liveness, the doctor's report, and
//! bounded termination.

use std::fmt;

use aether_bloomery::{BloomStatus, ViewDocument};
use aether_chassis_bloomery::bloomery::DoctorReport;

pub mod liveness;

/// Why a generated or pinned scenario is a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Bloom id as hex, when one is implicated.
    pub bloom: Option<String>,
    /// Member workpiece, when one is implicated.
    pub member: Option<String>,
    /// The state the reader observed.
    pub state: String,
    /// Which of the three readers objected.
    pub reader: &'static str,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} objected", self.reader)?;
        if let Some(bloom) = &self.bloom {
            write!(f, " bloom={bloom}")?;
        }
        if let Some(member) = &self.member {
            write!(f, " member={member}")?;
        }
        write!(f, ": {}", self.state)
    }
}

/// Compose the two existing readers plus bounded termination.
pub struct Oracle;

impl Oracle {
    /// Check liveness, the doctor report, and that every non-terminal member
    /// is in a named state with an operator exit.
    ///
    /// `outstanding` is the store's live order nonces.
    pub fn check(
        document: &ViewDocument,
        doctor: Option<&DoctorReport>,
        outstanding: &[String],
    ) -> Result<(), Violation> {
        match liveness::classify(document, outstanding) {
            liveness::Quiescence::Stalled(why) => {
                return Err(Violation {
                    bloom: document.blooms.first().map(|bloom| bloom.id.0.to_hex()),
                    member: None,
                    state: why,
                    reader: "liveness",
                });
            }
            liveness::Quiescence::Terminal(_) | liveness::Quiescence::Wedged(_) => {}
        }

        if let Some(report) = doctor {
            if let Some(check) = report.named("nonterminal_member_has_lane_or_dispatch").filter(|check| !check.passed) {
                return Err(Violation {
                    bloom: None,
                    member: None,
                    state: check.divergences.join("; "),
                    reader: "doctor.nonterminal_member_has_lane_or_dispatch",
                });
            }
            if !report.is_clean() {
                let summary = report
                    .violations()
                    .map(|check| format!("{}: {}", check.name, check.divergences.join("; ")))
                    .collect::<Vec<_>>()
                    .join(" | ");
                return Err(Violation { bloom: None, member: None, state: summary, reader: "doctor" });
            }
        }

        termination(document, outstanding)
    }
}

fn termination(document: &ViewDocument, outstanding: &[String]) -> Result<(), Violation> {
    if !outstanding.is_empty() {
        return Ok(());
    }
    for bloom in &document.blooms {
        if matches!(
            bloom.status,
            BloomStatus::Landed | BloomStatus::Superseded | BloomStatus::Resolved | BloomStatus::Withdrawn
        ) {
            continue;
        }
        if bloom.operator_hold.is_some() || bloom.review_park.is_some() || document.base_alert.is_some() {
            continue;
        }
        for member in &bloom.members {
            // Each of these is an accountable stop with a name on it: a
            // resolution, a wedge, a sick host, a construct that declined, a
            // surface amendment a person owes (ADR-0207), an operator's
            // withdrawal (#5327), or an eviction waiting on the sibling that
            // took its file (ADR-0204). A member with none of them and no lane is
            // the nameless wait this oracle exists to catch.
            let named = member.resolution.is_some()
                || member.wedge.is_some()
                || member.host_fault.is_some()
                || member.park.is_some()
                || member.awaiting_surface.is_some()
                || member.withdrawn.is_some()
                || member.evicted_by.is_some();
            if !named {
                return Err(Violation {
                    bloom: Some(bloom.id.0.to_hex()),
                    member: Some(member.workpiece.0.clone()),
                    state: "no lane, no outstanding order, no wedge, and no named stop".into(),
                    reader: "termination",
                });
            }
        }
    }
    Ok(())
}
