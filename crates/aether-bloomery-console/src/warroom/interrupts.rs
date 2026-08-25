//! Owner-authority stops: nothing moves until a person acts.
//! Live blooms only — a terminal bloom's stops have no remedy left.
//! Narrower than the alert band. A landing still inside budget, a
//! non-terminal executor fault, and a host fault stay out of this queue.

use super::Focus;
use crate::dto::{BloomView, SpendQuiesce, StageId, ViewDocument};
use crate::screen::live_blooms;

/// One row in the interrupt queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interrupt {
    pub kind: InterruptKind,
    pub detail: String,
    pub focus: Focus,
    pub stage: Option<StageId>,
}

/// The source fields the queue is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptKind {
    Park,
    Decision,
    Surface,
    Findings,
    Terminal,
    Wedge,
    Landing,
    Quiesce,
    Hold,
    BaseRed,
}

impl InterruptKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Park => "park",
            Self::Decision => "decision",
            Self::Surface => "surface",
            Self::Findings => "findings",
            Self::Terminal => "terminal",
            Self::Wedge => "wedge",
            Self::Landing => "landing",
            Self::Quiesce => "quiesce",
            Self::Hold => "hold",
            Self::BaseRed => "base",
        }
    }
}

/// Owner-authority entries in `view`, document then bloom then member.
#[must_use]
pub fn interrupts(view: &ViewDocument) -> Vec<Interrupt> {
    let mut entries = Vec::new();
    if let Some(quiesce) = &view.spend_quiesce {
        entries.push(quiesce_entry(quiesce));
    }
    if let Some(alert) = &view.base_alert {
        entries.push(Interrupt {
            kind: InterruptKind::BaseRed,
            detail: alert.failed.join(", "),
            focus: Focus::Seal,
            stage: None,
        });
    }
    push_surface_interrupts(&mut entries, view);
    for bloom in live_blooms(view) {
        push_bloom_interrupts(&mut entries, bloom);
    }
    entries
}

/// Every member awaiting a surface amendment (ADR-0207), at the document level
/// and ahead of the per-bloom walk.
///
/// Its own kind rather than an [`InterruptKind::Decision`]: an ADR-0151
/// question is settled by an answer, and this is settled only by a person
/// widening a boundary — labelling it `decision` sends an operator looking for
/// a question to answer, and there is none. Document-level rather than
/// per-member because the remedy is not the bloom's to produce: it sorts with
/// the other stops that wait on somebody outside the estate, instead of
/// sitting among the per-member rows a lap can still clear. The focus stays on
/// the member, so selecting the entry still lands where the request was made.
fn push_surface_interrupts(entries: &mut Vec<Interrupt>, view: &ViewDocument) {
    for bloom in live_blooms(view) {
        for member in &bloom.members {
            // A withdrawn member interrupts nobody (#5327), here for the same
            // reason the per-member walk skips it: an operator already decided
            // it, so its request has no one left to answer it.
            if member.withdrawn.is_some() || member.awaiting_surface.is_none() {
                continue;
            }
            entries.push(Interrupt {
                kind: InterruptKind::Surface,
                detail: member_detail(bloom, &member.workpiece),
                focus: Focus::member(bloom.id, member.workpiece.clone()),
                stage: None,
            });
        }
    }
}

fn quiesce_entry(quiesce: &SpendQuiesce) -> Interrupt {
    match quiesce {
        SpendQuiesce::Window { window, spent_micro_usd, ceiling_micro_usd } => Interrupt {
            kind: InterruptKind::Quiesce,
            detail: format!("{window}  {spent_micro_usd}/{ceiling_micro_usd}"),
            focus: Focus::Seal,
            stage: None,
        },
        SpendQuiesce::Bloom { window, bloom, spent_micro_usd, ceiling_micro_usd } => Interrupt {
            kind: InterruptKind::Quiesce,
            detail: format!("{window}  {}  {spent_micro_usd}/{ceiling_micro_usd}", bloom.prefix()),
            focus: Focus::bloom(*bloom),
            stage: None,
        },
        SpendQuiesce::Unknown => {
            Interrupt { kind: InterruptKind::Quiesce, detail: "unknown".to_owned(), focus: Focus::Seal, stage: None }
        }
    }
}

fn push_bloom_interrupts(entries: &mut Vec<Interrupt>, bloom: &BloomView) {
    let prefix = bloom.id.prefix();
    if let Some(park) = &bloom.review_park {
        entries.push(Interrupt {
            kind: InterruptKind::Park,
            detail: prefix.clone(),
            focus: Focus::bloom(bloom.id),
            stage: park.stage,
        });
    }
    if let Some(block) = &bloom.landing_blocked
        && block.rolls >= block.budget
    {
        entries.push(Interrupt {
            kind: InterruptKind::Landing,
            detail: format!("{prefix}  {}/{}", block.rolls, block.budget),
            focus: Focus::bloom(bloom.id),
            stage: None,
        });
    }
    if bloom.executor_fault.as_ref().is_some_and(|fault| fault.terminal) {
        entries.push(Interrupt {
            kind: InterruptKind::Terminal,
            detail: prefix.clone(),
            focus: Focus::bloom(bloom.id),
            stage: None,
        });
    }
    if bloom.operator_hold.is_some() {
        entries.push(Interrupt {
            kind: InterruptKind::Hold,
            detail: prefix.clone(),
            focus: Focus::bloom(bloom.id),
            stage: None,
        });
    }
    if let Some(composition) = &bloom.composition {
        if !composition.findings.is_empty() {
            entries.push(Interrupt {
                kind: InterruptKind::Findings,
                detail: prefix.clone(),
                focus: Focus::composition(bloom.id),
                stage: None,
            });
        }
        if composition.wedge.is_some() {
            entries.push(Interrupt {
                kind: InterruptKind::Wedge,
                detail: format!("composition {prefix}"),
                focus: Focus::composition(bloom.id),
                stage: None,
            });
        }
    }
    for member in &bloom.members {
        // A withdrawn member interrupts nobody (#5327): an operator already
        // decided it, and it is not coming back into the line.
        if member.withdrawn.is_some() {
            continue;
        }
        if let Some(pending) = &member.pending_decision {
            entries.push(Interrupt {
                kind: InterruptKind::Decision,
                detail: member_detail(bloom, &member.workpiece),
                focus: Focus::member(bloom.id, member.workpiece.clone()),
                stage: pending.stage,
            });
        }
        if member.wedge.is_some() {
            entries.push(Interrupt {
                kind: InterruptKind::Wedge,
                detail: member_detail(bloom, &member.workpiece),
                focus: Focus::member(bloom.id, member.workpiece.clone()),
                stage: None,
            });
        }
        if member.park.is_some() {
            entries.push(Interrupt {
                kind: InterruptKind::Park,
                detail: member_detail(bloom, &member.workpiece),
                focus: Focus::member(bloom.id, member.workpiece.clone()),
                stage: None,
            });
        }
    }
}

fn member_detail(bloom: &BloomView, workpiece: &str) -> String {
    if workpiece.is_empty() {
        bloom.id.prefix()
    } else {
        workpiece.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{InterruptKind, interrupts};
    use crate::dto::{
        AwaitingSurfaceView, BloomStatus, BloomView, DigestHex, ExecutorFaultView, HostFaultView, LandingBlock,
        MemberView, Present, ReviewParkView, ViewDocument,
    };
    use crate::warroom::Focus;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    #[test]
    fn machine_retryable_states_are_not_interrupts() {
        // The plausible bug: the queue copies the alert band, so a landing
        // still inside budget or a non-terminal fault looks like an owner stop.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                landing_blocked: Some(LandingBlock { rolls: 1, budget: 3 }),
                executor_fault: Some(ExecutorFaultView { rolls: 1, budget: 3, terminal: false }),
                members: vec![MemberView {
                    workpiece: "issue-1".to_owned(),
                    host_fault: Some(HostFaultView::default()),
                    ..MemberView::default()
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        assert!(interrupts(&view).is_empty());
    }

    #[test]
    fn a_surface_park_is_its_own_kind_ahead_of_the_per_bloom_entries() {
        // The plausible bug: a member awaiting a surface amendment is filed as
        // `decision`, which invites an operator to look for a question to
        // answer — there is none, and only a person widening a boundary
        // settles it. Buried among the per-member rows, it also reads as one
        // more thing a lap might clear.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                review_park: Some(ReviewParkView::default()),
                members: vec![MemberView {
                    workpiece: "issue-1".to_owned(),
                    awaiting_surface: Some(AwaitingSurfaceView::default()),
                    ..MemberView::default()
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };

        let entries = interrupts(&view);
        let surface: Vec<_> = entries.iter().filter(|entry| entry.kind == InterruptKind::Surface).collect();
        assert_eq!(surface.len(), 1, "the park is one entry: {entries:?}");
        assert_eq!(surface[0].detail, "issue-1", "and it names the member it focuses");
        assert!(
            !entries.iter().any(|entry| entry.kind == InterruptKind::Decision),
            "a surface park is not a question anyone can answer: {entries:?}",
        );
        assert_eq!(entries[0].kind, InterruptKind::Surface, "it sorts ahead of the per-bloom walk: {entries:?}");
    }

    #[test]
    fn a_superseded_blooms_wedge_leaves_the_band() {
        // The plausible bug: /view never drops a sealed bloom, so a predecessor
        // that wedged before supersession keeps a loud wedge row nothing can
        // clear — superseding is the documented remedy and it is what creates
        // the permanent row.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                status: Some(BloomStatus::Superseded),
                members: vec![MemberView {
                    workpiece: "issue-1".to_owned(),
                    wedge: Some(Present {}),
                    ..MemberView::default()
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        assert!(interrupts(&view).is_empty());
    }

    #[test]
    fn a_parked_member_asks_for_the_operator() {
        // The plausible bug: MemberView.park is decoded and then unread, so a
        // construct-declined park never raises a needs-you row.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                members: vec![MemberView {
                    workpiece: "issue-1".to_owned(),
                    park: Some(Present {}),
                    ..MemberView::default()
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let entries = interrupts(&view);
        assert_eq!(entries.len(), 1, "the park is one entry: {entries:?}");
        assert_eq!(entries[0].kind, InterruptKind::Park);
        assert_eq!(entries[0].focus, Focus::member(digest(1), "issue-1"));
    }
}
