//! Human-readable bloom list.

use std::fmt::Write as _;

use aether_bloomery::BloomStatus;

use super::dto::ViewDocument;

/// Render the live view: heads, then each bloom's status, members, and
/// supersession link.
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
        let _ = writeln!(out, "  {}  {}", bloom.id, status_label(bloom.status));
        if let Some(successor) = bloom.superseded_by {
            let _ = writeln!(out, "    superseded by  {successor}");
        }
        if bloom.members.is_empty() {
            let _ = writeln!(out, "    members        (none)");
            continue;
        }
        for member in &bloom.members {
            let _ = writeln!(out, "    member         {}  rev {}", member.workpiece, member.scope_revision);
        }
    }
    out
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
    use crate::bloom::dto::{BloomView, DigestHex, MemberView, ViewDocument};
    use aether_bloomery::BloomStatus;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    #[test]
    fn render_names_status_and_supersession() {
        let predecessor = digest(0x11);
        let successor = digest(0x22);
        let text = render(&ViewDocument {
            mainline: digest(1),
            observed: digest(2),
            blooms: vec![
                BloomView {
                    id: predecessor,
                    status: BloomStatus::Superseded,
                    superseded_by: Some(successor),
                    members: vec![MemberView {
                        workpiece: "wp-1".to_owned(),
                        scope_revision: digest(7),
                        awaiting_surface: None,
                        withdrawn: None,
                        cursor: None,
                    }],
                },
                BloomView {
                    id: successor,
                    status: BloomStatus::Sealed,
                    superseded_by: None,
                    members: vec![MemberView {
                        workpiece: "wp-1".to_owned(),
                        scope_revision: digest(7),
                        awaiting_surface: None,
                        withdrawn: None,
                        cursor: None,
                    }],
                },
            ],
        });

        assert!(text.contains("superseded"), "status is named: {text}");
        assert!(text.contains(&format!("superseded by  {successor}")), "supersession is linked: {text}");
        assert!(text.contains("wp-1"), "members are listed: {text}");
        assert!(text.contains(&format!("observed  {}", digest(2))), "observed head is shown: {text}");
    }
}
