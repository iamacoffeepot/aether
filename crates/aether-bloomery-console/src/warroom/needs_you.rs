//! One needs-you row per subject: the interrupt set, coloured by alerts.

use super::{Alert, AlertKind, Focus, Interrupt, InterruptKind, alerts, interrupts};
use crate::dto::ViewDocument;

/// How loudly the row should paint. Colour only; never sort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Attention,
    Loud,
}

/// One selectable needs-you line: who, what happened, what to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeedsYouRow {
    pub focus: Focus,
    pub subject: String,
    pub happened: String,
    pub action: String,
    pub severity: Severity,
}

/// Subject plus a digest of the row's source facts.
///
/// A dismissal survives a poll but not a change of facts. It is process-local
/// because the coordinator has no ack route.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DismissKey {
    focus: Focus,
    facts: String,
}

impl NeedsYouRow {
    #[must_use]
    pub fn dismiss_key(&self) -> DismissKey {
        DismissKey { focus: self.focus.clone(), facts: format!("{}|{}", self.happened, self.action) }
    }
}

/// Owner-authority subjects in `view`, one row per [`Focus`], document order.
#[must_use]
pub fn rows(view: &ViewDocument) -> Vec<NeedsYouRow> {
    let alerts = alerts(view);
    group_by_focus(interrupts(view)).into_iter().filter_map(|(focus, group)| fold_row(focus, &group, &alerts)).collect()
}

fn group_by_focus(interrupts: Vec<Interrupt>) -> Vec<(Focus, Vec<Interrupt>)> {
    let mut groups: Vec<(Focus, Vec<Interrupt>)> = Vec::new();
    for interrupt in interrupts {
        match groups.iter_mut().find(|(focus, _)| *focus == interrupt.focus) {
            Some((_, group)) => group.push(interrupt),
            None => groups.push((interrupt.focus.clone(), vec![interrupt])),
        }
    }
    groups
}

fn fold_row(focus: Focus, group: &[Interrupt], alerts: &[Alert]) -> Option<NeedsYouRow> {
    let representative = group.iter().find(|entry| interrupt_is_loud(entry.kind)).or_else(|| group.first())?;
    let subject = focus.subject();
    let loud = group.iter().any(|entry| interrupt_is_loud(entry.kind))
        || alerts.iter().any(|alert| alert.focus == focus && alert_is_loud(alert.kind));
    Some(NeedsYouRow {
        focus,
        subject,
        happened: group.iter().map(compose_happened).collect::<Vec<_>>().join(" · "),
        action: action_clause(representative.kind).to_owned(),
        severity: if loud {
            Severity::Loud
        } else {
            Severity::Attention
        },
    })
}

fn compose_happened(interrupt: &Interrupt) -> String {
    let kind = interrupt.kind.label();
    interrupt.stage.map_or_else(|| kind.to_owned(), |stage| format!("{kind} {stage}"))
}

fn action_clause(kind: InterruptKind) -> &'static str {
    match kind {
        InterruptKind::Park | InterruptKind::Findings => "accept or defer",
        InterruptKind::Decision => "answer",
        InterruptKind::Surface => "amend the surface",
        InterruptKind::Terminal | InterruptKind::Landing => "eject or re-approve",
        InterruptKind::Wedge => "widen the surface or eject",
        InterruptKind::Quiesce => "raise the ceiling or stand down",
        InterruptKind::Hold => "release",
        InterruptKind::BaseRed => "repair the base",
    }
}

fn interrupt_is_loud(kind: InterruptKind) -> bool {
    matches!(
        kind,
        InterruptKind::Terminal
            | InterruptKind::Wedge
            | InterruptKind::Landing
            | InterruptKind::Quiesce
            | InterruptKind::BaseRed
    )
}

fn alert_is_loud(kind: AlertKind) -> bool {
    matches!(kind, AlertKind::Landing | AlertKind::Fault | AlertKind::Wedge)
}

#[cfg(test)]
mod tests {
    use super::{Severity, rows};
    use crate::dto::{
        BaseAlertView, BloomView, CompositionFinding, CompositionView, DigestHex, ExecutorFaultView, HostFaultView,
        LandingBlock, MemberView, OperatorHoldView, PendingDecisionView, Present, ReviewParkView, SpendQuiesce,
        StageId, ViewDocument, WedgeCause,
    };
    use crate::warroom::{Focus, InterruptKind};

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn member(workpiece: &str) -> MemberView {
        MemberView { workpiece: workpiece.to_owned(), ..MemberView::default() }
    }

    #[test]
    fn stacked_stops_on_one_bloom_fold_to_one_loud_row() {
        // The plausible bug: park, at-budget landing, terminal fault, and hold
        // each mint their own chrome id, so j walks four rows for one bloom.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                review_park: Some(ReviewParkView::default()),
                landing_blocked: Some(LandingBlock { rolls: 2, budget: 2 }),
                executor_fault: Some(ExecutorFaultView { rolls: 3, budget: 3, terminal: true }),
                operator_hold: Some(OperatorHoldView { reason: "wait".to_owned(), operator: "owner".to_owned() }),
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let rows = rows(&view);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].focus, Focus::bloom(digest(1)));
        assert_eq!(rows[0].severity, Severity::Loud);
        assert!(!rows[0].action.is_empty());
    }

    #[test]
    fn a_subject_with_no_named_action_does_not_render() {
        // The plausible bug: the wide alert set (in-budget landing, non-terminal
        // fault, host fault) still paints a row even though nothing can be named.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                landing_blocked: Some(LandingBlock { rolls: 1, budget: 3 }),
                executor_fault: Some(ExecutorFaultView { rolls: 1, budget: 3, terminal: false }),
                members: vec![MemberView { host_fault: Some(HostFaultView::default()), ..member("issue-1") }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        assert!(rows(&view).is_empty());
    }

    #[test]
    fn a_park_row_names_the_stage_in_happened() {
        // The plausible bug: the coordinator already serves the park stage and
        // the DTO already decodes it, but the row never prints it.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                review_park: Some(ReviewParkView {
                    stage: Some(StageId::AggregateReview),
                    ..ReviewParkView::default()
                }),
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let rows = rows(&view);
        assert_eq!(rows.len(), 1);
        let happened = &rows[0].happened;
        assert!(happened.contains(StageId::AggregateReview.label()), "stage missing from happened: {happened}");
    }

    #[test]
    fn a_red_base_renders_exactly_one_seal_row() {
        // The plausible bug: a day-level alert that folds into a per-bloom row
        // is invisible when no bloom is sealed.
        let view = ViewDocument {
            base_alert: Some(BaseAlertView { failed: vec!["verify.docs".to_owned()], ..BaseAlertView::default() }),
            ..ViewDocument::default()
        };
        let rows = rows(&view);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].focus, Focus::Seal);
        assert!(rows[0].happened.contains(InterruptKind::BaseRed.label()));
        assert_eq!(rows[0].action, "repair the base");
        assert_eq!(rows[0].severity, Severity::Loud);
    }

    #[test]
    fn a_new_stop_on_one_bloom_mints_a_different_dismiss_key() {
        // The plausible bug: keying on the subject alone, so a park the
        // operator dismissed hides the wedge that replaces it.
        let parked = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                review_park: Some(ReviewParkView::default()),
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let first = rows(&parked)[0].dismiss_key();
        assert_eq!(first, rows(&parked)[0].dismiss_key());

        let wedged = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                members: vec![MemberView { wedge: Some(Present {}), ..member("issue-1") }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        assert_ne!(first, rows(&wedged)[0].dismiss_key());
    }

    #[test]
    fn every_interrupt_kind_names_an_action() {
        // The plausible bug: a kind is folded into a row whose action is empty,
        // so the operator still has to open the drill-in to learn the verb.
        let view = ViewDocument {
            spend_quiesce: Some(SpendQuiesce::Window {
                window: "bloomery/daily/2026-08-17".to_owned(),
                spent_micro_usd: 12,
                ceiling_micro_usd: 10,
            }),
            base_alert: Some(BaseAlertView { failed: vec!["verify.docs".to_owned()], ..BaseAlertView::default() }),
            blooms: vec![
                BloomView { id: digest(1), review_park: Some(ReviewParkView::default()), ..BloomView::default() },
                BloomView {
                    id: digest(2),
                    members: vec![MemberView {
                        pending_decision: Some(PendingDecisionView::default()),
                        ..member("issue-2")
                    }],
                    ..BloomView::default()
                },
                BloomView {
                    id: digest(3),
                    composition: Some(CompositionView {
                        findings: vec![CompositionFinding::default()],
                        ..CompositionView::default()
                    }),
                    ..BloomView::default()
                },
                BloomView {
                    id: digest(4),
                    executor_fault: Some(ExecutorFaultView { rolls: 3, budget: 3, terminal: true }),
                    ..BloomView::default()
                },
                BloomView {
                    id: digest(5),
                    members: vec![MemberView {
                        wedge: Some(Present {}),
                        wedge_cause: Some(WedgeCause::Work),
                        ..member("issue-5")
                    }],
                    ..BloomView::default()
                },
                BloomView {
                    id: digest(6),
                    landing_blocked: Some(LandingBlock { rolls: 2, budget: 2 }),
                    ..BloomView::default()
                },
                BloomView { id: digest(7), operator_hold: Some(OperatorHoldView::default()), ..BloomView::default() },
            ],
            ..ViewDocument::default()
        };
        let rows = rows(&view);
        for kind in [
            InterruptKind::Park,
            InterruptKind::Decision,
            InterruptKind::Findings,
            InterruptKind::Terminal,
            InterruptKind::Wedge,
            InterruptKind::Landing,
            InterruptKind::Quiesce,
            InterruptKind::Hold,
            InterruptKind::BaseRed,
        ] {
            let label = kind.label();
            let row = rows.iter().find(|row| row.happened.contains(label));
            let Some(row) = row else {
                panic!("missing row for {label}");
            };
            assert!(!row.action.is_empty(), "{label} action was empty");
        }
    }
}
