//! Accumulate mock-lane scripts keyed by workpiece, stage, and occurrence.
//!
//! [`crate::ScenarioHarness::script_lane`] used to rebuild one global script on
//! every call and drop the workpiece, so configuring member B replaced member
//! A's fault and configuring Verify replaced Construct. The mock still reads
//! one file beside the run directories; this module is the write side that
//! keeps every member's and stage's steps in that file.

use std::io;
use std::path::Path;

use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript as MockLaneScript};

/// Replace the steps for `workpiece`'s `command` with `modes`, leaving every
/// other key intact.
///
/// An empty workpiece is bloom-less (`verify.base`, aggregate verify): those
/// steps stay unkeyed, matching the reserved empty member axis the order
/// already carries.
#[must_use]
pub fn apply_lane_scripts(
    mut script: MockLaneScript,
    workpiece: &str,
    command: &str,
    modes: impl IntoIterator<Item = LaneMode>,
) -> MockLaneScript {
    let key = (!workpiece.is_empty()).then_some(workpiece);
    script.steps.retain(|step| step.command != command || step.workpiece.as_deref() != key);
    modes.into_iter().fold(script, |script, mode| match key {
        Some(workpiece) => script.then_for(workpiece, command, mode),
        None => script.then(command, mode),
    })
}

/// Read the script in `dir` (or start from all-passing), apply `workpiece`'s
/// `command` steps, and write it back.
///
/// # Errors
/// The existing script could not be read (when present), or the file could not
/// be written.
pub fn write_lane_scripts(
    dir: &Path,
    workpiece: &str,
    command: &str,
    modes: impl IntoIterator<Item = LaneMode>,
) -> io::Result<()> {
    let script = match MockLaneScript::read_from(dir) {
        Ok(script) => script,
        Err(error) if error.kind() == io::ErrorKind::NotFound => MockLaneScript::all_passing(),
        Err(error) => return Err(error),
    };
    apply_lane_scripts(script, workpiece, command, modes).write_to(dir).map(|_| ())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a fixture that cannot set up its files reports it by panicking")]
mod tests {
    use aether_bloomery::{CONSTRUCT_IMPLEMENT_COMMAND, VERIFY_MEMBER_COMMAND};
    use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript as MockLaneScript};

    use super::{apply_lane_scripts, write_lane_scripts};

    #[test]
    fn a_second_member_keeps_the_first_members_construct_fault() {
        // Tripwire: each script_lane call used to start from all-passing and
        // overwrite the file, so B's Candidate erased A's Decline.
        let script = apply_lane_scripts(
            MockLaneScript::all_passing(),
            "wp-a",
            CONSTRUCT_IMPLEMENT_COMMAND,
            [LaneMode::Declines],
        );
        let script =
            apply_lane_scripts(script, "wp-b", CONSTRUCT_IMPLEMENT_COMMAND, [LaneMode::Pass]);

        assert_eq!(script.mode_for_workpiece(CONSTRUCT_IMPLEMENT_COMMAND, Some("wp-a"), 0), LaneMode::Declines);
        assert_eq!(script.mode_for_workpiece(CONSTRUCT_IMPLEMENT_COMMAND, Some("wp-b"), 0), LaneMode::Pass);
    }

    #[test]
    fn a_later_stage_keeps_the_earlier_stage_fault() {
        // Tripwire: configuring Verify used to replace the whole script, so a
        // Construct Decline became the default Pass and the member never parked.
        let script = apply_lane_scripts(
            MockLaneScript::all_passing(),
            "wp",
            CONSTRUCT_IMPLEMENT_COMMAND,
            [LaneMode::Declines],
        );
        let script = apply_lane_scripts(script, "wp", VERIFY_MEMBER_COMMAND, [LaneMode::Fail]);

        assert_eq!(script.mode_for_workpiece(CONSTRUCT_IMPLEMENT_COMMAND, Some("wp"), 0), LaneMode::Declines);
        assert_eq!(script.mode_for_workpiece(VERIFY_MEMBER_COMMAND, Some("wp"), 0), LaneMode::Fail);
    }

    #[test]
    fn a_second_call_for_the_same_key_replaces_that_sequence() {
        let script = apply_lane_scripts(
            MockLaneScript::all_passing(),
            "wp",
            CONSTRUCT_IMPLEMENT_COMMAND,
            [LaneMode::Declines, LaneMode::Declines],
        );
        let script = apply_lane_scripts(script, "wp", CONSTRUCT_IMPLEMENT_COMMAND, [LaneMode::Pass]);

        assert_eq!(
            script
                .steps
                .iter()
                .filter(|step| step.command == CONSTRUCT_IMPLEMENT_COMMAND && step.workpiece.as_deref() == Some("wp"))
                .count(),
            1,
            "a later script_lane for the same member and stage replaces that sequence",
        );
        assert_eq!(script.mode_for_workpiece(CONSTRUCT_IMPLEMENT_COMMAND, Some("wp"), 0), LaneMode::Pass);
    }

    #[test]
    fn write_accumulates_across_calls_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        write_lane_scripts(dir.path(), "wp-a", CONSTRUCT_IMPLEMENT_COMMAND, [LaneMode::Declines]).unwrap();
        write_lane_scripts(dir.path(), "wp-b", CONSTRUCT_IMPLEMENT_COMMAND, [LaneMode::Pass]).unwrap();

        let script = MockLaneScript::read_from(dir.path()).unwrap();
        assert_eq!(script.mode_for_workpiece(CONSTRUCT_IMPLEMENT_COMMAND, Some("wp-a"), 0), LaneMode::Declines);
        assert_eq!(script.mode_for_workpiece(CONSTRUCT_IMPLEMENT_COMMAND, Some("wp-b"), 0), LaneMode::Pass);
    }
}
