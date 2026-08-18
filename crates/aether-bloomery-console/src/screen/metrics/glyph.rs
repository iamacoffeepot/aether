//! Block-character classes for the lane timeline.
//!
//! Stage families follow the dispatched-command grouping: construct,
//! review, verify, and the host-native remainder. The two silences are
//! distinct glyphs because they call for different operator actions.

use crate::dto::StageId;

/// One glyph class on a timeline row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellKind {
    Stage(StageFamily),
    Silence(Silence),
    Wedge,
    Now,
    Empty,
}

/// Why a member is silent between (or after) dispatched spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Silence {
    /// Blocked on a readiness edge — the operator unblocks the ancestor.
    Blocked,
    /// Queued on a slot — the operator waits for lane occupancy.
    Queued,
}

/// The dispatched-command family a stage paints as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageFamily {
    Construct,
    Review,
    Verify,
    Host,
}

/// Map a stage onto the dispatched-command grouping.
#[must_use]
pub fn family_of(stage: StageId) -> StageFamily {
    match stage {
        StageId::Construct | StageId::Refine | StageId::Reconcile => StageFamily::Construct,
        StageId::Review | StageId::AggregateReview => StageFamily::Review,
        StageId::Verify | StageId::AggregateVerify => StageFamily::Verify,
        StageId::Sketch
        | StageId::Scope
        | StageId::Approve
        | StageId::Integrate
        | StageId::Land
        | StageId::Study
        | StageId::Unknown => StageFamily::Host,
    }
}

/// One block character for a cell class.
#[must_use]
pub fn glyph(kind: CellKind) -> char {
    match kind {
        CellKind::Stage(StageFamily::Construct) => '█',
        CellKind::Stage(StageFamily::Review) => '▓',
        CellKind::Stage(StageFamily::Verify) => '▒',
        CellKind::Stage(StageFamily::Host) => '━',
        CellKind::Silence(Silence::Blocked) => '░',
        CellKind::Silence(Silence::Queued) => '┄',
        CellKind::Wedge => '◆',
        CellKind::Now => '▎',
        CellKind::Empty => ' ',
    }
}

/// The operator action a silence names, or `None` for a painted span.
#[must_use]
pub fn operator_action(kind: CellKind) -> Option<&'static str> {
    match kind {
        CellKind::Silence(Silence::Blocked) => Some("unblock the readiness ancestor"),
        CellKind::Silence(Silence::Queued) => Some("wait for a lane slot"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{CellKind, Silence, glyph, operator_action};

    #[test]
    fn blocked_and_queued_name_different_operator_actions() {
        // The plausible bug: both silences paint the same gap, so the
        // operator cannot tell a readiness wait from a slot wait.
        let blocked = CellKind::Silence(Silence::Blocked);
        let queued = CellKind::Silence(Silence::Queued);
        assert_ne!(glyph(blocked), glyph(queued), "the two silences must not share a glyph");
        assert_eq!(operator_action(blocked), Some("unblock the readiness ancestor"));
        assert_eq!(operator_action(queued), Some("wait for a lane slot"));
        assert_ne!(operator_action(blocked), operator_action(queued));
    }
}
