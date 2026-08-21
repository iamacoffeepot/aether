//! Live board vs history: complementary filters over bloom status, and the
//! member-state ladder the live lane uses one level down.

use crate::dto::{BloomStatus, BloomView, MemberView, ViewDocument};

/// `Landed`, `Superseded`, and a fully-`Withdrawn` bloom belong on history —
/// each is terminal and dispatches nothing. Everything else is on the board.
#[must_use]
pub fn is_history_status(status: Option<BloomStatus>) -> bool {
    matches!(status, Some(BloomStatus::Landed | BloomStatus::Superseded | BloomStatus::Withdrawn))
}

#[must_use]
pub fn is_live_status(status: Option<BloomStatus>) -> bool {
    !is_history_status(status)
}

pub fn live_blooms(view: &ViewDocument) -> impl Iterator<Item = &BloomView> {
    view.blooms.iter().filter(|bloom| is_live_status(bloom.status))
}

pub fn history_blooms(view: &ViewDocument) -> impl Iterator<Item = &BloomView> {
    view.blooms.iter().filter(|bloom| is_history_status(bloom.status))
}

/// One member's standing on the operator ladder. Precedence matches
/// `scripts/bloomery-operator.py`'s `member_status_state`: wedge, surface
/// request, hold, resolution, in-flight attempt, blocked, idle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberState {
    /// An operator took this member out of the bloom (#5327). Ranked first
    /// because it is the one terminal a person decided: it outranks a wedge
    /// the member earned, and the member is not coming back.
    Withdrawn,
    Wedged,
    /// Waiting on a person to widen the declared surface (ADR-0207). Its own
    /// rung rather than folded into `Held`: a hold is an ADR-0151 question an
    /// answer settles, and this is a boundary only an amendment moves.
    AwaitingSurface,
    Held,
    Integrated,
    Running,
    Blocked,
    Idle,
}

impl MemberState {
    #[must_use]
    pub fn of(member: &MemberView) -> Self {
        if member.withdrawn.is_some() {
            return Self::Withdrawn;
        }
        if member.wedge.is_some() {
            return Self::Wedged;
        }
        if member.awaiting_surface.is_some() {
            return Self::AwaitingSurface;
        }
        if member.pending_decision.is_some() {
            return Self::Held;
        }
        if member.resolution.is_some() {
            return Self::Integrated;
        }
        if attempt_in_flight(member) {
            return Self::Running;
        }
        if member.blocked_by.as_deref().is_some_and(|name| !name.is_empty()) {
            return Self::Blocked;
        }
        Self::Idle
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Withdrawn => "withdrawn",
            Self::Wedged => "WEDGED",
            Self::AwaitingSurface => "surface",
            Self::Held => "held",
            Self::Integrated => "integrated",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Idle => "idle",
        }
    }

    /// Needs a decision or is actively moving: the live board keeps these. A
    /// withdrawn member is neither — it has left the line and no decision is
    /// owed on it.
    #[must_use]
    pub fn walks(self) -> bool {
        matches!(self, Self::Running | Self::Wedged | Self::AwaitingSurface | Self::Held)
    }
}

fn attempt_in_flight(member: &MemberView) -> bool {
    member.host_fault.is_none()
        && member.cursor.as_ref().is_some_and(|cursor| cursor.stage.is_some() && cursor.attempts > 0)
}

#[cfg(test)]
mod tests {
    use super::{MemberState, is_history_status, is_live_status};
    use crate::dto::{BloomStatus, MemberView, Present};

    #[test]
    fn board_and_history_partition_every_status() {
        // The plausible bug: Landed still paints on the live board, or
        // Unknown/None fall through both filters and vanish from the document.
        let all = [
            None,
            Some(BloomStatus::Sealed),
            Some(BloomStatus::Resolved),
            Some(BloomStatus::Landed),
            Some(BloomStatus::Superseded),
            Some(BloomStatus::Unknown),
        ];
        for status in all {
            assert_ne!(
                is_live_status(status),
                is_history_status(status),
                "{status:?} must appear on exactly one of board or history"
            );
        }
        assert!(is_history_status(Some(BloomStatus::Landed)));
        assert!(is_history_status(Some(BloomStatus::Superseded)));
        assert!(is_live_status(Some(BloomStatus::Sealed)));
        assert!(is_live_status(Some(BloomStatus::Unknown)));
        assert!(is_live_status(None));
    }

    #[test]
    fn walks_and_at_rest_partition_every_member_state() {
        // The plausible bug: Blocked walks onto the live board, or a new
        // variant is added to the ladder and lands in neither set.
        for state in [
            MemberState::Wedged,
            MemberState::Held,
            MemberState::Integrated,
            MemberState::Running,
            MemberState::Blocked,
            MemberState::Idle,
        ] {
            match state {
                MemberState::Running | MemberState::Wedged | MemberState::Held => {
                    assert!(state.walks(), "{state:?} must walk");
                }
                MemberState::Integrated | MemberState::Blocked | MemberState::Idle => {
                    assert!(!state.walks(), "{state:?} must rest");
                }
            }
        }
    }

    #[test]
    fn member_state_keeps_wedge_above_blocked() {
        // The plausible bug: a second classifier reads blocked_by first, so a
        // wedged member paints as blocked and the ladder disagrees with the
        // operator script.
        assert_eq!(
            MemberState::of(&MemberView {
                wedge: Some(Present {}),
                blocked_by: Some("wp-a".to_owned()),
                ..MemberView::default()
            }),
            MemberState::Wedged
        );
    }
}
