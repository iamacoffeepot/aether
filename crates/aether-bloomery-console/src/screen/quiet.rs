//! Rest counts the live board no longer paints.

use super::partition::{MemberState, live_blooms};
use crate::dto::ViewDocument;

/// One settled fact for the quiet pane. A zero count is not a fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuietLine {
    pub count: usize,
    pub phrase: &'static str,
}

impl QuietLine {
    #[must_use]
    pub fn text(&self) -> String {
        format!("{} {}", self.count, self.phrase)
    }
}

/// Counts, over live blooms only, the members and bloom headers the live
/// lane dropped. History belongs to the `h` screen and is not counted.
#[must_use]
pub fn quiet_lines(view: &ViewDocument) -> Vec<QuietLine> {
    let mut resolved = 0;
    let mut blocked = 0;
    let mut idle = 0;
    let mut blooms_at_rest = 0;
    for bloom in live_blooms(view) {
        let mut walking = false;
        for member in &bloom.members {
            match MemberState::of(member) {
                MemberState::Integrated => resolved += 1,
                MemberState::Blocked => blocked += 1,
                MemberState::Idle => idle += 1,
                MemberState::Withdrawn => {}
                MemberState::Running
                | MemberState::Wedged
                | MemberState::AwaitingSurface
                | MemberState::Evicted
                | MemberState::Held => {
                    walking = true;
                }
            }
        }
        if !walking {
            blooms_at_rest += 1;
        }
    }
    [
        (resolved, "resolved, awaiting land"),
        (blocked, "blocked on peers"),
        (idle, "idle"),
        (blooms_at_rest, "blooms at rest"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, phrase)| QuietLine { count, phrase })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::super::partition::{MemberState, live_blooms};
    use super::quiet_lines;
    use crate::dto::{
        BloomStatus, BloomView, CompositionCursorView, DigestHex, MemberView, Present, StageId, ViewDocument,
    };

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn member(workpiece: &str) -> MemberView {
        MemberView { workpiece: workpiece.to_owned(), ..MemberView::default() }
    }

    fn running(workpiece: &str) -> MemberView {
        MemberView {
            cursor: Some(CompositionCursorView { stage: Some(StageId::Construct), attempts: 1, candidate: None }),
            ..member(workpiece)
        }
    }

    #[test]
    fn a_zero_count_prints_no_line() {
        // The plausible bug: an all-walking fleet still paints "0 idle", so
        // the quiet pane invents rest that the live board did not drop.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                status: Some(BloomStatus::Sealed),
                members: vec![running("wp")],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        assert!(quiet_lines(&view).is_empty());
    }

    #[test]
    fn every_dropped_live_member_is_counted_once() {
        // The plausible bug: a resolved member is dropped from the live lane
        // and also omitted from quiet, or counted on two lines (integrated
        // and blocked) because blocked_by survives resolution.
        let view = ViewDocument {
            blooms: vec![
                BloomView {
                    id: digest(1),
                    status: Some(BloomStatus::Sealed),
                    members: vec![
                        running("wp-walk"),
                        MemberView {
                            resolution: Some(Present {}),
                            blocked_by: Some("wp-a".to_owned()),
                            ..member("wp-done")
                        },
                        MemberView { blocked_by: Some("wp-a".to_owned()), ..member("wp-block") },
                        member("wp-idle"),
                    ],
                    ..BloomView::default()
                },
                BloomView {
                    id: digest(2),
                    status: Some(BloomStatus::Sealed),
                    members: vec![member("wp-rest")],
                    ..BloomView::default()
                },
                BloomView {
                    id: digest(3),
                    status: Some(BloomStatus::Landed),
                    members: vec![MemberView { resolution: Some(Present {}), ..member("wp-landed") }],
                    ..BloomView::default()
                },
            ],
            ..ViewDocument::default()
        };
        let lines = quiet_lines(&view);
        let dropped: Vec<_> = live_blooms(&view)
            .flat_map(|bloom| bloom.members.iter())
            .filter(|member| !MemberState::of(member).walks())
            .collect();
        let member_counts: usize =
            lines.iter().filter(|line| line.phrase != "blooms at rest").map(|line| line.count).sum();
        assert_eq!(member_counts, dropped.len());
        assert_eq!(dropped.len(), 4);
        assert_eq!(lines.iter().find(|line| line.phrase == "resolved, awaiting land").map(|line| line.count), Some(1));
        assert_eq!(lines.iter().find(|line| line.phrase == "blocked on peers").map(|line| line.count), Some(1));
        assert_eq!(lines.iter().find(|line| line.phrase == "idle").map(|line| line.count), Some(2));
        assert_eq!(lines.iter().find(|line| line.phrase == "blooms at rest").map(|line| line.count), Some(1));
    }
}
