//! The shared model-lane body: assemble the prompt, fork headless Claude,
//! and derive the result record both the `construct.implement` and
//! `review.critic` lanes run through.

use std::io::Write;
use std::process::{ExitStatus, Stdio};
use std::{fs, thread};

use anyhow::{Context, Result, bail};

use crate::transform::messages::derive_result_record;
use crate::transform::peak_memory::PeakMemory;
use crate::transform::sccache::{self, CompilerCache};
use crate::transform::scratch::Scratch;
use crate::transform::{TransformArgs, conventions};

/// The headless-Claude argv the `construct.implement` lane runs (#3511): `-p`
/// non-interactive, emitting the stream-json transcript the in-repo
/// result-record derivation reads. `--model` and `--effort` are the CLI's own
/// flags for the two axes an agent profile calibrates, each included only when
/// the caller resolved one — when the caller resolves neither, both are omitted
/// and `claude -p` falls back to the operator's ambient defaults (#3592). Pure
/// so the profile wiring is testable without spawning Claude; the assembled
/// prompt is piped on the child's stdin (not an argv positional).
fn construct_argv(model: Option<&str>, effort: Option<&str>, resume: Option<&str>) -> Vec<String> {
    // `--dangerously-skip-permissions` is what makes the lane actually run
    // headless: `claude -p` under the default permission mode denies every
    // `Edit`/`Write`, and a non-interactive session has no way to grant one, so
    // the lane investigates for dozens of turns and leaves a clean worktree —
    // `produced_candidate: false` twice wedged the first live Claude member
    // (bloom `73d025b42e0a`, 2026-08-12). The sibling arms already carry their
    // headless equivalents (`--disable-approval` on muse, `--approve-for-me` on
    // codex); Claude's flag is broader because its bash-running lane needs more
    // than auto-accepted edits, and the lane is already the trust boundary's
    // narrow side (ADR-0152): a scrubbed environment, no credentials, a scratch
    // worktree, and every capture, commit, and push host-side.
    let mut argv = vec!["-p".to_owned(), "--dangerously-skip-permissions".to_owned()];
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    if let Some(effort) = effort {
        argv.push("--effort".to_owned());
        argv.push(effort.to_owned());
    }
    if let Some(session) = resume {
        argv.push("--resume".to_owned());
        argv.push(session.to_owned());
    }
    argv.push("--output-format".to_owned());
    argv.push("stream-json".to_owned());
    argv.push("--verbose".to_owned());
    argv
}

/// A copy of `args` with the resume handle cleared — the fresh-launch fallback
/// when the harness rejects `--resume`.
fn without_resume(args: &TransformArgs) -> TransformArgs {
    let mut args = args.clone();
    args.resume = None;
    args
}

/// Did the CLI refuse the `--resume` handle *before* starting a billed turn?
///
/// A non-zero exit after the CLI emitted a transcript is an operational
/// failure (auth, crash, SIGKILL) — not a missing session file — and must
/// not launch a second full-cost cold run. Spawn failures never reach here.
fn resume_handle_rejected(status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> bool {
    if status.success() || !stdout.is_empty() {
        return false;
    }
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    let names_the_handle = stderr.contains("session") || stderr.contains("resume") || stderr.contains("conversation");
    let rejected = stderr.contains("not found")
        || stderr.contains("unknown")
        || stderr.contains("invalid")
        || stderr.contains("no conversation");
    names_the_handle && rejected
}

/// Assemble the headless-Claude prompt for the construct lane from the lane-owned
/// `instructions`, the subject tree's `conventions`, the checked-out `subject`,
/// and the work-order `task` — pure so the assembly is testable without spawning
/// Claude (#3572). The subject header names the exact sealed tree the worker is
/// on; the `## Task` section carries the operator's work-order description
/// (#3595) so the model is told *what* to build, not just *where*.
///
/// Both optional sections are presence-driven: a `None` task appends none (the
/// fail-legible path for a member with no persisted description), and `None`
/// conventions append none (a subject tree that carries no conventions file,
/// #4647). The order is deliberate — conventions are long and general, the work
/// order is short and specific, so the task stays last where the instructions
/// promise it.
pub(super) fn assemble_construct_prompt(
    instructions: &str,
    conventions: Option<&str>,
    subject: Option<&str>,
    task: Option<&str>,
) -> String {
    let subject_line = subject.map_or_else(
        || "You are working in the checked-out subject tree — the sealed source this work order named.".to_owned(),
        |subject| {
            format!(
                "You are working in the checked-out subject tree at commit `{subject}` — \
                 the exact sealed source this work order named.",
            )
        },
    );
    let conventions_section =
        conventions.map_or_else(String::new, |text| format!("\n{}\n", conventions::section(text)));
    let task_section = task.map_or_else(String::new, |task| format!("\n## Task\n\n{task}\n"));
    format!("{instructions}\n{conventions_section}\n## Subject\n\n{subject_line}\n{task_section}")
}

/// The last `max` bytes of `s`, snapped forward to a char boundary — for
/// carrying a bounded stderr tail into an operational-failure error.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Fork headless Claude with `prompt` on stdin, capture its stream-json
/// transcript to `<out>/transcript.jsonl`, and derive the result record — the
/// shared body of both model lanes (`construct.implement` / `review.critic`).
pub(super) fn run_headless_claude(
    prompt: &str,
    args: &TransformArgs,
    scratch: &Scratch,
    cache: Option<&CompilerCache>,
    peak: &PeakMemory,
) -> Result<serde_json::Value> {
    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;

    // Run headless Claude at the resolved model and reasoning effort — both the
    // CLI's own flags, so the calibrated profile reaches the child rather than an
    // env knob nothing on the other side reads.
    //
    // Under the host's peak-memory wrapper when it has one (#4912): what a
    // construct lane costs in RAM is the builds its agent drives, and the
    // wrapper's reading covers the whole reaped tree rather than this process.
    let prompt = if args.resume.is_some() {
        format!(
            "{prompt}\nThe working tree was reset since the previous attempt; do not assume files you edited last time are still there.\n"
        )
    } else {
        prompt.to_owned()
    };
    let mut claude = peak.command("claude");
    claude.args(construct_argv(args.model.as_deref(), args.effort.as_deref(), args.resume.as_deref()));
    scratch.export(&mut claude);
    sccache::export(cache, &mut claude);
    claude.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = claude.spawn().context("run headless claude")?;
    // Pipe the prompt on a separate thread and reap the child unconditionally: if
    // the write races an early exit (broken pipe) or the prompt outgrows the OS
    // pipe buffer, returning on the write error before `wait_with_output` would
    // drop `child` un-waited — `Child`'s `Drop` neither waits nor reaps — leaving a
    // zombie (or a live, still-billing process) behind. Waiting first, then
    // surfacing the writer's error, guarantees the child is reaped on every path.
    let mut stdin = child.stdin.take().context("headless claude stdin was not captured")?;
    let prompt_bytes = prompt.as_bytes().to_vec();
    // Infra thread in a build tool — no settlement/trace umbrella applies here;
    // it exists only to pipe stdin while the main thread reaps the child.
    #[allow(clippy::disallowed_methods)]
    let writer = thread::spawn(move || stdin.write_all(&prompt_bytes));
    let run = child.wait_with_output().context("await headless claude")?;
    // Before the exit check: a run that died still peaked at something, and the
    // reading is what the concurrency model is calibrated from either way.
    peak.observe(&run.stderr);
    // A non-zero exit is the CLI itself failing to run (auth, bad args, crash) —
    // an operational failure, distinct from a task-level error, which a completed
    // run records as `is_error` inside the transcript. Surface it rather than
    // writing an empty/garbage result record and reporting success. Check it
    // *before* the writer's result: an early child exit makes the stdin write race
    // a broken pipe, so joining the writer first would surface that downstream
    // symptom and mask the child's real exit cause.
    if args.resume.is_some() && resume_handle_rejected(run.status, &run.stdout, &run.stderr) {
        // Session file gone / unknown id — the CLI never started the billed
        // turn. Any other non-zero (auth, crash after tokens, SIGKILL) is
        // the operational failure the comment above names; retrying that
        // cold would double the spend this pool exists to cut.
        let _ = writer.join();
        return run_headless_claude(&prompt, &without_resume(args), scratch, cache, peak);
    }
    if !run.status.success() {
        bail!(
            "headless claude exited {}: {}",
            run.status.code().map_or_else(|| "by signal".to_owned(), |c| c.to_string()),
            tail(&String::from_utf8_lossy(&run.stderr), 1000),
        );
    }
    // The child exited zero, so a writer error here is an unexplained broken pipe
    // (or an OS-buffer overrun) worth propagating rather than a symptom of the exit
    // above.
    writer.join().expect("prompt-writer thread panicked").context("pipe the assembled prompt to headless claude")?;

    let transcript_path = args.out.join("transcript.jsonl");
    fs::write(&transcript_path, &run.stdout).with_context(|| format!("write {}", transcript_path.display()))?;

    // Derive the result record over the whole transcript, in-repo (no node
    // shell-out): the terminal `result` plus the first-call cache signal.
    Ok(derive_result_record(&String::from_utf8_lossy(&run.stdout)))
}

#[cfg(test)]
mod tests {
    use super::{construct_argv, resume_handle_rejected, tail};
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    #[test]
    fn tail_snaps_to_a_char_boundary_without_panicking() {
        assert_eq!(tail("short", 10), "short", "under the cap returns the whole string");
        assert_eq!(tail("abcdef", 3), "def", "the ascii byte cut is already a char boundary");
        // "aébc" is 5 bytes; the byte cut (5 - 3 = byte 2) lands inside the
        // 2-byte 'é'. Snapping forward to byte 3 drops the partial char rather
        // than slicing mid-char and panicking.
        assert_eq!(tail("aébc", 3), "bc");
    }

    // Tripwire: both calibrated axes reach the child as CLI flags. The effort used
    // to be exported as an `AETHER_CONSTRUCT_EFFORT` env var that neither this
    // repository nor the Claude Code CLI reads, so the lane ran at the operator's
    // ambient effort however the profile was calibrated (#4324).
    #[test]
    fn construct_argv_carries_the_resolved_model_and_effort_and_stream_json() {
        let argv = construct_argv(Some("claude-opus-4-8"), Some("high"), None);
        assert_eq!(argv.first().map(String::as_str), Some("-p"), "headless, non-interactive");
        let model_at = argv.iter().position(|a| a == "--model").expect("argv pins the model");
        assert_eq!(argv[model_at + 1], "claude-opus-4-8", "the resolved model rides argv");
        let effort_at = argv.iter().position(|a| a == "--effort").expect("argv pins the effort tier");
        assert_eq!(argv[effort_at + 1], "high", "the resolved effort rides argv, not an unread env var");
        // The stream-json transcript is what the result-record derivation reads.
        assert!(argv.windows(2).any(|w| w == ["--output-format", "stream-json"]), "emits the stream-json transcript");
        // Tripwire: without the permission bypass, headless `claude -p` denies
        // every write and the lane wedges on `produced_candidate: false`
        // (bloom `73d025b42e0a`).
        assert!(argv.iter().any(|a| a == "--dangerously-skip-permissions"), "headless needs the write gate open");
        assert!(!argv.iter().any(|a| a == "--resume"), "a cold launch names no session");
    }

    #[test]
    fn construct_argv_threads_the_resume_handle() {
        let argv = construct_argv(Some("claude-opus-4-8"), Some("high"), Some("sess-1"));
        let at = argv.iter().position(|a| a == "--resume").expect("a resume launch names the session");
        assert_eq!(argv[at + 1], "sess-1");
    }

    #[test]
    fn a_missing_session_is_a_resume_reject_and_a_crash_after_tokens_is_not() {
        // Tripwire: a non-zero exit used to relaunch cold whenever `--resume`
        // was on argv, so an auth failure or a crash that had already billed
        // doubled the spend. Only a handle the CLI refused *before* emitting
        // a transcript degrades.
        let failed = ExitStatus::from_raw(1 << 8);
        assert!(resume_handle_rejected(failed, b"", b"No conversation found with session ID sess-1"));
        assert!(resume_handle_rejected(failed, b"", b"error: unknown session"));
        assert!(
            !resume_handle_rejected(failed, br#"{"type":"result"}"#, b"No conversation found"),
            "a transcript means the CLI ran — do not double-spend"
        );
        assert!(
            !resume_handle_rejected(failed, b"", b"authentication failed"),
            "an auth failure is not a missing session file"
        );
        assert!(!resume_handle_rejected(ExitStatus::from_raw(0), b"", b"No conversation found"));
    }

    #[test]
    fn construct_argv_omits_the_profile_flags_when_none_falls_back_to_ambient() {
        let argv = construct_argv(None, None, None);
        assert_eq!(argv.first().map(String::as_str), Some("-p"), "headless, non-interactive");
        assert!(!argv.iter().any(|a| a == "--model"), "no resolved model means no --model flag (ambient default)");
        assert!(!argv.iter().any(|a| a == "--effort"), "no resolved effort means no --effort flag (ambient default)");
        assert!(
            argv.windows(2).any(|w| w == ["--output-format", "stream-json"]),
            "still emits the stream-json transcript"
        );
    }
}
