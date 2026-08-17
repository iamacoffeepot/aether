//! The shared model-lane body: assemble the prompt, fork headless Claude,
//! and derive the result record both the `construct.implement` and
//! `review.critic` lanes run through.

use std::fs;

use anyhow::{Context, Result, bail};

use aether_bloomery::split_lane_identity;

use crate::transform::lane::{execute, resume_handle_rejected, resumed_prompt, without_resume};
use crate::transform::messages::derive_result_record;
use crate::transform::peak_memory::PeakMemory;
use crate::transform::review::REVIEW_CRITIC;
use crate::transform::review_mcp;
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

/// Assemble the headless-Claude prompt for the construct lane from the lane-owned
/// `instructions`, the curated lane context, the checked-out `subject`, and the
/// work-order `task` — pure so the assembly is testable without spawning Claude
/// (#3572). The subject header names the exact sealed tree the worker is on; the
/// `## Task` section carries the operator's work-order description (#3595) so
/// the model is told *what* to build, not just *where*.
///
/// `seeded` names the construct checkpoint this dispatch resumes from (#4994).
/// Present only when the reducer seeded the checkout from a dead attempt's
/// partial capture; the prompt then names that commit and its trust-but-verify
/// posture. `None` is a cold start from the sealed (or spliced) base, and the
/// prompt says nothing about a checkpoint the worker does not have. The section
/// sits after the work order so a cold sibling still shares the cached prefix
/// through the stable bulk (#4985).
///
/// Prompt caching is prefix-exact (#4985). The shared bulk leads — conventions
/// first (the same curated lane context every lane of a bloom inlines, #5141),
/// then the lane instructions, subject, and work-order body — and anything that
/// varies per lane (a leading `Workpiece:` identity header, #4984) sits in a
/// trailing `## Lane` section. Sibling lanes then share the cached prefix; each
/// writes only the tail. A `None` task appends none (the fail-legible path for a
/// member with no persisted description). The conventions section is never
/// optional: a missing `lane_context.md` is a compile error, not a silent omit.
pub(super) fn assemble_construct_prompt(
    instructions: &str,
    subject: Option<&str>,
    task: Option<&str>,
    seeded: Option<&str>,
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
    let conventions_section = format!("{}\n\n", conventions::section());
    let (task_body, lane_identity) = task.map_or(("", None), split_lane_identity);
    let task_section = if task_body.is_empty() {
        String::new()
    } else {
        format!("\n## Task\n\n{task_body}\n")
    };
    let seeded_section = seeded.map_or_else(String::new, seeded_state_section);
    let lane_section = lane_identity.map_or_else(String::new, |id| format!("\n## Lane\n\n{id}\n"));
    format!(
        "{conventions_section}{instructions}\n\n## Subject\n\n{subject_line}\n{task_section}{seeded_section}{lane_section}"
    )
}

/// The trailing `## Seeded state` section: present only when this dispatch
/// resumes from a construct checkpoint (#4994). Names the commit and the
/// trust-but-verify posture a partial tree demands.
fn seeded_state_section(commit: &str) -> String {
    format!(
        "\n## Seeded state\n\n\
         This dispatch resumes from checkpoint `{commit}`. A prior attempt on this workpiece died \
         mid-stage and left that partial tree as your starting point rather than the clean sealed \
         base. The tree is untrusted: it can be mid-refactor garbage that does not compile. Verify \
         what is there before building on it, and discard it if it is not a foundation. A lane that \
         silently inherits a broken tree and assumes it is the base produces a worse candidate than \
         one that started cold.\n"
    )
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
    let mut claude = peak.command("claude");
    let mut flags = construct_argv(args.model.as_deref(), args.effort.as_deref(), args.resume.as_deref());
    // Tool injection is Claude-only. Codex / muse / grok review paths have no
    // MCP hook and keep the terminal `VERDICT:` parse.
    if args.command == REVIEW_CRITIC {
        let config = review_mcp::prepare(&args.out)?;
        flags.extend(review_mcp::mcp_argv(&config));
    }
    claude.args(flags);
    scratch.export(&mut claude);
    sccache::export(cache, &mut claude);
    // Piped stdin + streamed stdout share the lane primitive: the child is
    // reaped before any pipe-thread error returns, and a nonzero exit keeps
    // precedence over a broken prompt pipe.
    let run = execute(
        claude,
        &args.out,
        "headless claude",
        peak,
        Some(resumed_prompt(prompt, args.resume.as_deref()).into_bytes()),
    )?;
    // A non-zero exit is the CLI itself failing to run (auth, bad args, crash) —
    // an operational failure, distinct from a task-level error, which a completed
    // run records as `is_error` inside the transcript. Surface it rather than
    // writing an empty/garbage result record and reporting success.
    if args.resume.is_some() && resume_handle_rejected(run.status, &run.stdout, &run.stderr) {
        // Session file gone / unknown id — the CLI never started the billed
        // turn. Any other non-zero (auth, crash after tokens, SIGKILL) is
        // the operational failure the comment above names; retrying that
        // cold would double the spend this pool exists to cut. Any streamed
        // byte is already a transcript, so this degrade stays empty-only.
        return run_headless_claude(prompt, &without_resume(args), scratch, cache, peak);
    }
    if !run.status.success() {
        bail!(
            "headless claude exited {}: {}",
            run.status.code().map_or_else(|| "by signal".to_owned(), |c| c.to_string()),
            tail(&String::from_utf8_lossy(&run.stderr), 1000),
        );
    }

    // Derive the result record over the whole transcript, in-repo (no node
    // shell-out): the terminal `result` plus the first-call cache signal.
    Ok(derive_result_record(&String::from_utf8_lossy(&run.stdout)))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;
    use std::{env, fs, process};

    use super::{construct_argv, tail};
    use crate::transform::lane::{execute, resume_handle_rejected};
    use crate::transform::peak_memory;

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
        assert!(!argv.iter().any(|a| a == "--mcp-config"), "construct does not inject the review report server");
    }

    #[test]
    fn construct_argv_threads_the_resume_handle() {
        let argv = construct_argv(Some("claude-opus-4-8"), Some("high"), Some("sess-1"));
        let at = argv.iter().position(|a| a == "--resume").expect("a resume launch names the session");
        assert_eq!(argv[at + 1], "sess-1");
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

    /// A per-test evidence directory, unique per call so concurrent test threads
    /// never collide — the sibling lanes' convention.
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-claude-stream-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    // Claude's launch is the stdin-piped arm of the shared primitive. The
    // assembled prompt has to reach the child and the transcript, not sit in
    // an argv positional the way the other arms deliver theirs.
    #[test]
    fn the_prompt_reaches_the_child_on_stdin_and_lands_in_the_transcript() {
        let out = scratch_dir("stdin");
        let prompt = b"hello from stdin\n";
        let run = execute(Command::new("cat"), &out, "fixture", &peak_memory::detect(), Some(prompt.to_vec()))
            .expect("cat the prompt");
        assert!(run.status.success());
        assert_eq!(run.stdout, prompt);
        assert_eq!(fs::read(out.join("transcript.jsonl")).expect("read transcript"), prompt);
    }

    // Returning on the writer error before wait drops `Child` un-waited —
    // a zombie, or a live still-billing process. The child's exit stays the
    // error; a broken prompt pipe is the symptom of that exit.
    #[test]
    fn a_broken_prompt_pipe_reaps_the_child_and_does_not_mask_its_exit() {
        let out = scratch_dir("reap");
        let pidfile = out.join("pid");
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!("echo $$ > {}; exit 42", pidfile.display()));

        let run = execute(command, &out, "fixture", &peak_memory::detect(), Some(vec![b'x'; 1 << 20]))
            .expect("nonzero exit wins over a broken prompt pipe");
        assert_eq!(run.status.code(), Some(42), "the child's exit is the error, not EPIPE");
        let pid: u32 = fs::read_to_string(&pidfile).expect("pid").trim().parse().expect("pid number");
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "the child must be reaped, not left as a zombie or a live process"
        );
    }

    // Any streamed byte means the CLI ran. A resume-shaped stderr after that
    // is not a missing session, so the Claude arm must not fall back cold.
    #[test]
    fn streamed_output_keeps_a_resume_launch_from_falling_back() {
        let out = scratch_dir("no-fallback");
        let mut command = Command::new("sh");
        command.arg("-c").arg("printf '{\"type\":\"result\"}\\n'; echo 'No conversation found' >&2; exit 1");

        let run = execute(command, &out, "fixture", &peak_memory::detect(), Some(b"prompt\n".to_vec()))
            .expect("partial failure still returns the run");
        assert!(
            !resume_handle_rejected(run.status, &run.stdout, &run.stderr),
            "streamed output must not relaunch cold"
        );
        assert_eq!(run.stdout, b"{\"type\":\"result\"}\n");
    }
}
