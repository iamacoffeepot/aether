//! Owner-authority stops: nothing moves until a person acts.
//!
//! Narrower than the alert band. A landing still inside budget, a
//! non-terminal executor fault, and a host fault stay out of this queue.

use super::Focus;
use crate::dto::{BloomView, SpendQuiesce, StageId, ViewDocument};

/// One row in the interrupt queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interrupt {
    pub kind: InterruptKind,
    pub detail: String,
    pub focus: Focus,
    pub stage: Option<StageId>,
}

/// The eight source fields the queue is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptKind {
    Park,
    Decision,
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
    for bloom in &view.blooms {
        push_bloom_interrupts(&mut entries, bloom);
    }
    entries
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
        // A member waiting on a surface amendment is blocked on a *person*
        // (ADR-0207), so it belongs on the same list as a pending decision:
        // more attempts cannot move it.
        if member.awaiting_surface.is_some() {
            entries.push(Interrupt {
                kind: InterruptKind::Decision,
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
    use super::interrupts;
    use crate::dto::{BloomView, DigestHex, ExecutorFaultView, HostFaultView, LandingBlock, MemberView, ViewDocument};

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
}
