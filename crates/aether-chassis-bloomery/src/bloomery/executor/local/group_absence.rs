//! Strict process-group absence for fixture-root deletion (#5691).
//!
//! Production `ps_listing_has_live_member` skips malformed rows and decodes
//! stdout lossily; `proc_listing_is_live` treats an unreadable `observe` as
//! "not a member". Neither may license deleting a scratch root. This helper
//! asks `ps -A -o pgid= -o state=` (the same argv production uses) and only
//! reports [`GroupAbsence::Absent`] when the table is valid UTF-8, every
//! nonempty row parses as exactly two fields, and no live (non-zombie) member
//! of the target group remains. A totally empty table is [`GroupAbsence::Unknown`]:
//! a real `ps -A` includes this process.

use std::process::{Command, Stdio};
use std::str;

/// Whether a process group still has a live member, strictly enough to delete
/// a fixture root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupAbsence {
    /// Valid table, at least one row, no live member of the target group.
    Absent,
    /// Valid table, a live (non-zombie) member of the target group.
    Occupied,
    /// Spawn failed, nonzero status, non-UTF-8 stdout, malformed row, empty
    /// table, or a pgid this helper will not name.
    Unknown,
}

/// Observe whether `pgid` has a live member, using a strict `ps` table.
///
/// Group 0, group 1, and a pgid that does not fit a signed pid are
/// [`GroupAbsence::Unknown`] without spawning `ps`.
#[must_use]
pub fn strict_group_absence(pgid: u32) -> GroupAbsence {
    if !pgid_is_a_private_group(pgid) {
        return GroupAbsence::Unknown;
    }
    let output = Command::new("ps")
        .args(["-A", "-o", "pgid=", "-o", "state="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return GroupAbsence::Unknown;
    };
    if !output.status.success() {
        return GroupAbsence::Unknown;
    }
    parse_strict_ps_table(&output.stdout, pgid)
}

fn pgid_is_a_private_group(pgid: u32) -> bool {
    i32::try_from(pgid).is_ok_and(|pid| pid > 1)
}

fn parse_strict_ps_table(stdout: &[u8], pgid: u32) -> GroupAbsence {
    let Ok(text) = str::from_utf8(stdout) else {
        return GroupAbsence::Unknown;
    };
    let mut occupied = false;
    let mut rows = 0_usize;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(first) = fields.next() else {
            continue;
        };
        let Some(state) = fields.next() else {
            return GroupAbsence::Unknown;
        };
        if fields.next().is_some() {
            return GroupAbsence::Unknown;
        }
        let Ok(member_pgid) = first.parse::<u32>() else {
            return GroupAbsence::Unknown;
        };
        rows += 1;
        if member_pgid == pgid && !state.starts_with('Z') {
            occupied = true;
        }
    }
    if rows == 0 {
        return GroupAbsence::Unknown;
    }
    if occupied {
        GroupAbsence::Occupied
    } else {
        GroupAbsence::Absent
    }
}

#[cfg(test)]
mod tests {
    use super::{GroupAbsence, parse_strict_ps_table, strict_group_absence};

    #[test]
    fn an_empty_table_is_unknown_not_absent() {
        assert_eq!(parse_strict_ps_table(b"", 10), GroupAbsence::Unknown);
        assert_eq!(parse_strict_ps_table(b"\n\n  \n", 10), GroupAbsence::Unknown);
    }

    #[test]
    fn other_groups_and_no_target_are_absent() {
        assert_eq!(parse_strict_ps_table(b"    99 S\n     1 R\n", 10), GroupAbsence::Absent);
    }

    #[test]
    fn a_running_member_is_occupied() {
        assert_eq!(parse_strict_ps_table(b"    10 S\n    99 R\n", 10), GroupAbsence::Occupied);
    }

    #[test]
    fn a_zombie_in_the_target_group_is_not_occupying() {
        assert_eq!(parse_strict_ps_table(b"    10 Z+\n    99 R\n", 10), GroupAbsence::Absent);
    }

    #[test]
    fn invalid_utf8_is_unknown() {
        assert_eq!(parse_strict_ps_table(b"10 S\n\xff\n", 10), GroupAbsence::Unknown);
    }

    #[test]
    fn a_malformed_row_makes_the_whole_table_unknown() {
        // Tripwire: production `ps_listing_has_live_member` skips garbage and
        // can report the target empty. A leftover malformed row must not
        // license deletion, even when every valid row belongs to another group.
        assert_eq!(parse_strict_ps_table(b"not a ps line\n    99 S\n", 10), GroupAbsence::Unknown);
        assert_eq!(parse_strict_ps_table(b"10\n    99 S\n", 10), GroupAbsence::Unknown);
        assert_eq!(parse_strict_ps_table(b"10 S extra\n    99 S\n", 10), GroupAbsence::Unknown);
        assert_eq!(parse_strict_ps_table(b"x S\n    99 S\n", 10), GroupAbsence::Unknown);
    }

    #[test]
    fn an_unnameable_pgid_is_unknown_without_parsing_a_table() {
        assert_eq!(strict_group_absence(0), GroupAbsence::Unknown);
        assert_eq!(strict_group_absence(1), GroupAbsence::Unknown);
        assert_eq!(strict_group_absence(u32::MAX), GroupAbsence::Unknown);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn an_owned_child_group_is_occupied_until_the_child_exits() {
        use std::os::unix::process::CommandExt as _;
        use std::process::{Child, Command, Stdio};

        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let child = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the probe child forks");
        let pgid = child.id();
        let mut guard = ChildGuard(child);
        assert!(pgid > 1, "the child must own a private group");
        assert_eq!(strict_group_absence(pgid), GroupAbsence::Occupied, "a just-spawned group must look occupied");
        guard.0.kill().expect("the probe child is signalled");
        guard.0.wait().expect("the probe child is reaped");
        assert_eq!(strict_group_absence(pgid), GroupAbsence::Absent, "a waited-out group must look absent");
    }
}
