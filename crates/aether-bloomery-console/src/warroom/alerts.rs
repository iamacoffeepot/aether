//! Severity: every loud token the operator should see.
//!
//! Wider than the interrupt queue — a landing still inside budget, a
//! non-terminal fault, and a host fault are loud here and do not qualify
//! as owner-authority stops.

use super::Focus;
use crate::dto::{BloomView, MemberView, ViewDocument};

/// One loud condition. Display text lives on the folded needs-you row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    pub kind: AlertKind,
    pub detail: String,
    pub focus: Focus,
}

/// Discriminator for a loud condition. Severity is matched onto a
/// [`super::NeedsYouRow`] by [`Focus`]; this kind never selects on its own.
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
            alerts.push(Alert { kind: AlertKind::Park, detail: prefix.clone(), focus: Focus::bloom(bloom.id) });
        }
        if bloom.landing_blocked.is_some() {
            alerts.push(Alert { kind: AlertKind::Landing, detail: prefix.clone(), focus: Focus::bloom(bloom.id) });
        }
        if bloom.executor_fault.is_some() {
            alerts.push(Alert { kind: AlertKind::Fault, detail: prefix, focus: Focus::bloom(bloom.id) });
        }
        for member in &bloom.members {
            push_member_alerts(&mut alerts, bloom, member);
        }
    }
    alerts
}

fn push_member_alerts(alerts: &mut Vec<Alert>, bloom: &BloomView, member: &MemberView) {
    // A withdrawn member raises nothing (#5327): an operator decided it, so
    // there is no unanswered condition for the war room to shout about, and a
    // wedge it carried on the way out is history rather than a live stop.
    if member.withdrawn.is_some() {
        return;
    }
    let focus = Focus::member(bloom.id, member.workpiece.clone());
    let detail = match &focus {
        Focus::Member { workpiece, .. } => workpiece.clone(),
        _ => bloom.id.prefix(),
    };
    if member.wedge.is_some() {
        alerts.push(Alert { kind: AlertKind::Wedge, detail: detail.clone(), focus: focus.clone() });
    }
    if member.host_fault.is_some() {
        alerts.push(Alert { kind: AlertKind::HostFault, detail, focus });
    }
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
        // The plausible bug: the walk only looks at bloom-level fields, so a
        // wedged or host-faulted member stays quiet.
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
        let kinds: Vec<_> = alerts(&view).into_iter().map(|alert| alert.kind).collect();
        assert_eq!(
            kinds,
            [AlertKind::Park, AlertKind::Landing, AlertKind::Fault, AlertKind::Wedge, AlertKind::HostFault,]
        );
    }
}
