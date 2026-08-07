//! The Codex harness arm: fork `codex exec` headless and derive the shared
//! result-record envelope from its JSONL transcript.

use std::process::{Command, Stdio};

use anyhow::Result;

use crate::transform::TransformArgs;
use crate::transform::lane::{Terminal, Usage, capture, record, write_prompt};

/// The harness's runner-facing name, for the binary and for error text.
const CODEX: &str = "codex";

/// The `codex exec` argv for a model-lane run.
///
/// `-s workspace-write` plus `--approve-for-me` is the narrowest pair that runs
/// headless: approvals are routed through automatic review instead of a prompt,
/// and writes stay inside the run's scratch worktree. The blanket
/// `--dangerously-bypass-approvals-and-sandbox` would also drop the sandbox,
/// which is more than a lane needs.
///
/// Codex reads its prompt from a positional argument (it has no `--prompt-file`);
/// the assembled prompt is still written beside the transcript by the caller.
fn codex_argv(prompt: &str, model: Option<&str>, effort: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "exec".to_owned(),
        "--json".to_owned(),
        "-s".to_owned(),
        "workspace-write".to_owned(),
        "--approve-for-me".to_owned(),
    ];
    if let Some(model) = model {
        argv.push("-m".to_owned());
        argv.push(model.to_owned());
    }
    if let Some(effort) = effort {
        // Codex takes reasoning effort as a config override rather than a flag.
        argv.push("-c".to_owned());
        argv.push(format!("model_reasoning_effort={effort}"));
    }
    argv.push(prompt.to_owned());
    argv
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
pub(super) fn run(prompt: &str, args: &TransformArgs) -> Result<serde_json::Value> {
    write_prompt(&args.out, prompt)?;
    let mut command = Command::new(CODEX);
    command
        .args(codex_argv(prompt, args.model.as_deref(), args.effort.as_deref()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(record(derive_terminal(&capture(command, &args.out, CODEX)?)))
}

#[cfg(test)]
mod tests {
    use super::{codex_argv, derive_terminal};

    #[test]
    fn argv_runs_headless_and_carries_the_resolved_profile() {
        let argv = codex_argv("do the thing", Some("gpt-5"), Some("high"));
        assert_eq!(argv.first().map(String::as_str), Some("exec"));
        assert!(argv.iter().any(|a| a == "--json"), "the transcript is what the record derives from");
        assert!(argv.windows(2).any(|w| w == ["-s", "workspace-write"]), "writes stay in the run's worktree");
        assert!(argv.iter().any(|a| a == "--approve-for-me"), "headless needs the approval prompt gone");
        // The blanket bypass would drop the sandbox too, which is more than a
        // lane needs.
        assert!(!argv.iter().any(|a| a == "--dangerously-bypass-approvals-and-sandbox"), "the sandbox stays on");
        assert!(argv.windows(2).any(|w| w == ["-m", "gpt-5"]), "the resolved model rides argv");
        assert!(
            argv.iter().any(|a| a == "model_reasoning_effort=high"),
            "codex takes effort as a config override, not a flag",
        );
        assert_eq!(argv.last().map(String::as_str), Some("do the thing"), "the prompt is the positional");
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
