//! The Codex harness arm: fork `codex exec` headless and derive the shared
//! result-record envelope from its JSONL transcript.

use std::process::Stdio;

use anyhow::Result;

use crate::transform::TransformArgs;
use crate::transform::lane::{
    Terminal, Usage, capture, capture_resumed, record, resumed_prompt, without_resume, write_prompt,
};
use crate::transform::peak_memory::PeakMemory;
use crate::transform::sccache::{self, CompilerCache};
use crate::transform::scratch::Scratch;

/// The harness's runner-facing name, for the binary and for error text.
const CODEX: &str = "codex";

/// The `codex exec` argv for a model-lane run.
///
/// `--approve-for-me` alone is what runs the lane headless: it routes approvals
/// through automatic review instead of a prompt *and* implies the
/// workspace-write sandbox, so writes still stay inside the run's scratch
/// worktree. Naming the sandbox explicitly alongside it is not redundant but
/// fatal — codex-cli rejects the pair before the run starts ("the argument
/// `--sandbox <SANDBOX_MODE>` cannot be used with `--approve-for-me`"), which
/// would kill every codex lane at fork. The blanket
/// `--dangerously-bypass-approvals-and-sandbox` would drop the sandbox
/// altogether, which is more than a lane needs.
///
/// Codex reads its prompt from a positional argument (it has no `--prompt-file`);
/// the assembled prompt is still written beside the transcript by the caller.
///
/// Resume is a **subcommand**, not a flag, and its placement is exact:
/// `codex exec --json --approve-for-me [-m …] [-c …] resume <thread-id>
/// <prompt>`. The exec-level flags must precede `resume`, and the thread id must
/// precede the prompt positional — codex-cli rejects every other ordering, so a
/// flag appended after `resume` or an id placed after the prompt kills the lane
/// at fork rather than degrading.
fn codex_argv(prompt: &str, model: Option<&str>, effort: Option<&str>, resume: Option<&str>) -> Vec<String> {
    let mut argv = vec!["exec".to_owned(), "--json".to_owned(), "--approve-for-me".to_owned()];
    if let Some(model) = model {
        argv.push("-m".to_owned());
        argv.push(model.to_owned());
    }
    if let Some(effort) = effort {
        // Codex takes reasoning effort as a config override rather than a flag.
        argv.push("-c".to_owned());
        argv.push(format!("model_reasoning_effort={effort}"));
    }
    if let Some(thread) = resume {
        argv.push("resume".to_owned());
        argv.push(thread.to_owned());
    }
    argv.push(prompt.to_owned());
    argv
}

/// Read the thread id `codex exec --json` announces its session under.
///
/// ```json
/// {"type":"thread.started","thread_id":"019f…"}
/// ```
///
/// It is the handle a later lap resumes, and it arrives before any work, so a
/// run that died mid-lap still names the thread its retry can continue.
pub(super) fn derive_thread_id(transcript: &str) -> Option<String> {
    transcript.lines().find_map(|line| {
        let event = serde_json::from_str::<serde_json::Value>(line).ok()?;
        (event.get("type").and_then(serde_json::Value::as_str) == Some("thread.started"))
            .then(|| event.get("thread_id")?.as_str().map(str::to_owned))
            .flatten()
    })
}

/// Read the run's terminal out of a `codex exec --json` JSONL `transcript`.
///
/// Codex reports the final agent message and the terminal separately:
///
/// ```json
/// {"type":"item.completed","item":{"type":"agent_message","text":"…"}}
/// {"type":"turn.completed","usage":{"input_tokens":16147,"output_tokens":5, …}}
/// ```
///
/// so the text comes from the last `agent_message` item and the error state from
/// whether the run reached `turn.completed`. `None` for a transcript with no
/// terminal at all — the caller renders that as the fail-closed `no_result` row.
pub(super) fn derive_terminal(transcript: &str) -> Option<Terminal> {
    let mut text = String::new();
    let mut terminal: Option<Terminal> = None;
    for line in transcript.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match event.get("type").and_then(serde_json::Value::as_str) {
            Some("item.completed") => {
                let item = event.get("item")?;
                if item.get("type").and_then(serde_json::Value::as_str) == Some("agent_message")
                    && let Some(message) = item.get("text").and_then(serde_json::Value::as_str)
                {
                    // Last agent message wins — it is the run's final word, which
                    // is where the review lane's VERDICT line has to be.
                    message.clone_into(&mut text);
                }
            }
            // Any terminal that is not `turn.completed` — `turn.failed`, an
            // `error` — is a failed run, so the match is on the completed case
            // and everything else falls through as an error terminal.
            Some(kind @ ("turn.completed" | "turn.failed" | "error")) => {
                terminal = Some(Terminal {
                    is_error: kind != "turn.completed",
                    text: String::new(),
                    usage: event.get("usage").map(|usage| {
                        let count = |field: &str| usage.get(field).and_then(serde_json::Value::as_u64).unwrap_or(0);
                        Usage {
                            input: count("input_tokens"),
                            cache_read: count("cached_input_tokens"),
                            cache_write: count("cache_write_input_tokens"),
                            output: count("output_tokens"),
                        }
                    }),
                });
            }
            _ => {}
        }
    }
    // The text is collected across the whole transcript, so it is folded in only
    // once the terminal is known.
    terminal.map(|terminal| Terminal { text, ..terminal })
}

/// Run a model lane under Codex and return the shared result record.
pub(super) fn run(
    prompt: &str,
    args: &TransformArgs,
    scratch: &Scratch,
    cache: Option<&CompilerCache>,
    peak: &PeakMemory,
) -> Result<serde_json::Value> {
    let Some(transcript) = launch(prompt, args, scratch, cache, peak)? else {
        // Codex refused the thread before starting a billed turn — the session
        // is gone, so this lap is a cold one.
        return run(prompt, &without_resume(args), scratch, cache, peak);
    };
    Ok(record(derive_terminal(&transcript), derive_thread_id(&transcript)))
}

/// Fork Codex once and hand back its transcript, or `None` when a resumed launch
/// was refused its thread before spending anything.
fn launch(
    prompt: &str,
    args: &TransformArgs,
    scratch: &Scratch,
    cache: Option<&CompilerCache>,
    peak: &PeakMemory,
) -> Result<Option<String>> {
    let prompt = resumed_prompt(prompt, args.resume.as_deref());
    write_prompt(&args.out, &prompt)?;
    let mut command = peak.command(CODEX);
    command
        .args(codex_argv(&prompt, args.model.as_deref(), args.effort.as_deref(), args.resume.as_deref()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scratch.export(&mut command);
    sccache::export(cache, &mut command);

    if args.resume.is_some() {
        return capture_resumed(command, &args.out, CODEX, peak);
    }
    capture(command, &args.out, CODEX, peak).map(Some)
}

#[cfg(test)]
mod tests {
    use super::{codex_argv, derive_terminal, derive_thread_id};
    use crate::transform::lane::record;

    #[test]
    fn argv_runs_headless_and_carries_the_resolved_profile() {
        let argv = codex_argv("do the thing", Some("gpt-5"), Some("high"), None);
        assert_eq!(argv.first().map(String::as_str), Some("exec"));
        assert!(argv.iter().any(|a| a == "--json"), "the transcript is what the record derives from");
        assert!(argv.iter().any(|a| a == "--approve-for-me"), "headless needs the approval prompt gone");
        // Tripwire: `--approve-for-me` already implies the workspace-write
        // sandbox, and codex-cli refuses to start when `--sandbox` is also
        // named — so re-adding the flag for defense in depth kills every codex
        // lane at fork, before a token is spent.
        assert!(
            !argv.iter().any(|a| a == "-s" || a == "--sandbox"),
            "codex-cli rejects an explicit sandbox alongside --approve-for-me",
        );
        // The blanket bypass would drop the implied sandbox too, which is more
        // than a lane needs.
        assert!(!argv.iter().any(|a| a == "--dangerously-bypass-approvals-and-sandbox"), "the sandbox stays on");
        assert!(argv.windows(2).any(|w| w == ["-m", "gpt-5"]), "the resolved model rides argv");
        assert!(
            argv.iter().any(|a| a == "model_reasoning_effort=high"),
            "codex takes effort as a config override, not a flag",
        );
        assert_eq!(argv.last().map(String::as_str), Some("do the thing"), "the prompt is the positional");
        assert!(!argv.iter().any(|a| a == "resume"), "a cold launch names no thread");
    }

    // Tripwire: codex-cli accepts exactly one ordering for a resumed exec —
    // every exec-level flag before the `resume` subcommand, the thread id
    // between `resume` and the prompt positional. A flag pushed after `resume`
    // or an id after the prompt is not a degraded run but a parse error at fork,
    // so the whole lane dies before a token is spent.
    #[test]
    fn argv_puts_the_exec_flags_before_resume_and_the_thread_before_the_prompt() {
        let argv = codex_argv("do the thing", Some("gpt-5"), Some("high"), Some("019f"));
        let at = |needle: &str| argv.iter().position(|a| a == needle).unwrap_or_else(|| panic!("argv has {needle}"));

        assert_eq!(argv.first().map(String::as_str), Some("exec"));
        for flag in ["--json", "--approve-for-me", "-m", "-c"] {
            assert!(at(flag) < at("resume"), "{flag} is an exec-level flag and must precede the subcommand");
        }
        assert_eq!(argv[at("resume") + 1], "019f", "the thread id follows the subcommand");
        assert_eq!(argv.last().map(String::as_str), Some("do the thing"), "the prompt stays the last positional");
    }

    // Tripwire: the thread id the pool deposits comes off `thread.started`,
    // which codex emits before any work — so a run that died mid-lap still names
    // the thread its retry continues. Losing it leaves the pool with nothing to
    // key a warm lap on, and every retry relaunches cold at full price.
    #[test]
    fn the_started_threads_id_reaches_the_envelope_the_pool_deposits() {
        let transcript = concat!(
            r#"{"type":"thread.started","thread_id":"019f8c2a"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":16147,"output_tokens":5}}"#,
        );
        assert_eq!(derive_thread_id(transcript).as_deref(), Some("019f8c2a"));
        assert_eq!(record(derive_terminal(transcript), derive_thread_id(transcript))["session_id"], "019f8c2a");

        // A transcript that never announced a thread names none, rather than
        // depositing an empty handle a later lap would try to resume.
        assert_eq!(derive_thread_id(r#"{"type":"turn.started"}"#), None);
        assert_eq!(derive_thread_id(r#"{"type":"thread.started"}"#), None);
    }

    // The event shape captured from a real `codex exec --json` run: the final
    // text and the terminal arrive as separate records, so the derivation has to
    // join them rather than reading either alone.
    #[test]
    fn the_final_agent_message_joins_the_turn_terminal() {
        let transcript = concat!(
            r#"{"type":"thread.started","thread_id":"019f"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"VERDICT: pass"}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":16147,"cached_input_tokens":11008,"cache_write_input_tokens":0,"output_tokens":5}}"#,
        );
        let terminal = derive_terminal(transcript).expect("a terminal is present");
        assert!(!terminal.is_error);
        assert_eq!(terminal.text, "VERDICT: pass", "the agent message is the run's final word");
        let usage = terminal.usage.expect("codex does report token counts");
        assert_eq!(usage.input, 16147);
        assert_eq!(usage.cache_read, 11008);
        assert_eq!(usage.output, 5);

        // A failed turn is an error terminal even when the run emitted a message
        // first — otherwise a run that died mid-way reads as a clean conclusion.
        let failed = concat!(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"partial"}}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"boom"}}"#,
        );
        assert!(derive_terminal(failed).expect("present").is_error);

        // A run that never reached a terminal has none at all.
        assert!(derive_terminal(r#"{"type":"turn.started"}"#).is_none());
        assert!(derive_terminal("").is_none());
    }
}
