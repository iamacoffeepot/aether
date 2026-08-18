//! Severity: every loud token the operator should see.
//!
//! Wider than the interrupt queue — a landing still inside budget, a
//! non-terminal fault, and a host fault are loud here and do not qualify
//! as owner-authority stops.

use super::Focus;
use crate::dto::{BloomView, MemberView, ViewDocument};

/// One loud token in the alert band.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    pub kind: AlertKind,
    pub token: String,
    pub detail: String,
    pub focus: Focus,
}

/// Discriminator for a selectable alert token. Display text lives on
/// [`Alert::token`] so a formatted landing/fault string can change without
/// losing the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertKind {
    Park,
    Landing,
    Fault,
    Wedge,
    HostFault,
}

/// Every loud token in `view`, bloom then member, in document order.
#[must_use]
pub fn alerts(view: &ViewDocument) -> Vec<Alert> {
    let mut alerts = Vec::new();
    for bloom in &view.blooms {
        let prefix = bloom.id.prefix();
        if bloom.review_park.is_some() {
            alerts.push(Alert {
                kind: AlertKind::Park,
                token: "PARK".to_owned(),
                detail: prefix.clone(),
                focus: Focus::bloom(bloom.id),
            });
        }
        if let Some(block) = &bloom.landing_blocked {
            alerts.push(Alert {
                kind: AlertKind::Landing,
                token: format!("land: blocked {}/{}", block.rolls, block.budget),
                detail: prefix.clone(),
                focus: Focus::bloom(bloom.id),
            });
        }
        if let Some(fault) = &bloom.executor_fault {
            alerts.push(Alert {
                kind: AlertKind::Fault,
                token: executor_fault_token(fault.rolls, fault.budget, fault.terminal),
                detail: prefix,
                focus: Focus::bloom(bloom.id),
            });
        }
        for member in &bloom.members {
            push_member_alerts(&mut alerts, bloom, member);
        }
    }
    alerts
}

fn push_member_alerts(alerts: &mut Vec<Alert>, bloom: &BloomView, member: &MemberView) {
    let focus = Focus::member(bloom.id, member.workpiece.clone());
    let detail = match &focus {
        Focus::Member { workpiece, .. } => workpiece.clone(),
        _ => bloom.id.prefix(),
    };
    if member.wedge.is_some() {
        alerts.push(Alert {
            kind: AlertKind::Wedge,
            token: "WEDGED".to_owned(),
            detail: detail.clone(),
            focus: focus.clone(),
        });
    }
    if member.host_fault.is_some() {
        alerts.push(Alert { kind: AlertKind::HostFault, token: "hostfault".to_owned(), detail, focus });
    }
}

fn executor_fault_token(rolls: u32, budget: u32, terminal: bool) -> String {
    let mut token = format!("FAULT {rolls}/{budget}");
    if terminal {
        token.push_str(" TERMINAL");
    }
    token
}

#[cfg(test)]
mod tests {
    use super::{AlertKind, alerts};
    use crate::dto::{
        BloomView, DigestHex, ExecutorFaultView, HostFaultView, LandingBlock, MemberView, Present, ReviewParkView,
        ViewDocument, WedgeCause,
    };

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn member(workpiece: &str) -> MemberView {
        MemberView { workpiece: workpiece.to_owned(), ..MemberView::default() }
    }

    #[test]
    fn alerts_name_every_loud_state() {
        // The plausible bug: the band only looks at bloom-level fields, so a
        // wedged or host-faulted member stays quiet; or TERMINAL is dropped.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(0xab),
                review_park: Some(ReviewParkView::default()),
                landing_blocked: Some(LandingBlock { rolls: 2, budget: 3 }),
                executor_fault: Some(ExecutorFaultView { rolls: 3, budget: 3, terminal: true }),
                members: vec![
                    MemberView { wedge: Some(Present {}), wedge_cause: Some(WedgeCause::Work), ..member("issue-1") },
                    MemberView { host_fault: Some(HostFaultView::default()), ..member("issue-2") },
                ],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let tokens: Vec<_> = alerts(&view).into_iter().map(|alert| (alert.kind, alert.token)).collect();
        assert_eq!(
            tokens,
            [
                (AlertKind::Park, "PARK".to_owned()),
                (AlertKind::Landing, "land: blocked 2/3".to_owned()),
                (AlertKind::Fault, "FAULT 3/3 TERMINAL".to_owned()),
                (AlertKind::Wedge, "WEDGED".to_owned()),
                (AlertKind::HostFault, "hostfault".to_owned()),
            ]
        );
    }
}
