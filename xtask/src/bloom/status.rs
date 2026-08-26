//! Human-readable bloom list.

use std::fmt::Write as _;

use aether_bloomery::{BloomStatus, BloomView, MemberView, ViewDocument};

/// Render the live view: heads, then each bloom's status, members, and
/// supersession link. A wedged, held, or parked member is named so a stopped
/// bloom is readable without a second query.
pub fn render(view: &ViewDocument) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "mainline  {}", view.mainline);
    let _ = writeln!(out, "observed  {}", view.observed);
    if view.blooms.is_empty() {
        let _ = writeln!(out, "blooms    (none)");
        return out;
    }
    let _ = writeln!(out, "blooms");
    for bloom in &view.blooms {
        render_bloom(&mut out, bloom);
    }
    out
}

fn render_bloom(out: &mut String, bloom: &BloomView) {
    let _ = writeln!(out, "  {}  {}", bloom.id, status_label(bloom.status));
    if let Some(successor) = bloom.superseded_by {
        let _ = writeln!(out, "    superseded by  {successor}");
    }
    if let Some(hold) = &bloom.operator_hold {
        let _ = writeln!(out, "    hold           {} ({})", hold.reason, hold.operator);
    }
    if bloom.members.is_empty() {
        let _ = writeln!(out, "    members        (none)");
        return;
    }
    for member in &bloom.members {
        render_member(out, member);
    }
}

fn render_member(out: &mut String, member: &MemberView) {
    let _ = writeln!(out, "    member         {}  rev {}", member.workpiece, member.scope_revision);
    if let Some(wedge) = &member.wedge {
        let _ = writeln!(out, "      wedge        {:?}  evidence {}", wedge.stage, wedge.evidence);
    }
    if let Some(park) = &member.park {
        let _ = writeln!(out, "      park         {:?}  evidence {}", park.stage, park.evidence);
    }
}

fn status_label(status: BloomStatus) -> &'static str {
    match status {
        BloomStatus::Sealed => "sealed",
        BloomStatus::Resolved => "resolved",
        BloomStatus::Landed => "landed",
        BloomStatus::Superseded => "superseded",
        BloomStatus::Withdrawn => "withdrawn",
    }
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::bloom::dto::{test_bloom, test_member, test_view};
    use aether_bloomery::{BloomStatus, Digest, MemberPark, OperatorHold, StageId, Wedge};

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    #[test]
    fn render_names_status_and_supersession() {
        let predecessor = digest(0x11);
        let successor = digest(0x22);
        let mut pred = test_bloom(predecessor, BloomStatus::Superseded, vec![test_member("wp-1", digest(7))]);
        pred.superseded_by = Some(aether_bloomery::BloomId(successor));
        let text = render(&test_view(
            digest(1),
            digest(2),
            vec![pred, test_bloom(successor, BloomStatus::Sealed, vec![test_member("wp-1", digest(7))])],
        ));

        assert!(text.contains("superseded"), "status is named: {text}");
        assert!(text.contains(&format!("superseded by  {successor}")), "supersession is linked: {text}");
        assert!(text.contains("wp-1"), "members are listed: {text}");
        assert!(text.contains(&format!("observed  {}", digest(2))), "observed head is shown: {text}");
    }

    #[test]
    fn render_names_a_wedge_a_hold_and_a_park() {
        // Tripwire: before the shared MemberView/BloomView these fields did not
        // exist client-side, so `xtask bloom status` could not name a stopped
        // member. A render that drops any of the three is a silent operator miss.
        let mut member = test_member("wp-wedged", digest(7));
        member.wedge = Some(Wedge {
            stage: StageId::Verify,
            evidence: digest(0xee),
            repeated_verifiers: aether_bloomery::VerifyFailureSet::EMPTY,
        });
        member.park = Some(MemberPark { stage: StageId::Construct, evidence: digest(0xcc) });
        let mut bloom = test_bloom(digest(0xab), BloomStatus::Sealed, vec![member]);
        bloom.operator_hold = Some(OperatorHold { reason: "wait for review".to_owned(), operator: "eve".to_owned() });

        let text = render(&test_view(digest(1), digest(2), vec![bloom]));
        assert!(text.contains("wedge"), "a wedged member is named: {text}");
        assert!(text.contains("hold"), "a held bloom is named: {text}");
        assert!(text.contains("wait for review"), "the hold reason is shown: {text}");
        assert!(text.contains("park"), "a parked member is named: {text}");
    }
}
