//! Argv-safe transport for the composed work-order task (#5161).
//!
//! The lane child receives its task as one `--task <text>` argv string, and
//! Linux caps a single `execve` argument at 128KiB (`MAX_ARG_STRLEN`). The
//! composed task is the sealed description plus unbounded advisory overlays —
//! review findings (ADR-0153) and the fold-conflict overlay (ADR-0189), whose
//! conflicted-candidate section grows with the diff it names — so a fat enough
//! overlay makes the spawn itself fail with `E2BIG`, before any lane runs.
//!
//! A file-based flag is not the fix: the lane's `xtask` is compiled from the
//! *sealed subject tree*, which predates any flag added here, so the coordinator
//! cannot hand an already-sealed dispatch a transport its lane CLI has never
//! heard of. The transport stays `--task <text>`; what changes is that an
//! over-budget task travels truncated, with the complete text spilled beside the
//! run's evidence and a pointer line naming it — the lane runs in a shell on
//! this host and can read the spill with its own tools.

use std::borrow::Cow;
use std::fs;
use std::io;
use std::path::Path;

/// Flags the work-order argv producer emits that take a following value. The
/// mock-lane argv consumer reads the same table, so a flag added here is skipped
/// as a value-pair rather than leaking into the positional command.
pub(super) const VALUE_TAKING_FLAGS: &[&str] = &[
    "--out",
    "--nonce",
    "--diff-base",
    "--subject",
    "--harness",
    "--model",
    "--effort",
    "--task",
    "--resume",
    "--seeded",
];

/// Whether `flag` is a work-order flag whose next argv word is its value.
#[must_use]
pub(super) fn takes_value(flag: &str) -> bool {
    VALUE_TAKING_FLAGS.contains(&flag)
}

/// Append `flag` and `value` as one pair. The flag must be in
/// [`VALUE_TAKING_FLAGS`] so the mock consumer stays able to skip it.
pub(super) fn push_value_flag(args: &mut Vec<String>, flag: &'static str, value: impl Into<String>) {
    debug_assert!(takes_value(flag), "{flag} takes a value but is not in VALUE_TAKING_FLAGS");
    args.push(flag.to_owned());
    args.push(value.into());
}

/// The largest task the spawn passes through argv untouched. Comfortably under
/// the kernel's 128KiB per-argument cap, leaving room for the pointer suffix an
/// over-budget task gains and for the rest of the argv/environment block.
pub(super) const ARGV_TASK_BUDGET_BYTES: usize = 96 * 1024;

/// The composed task as the spawn may carry it: the text itself when it fits
/// the budget, else its head truncated at a char boundary plus a pointer line
/// naming the spill file — written to `evidence_dir` first, carrying the exact
/// original text.
///
/// # Errors
/// Writing the spill file failed. The caller refuses the spawn rather than
/// dispatching a task whose named spill does not exist.
pub(super) fn argv_safe_task<'task>(task: &'task str, evidence_dir: &Path) -> io::Result<Cow<'task, str>> {
    if task.len() <= ARGV_TASK_BUDGET_BYTES {
        return Ok(Cow::Borrowed(task));
    }

    let spill = evidence_dir.join("task-full.md");
    fs::write(&spill, task)?;
    let mut cut = ARGV_TASK_BUDGET_BYTES;
    while !task.is_char_boundary(cut) {
        cut -= 1;
    }
    Ok(Cow::Owned(format!(
        "{}\n\n[The task was truncated here at {cut} of {} bytes — one argv string cannot carry it. \
         The complete text is on this host at {}; read it before acting on the truncated tail.]",
        &task[..cut],
        task.len(),
        spill.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_within_budget_passes_through_untouched_and_spills_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let task = "a".repeat(ARGV_TASK_BUDGET_BYTES);

        let passed = argv_safe_task(&task, dir.path()).expect("within budget");

        assert!(matches!(passed, Cow::Borrowed(_)), "an in-budget task must not be rewritten");
        assert!(!dir.path().join("task-full.md").exists(), "an in-budget task must not spill");
    }

    #[test]
    fn an_over_budget_task_is_truncated_on_a_char_boundary_and_spilled_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A multi-byte char straddling the cut exercises the boundary walk-back.
        let task = "é".repeat(ARGV_TASK_BUDGET_BYTES);

        let passed = argv_safe_task(&task, dir.path()).expect("spill written");

        // Tripwire: the whole point of this module — the argv string stays under
        // the kernel's 128KiB per-argument cap however large the task grows.
        assert!(passed.len() < 128 * 1024, "argv task must stay under MAX_ARG_STRLEN, got {}", passed.len());
        let spill_path = dir.path().join("task-full.md");
        assert!(passed.contains(spill_path.to_str().expect("utf-8 path")), "the pointer must name the spill");
        assert_eq!(fs::read_to_string(spill_path).expect("spill readable"), task, "the spill is the exact task");
        let head_bytes = passed.find("\n\n[The task was truncated").expect("the pointer follows the head");
        assert!(task.starts_with(&passed[..head_bytes]), "the head survives verbatim");
        assert!(
            (ARGV_TASK_BUDGET_BYTES - 4..=ARGV_TASK_BUDGET_BYTES).contains(&head_bytes),
            "the cut walks back only to the nearest char boundary, got {head_bytes}"
        );
    }
}
