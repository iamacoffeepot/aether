//! The Muse harness arm: fork `muse exec` headless and derive the shared
//! result-record envelope from its JSONL transcript.

mod usage;

use std::process::{Command, Stdio};

use anyhow::Result;

use crate::transform::TransformArgs;
use crate::transform::lane::{Terminal, capture, record, write_prompt};
use crate::transform::scratch::Scratch;

/// The harness's runner-facing name, for the binary and for error text.
const MUSE: &str = "muse";

/// Muse's reasoning-effort spelling for a resolved tier. Its vocabulary is
/// `none|minimal|low|medium|high|xhigh|ultra`, which matches
/// `ReasoningEffort::as_str` everywhere except the top tier — Muse calls it
/// `ultra`. Rendered here rather than passed through, so a calibrated tier
/// reaches the child as something it recognizes instead of being silently
/// ignored.
fn muse_effort(effort: &str) -> &str {
    match effort {
        "max" => "ultra",
        other => other,
    }
}

/// The `muse exec` argv for a model-lane run.
///
/// `--disable-approval` is what makes it headless; the sandbox stays **on**,
/// because the run's scratch worktree is exactly the blast radius it should
/// have. The blanket `--yolo` would also disable the sandbox and trust the
/// workspace, which is more than a lane needs.
fn muse_argv(prompt_file: &str, model: Option<&str>, effort: Option<&str>) -> Vec<String> {
    let mut argv =
        vec!["exec".to_owned(), "--json".to_owned(), "--disable-approval".to_owned(), "--prompt-file".to_owned()];
    argv.push(prompt_file.to_owned());
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    if let Some(effort) = effort {
        argv.push("--reasoning-effort".to_owned());
        argv.push(muse_effort(effort).to_owned());
    }
    argv
}

/// Read the run's terminal out of a `muse exec --json` JSONL `transcript`.
///
/// Muse ends a run with a `run.terminal.*` record whose payload carries the
/// terminal state and the final text:
///
/// ```json
/// {"payload_type":"run.terminal.completed",
///  "payload":{"kind":"run_terminal","terminal":"completed","text":"…","reason":null}}
/// ```
///
/// `None` for a transcript with no terminal record — the caller renders that as
/// the fail-closed `no_result` row.
///
/// The terminal carries no `usage`: Muse keeps its token counts out of `--json`
/// entirely and writes them to its session log instead, so `run` fills them in
/// from there (`usage`) rather than from the transcript.
pub(super) fn derive_terminal(transcript: &str) -> Option<Terminal> {
    let mut terminal = None;
    for line in transcript.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // Match on the payload's own `kind` rather than the `payload_type`
        // suffix: the type string carries the terminal state in its tail
        // (`run.terminal.completed` / `.failed`), so keying on it would mean
        // parsing the state twice and could disagree with `payload.terminal`.
        let payload = event.get("payload")?;
        if payload.get("kind").and_then(serde_json::Value::as_str) != Some("run_terminal") {
            continue;
        }
        // Last terminal wins, mirroring the Claude arm's last-`result` rule.
        terminal = Some(Terminal {
            is_error: payload.get("terminal").and_then(serde_json::Value::as_str) != Some("completed"),
            text: payload.get("text").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned(),
            usage: None,
        });
    }
    terminal
}

/// Run a model lane under Muse and return the shared result record.
///
/// The token counts are joined on afterwards from the session log, keyed by the
/// id the transcript carries. A run whose log cannot be read still records its
/// attempt, with the columns null rather than zero.
pub(super) fn run(prompt: &str, args: &TransformArgs, scratch: &Scratch) -> Result<serde_json::Value> {
    let prompt_file = write_prompt(&args.out, prompt)?;
    let mut command = Command::new(MUSE);
    command
        .args(muse_argv(&prompt_file.to_string_lossy(), args.model.as_deref(), args.effort.as_deref()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scratch.export(&mut command);

    let transcript = capture(command, &args.out, MUSE)?;
    Ok(record(derive_terminal(&transcript).map(|terminal| Terminal {
        usage: usage::session_id(&transcript).and_then(|session| usage::from_session_log(&session)),
        ..terminal
    })))
}

#[cfg(test)]
mod tests {
    use super::{derive_terminal, muse_argv, muse_effort};

    #[test]
    fn argv_runs_headless_and_carries_the_resolved_profile() {
        let argv = muse_argv("/out/prompt.md", Some("muse-spark-1.2-contributor"), Some("high"));
        assert_eq!(argv.first().map(String::as_str), Some("exec"));
        assert!(argv.iter().any(|a| a == "--disable-approval"), "headless needs the approval prompt gone");
        assert!(argv.iter().any(|a| a == "--json"), "the transcript is what the record derives from");
        // The sandbox stays on: the run's scratch worktree is the blast radius
        // it should have, and --yolo would drop the sandbox too.
        assert!(!argv.iter().any(|a| a == "--yolo"), "the sandbox stays on");
        let model_at = argv.iter().position(|a| a == "--model").expect("argv pins the model");
        assert_eq!(argv[model_at + 1], "muse-spark-1.2-contributor");
        let effort_at = argv.iter().position(|a| a == "--reasoning-effort").expect("argv pins the effort");
        assert_eq!(argv[effort_at + 1], "high");

        // No resolved profile names neither flag, so the child falls back to the
        // operator's ambient defaults rather than a fabricated one.
        let bare = muse_argv("/out/prompt.md", None, None);
        assert!(!bare.iter().any(|a| a == "--model"));
        assert!(!bare.iter().any(|a| a == "--reasoning-effort"));
    }

    // Tripwire: Muse calls the top tier `ultra`, not `max`. Passing our own
    // spelling through would hand the child a value it does not recognize, so a
    // stage calibrated at the deepest tier would quietly run at the default.
    #[test]
    fn the_top_effort_tier_is_rendered_in_muses_own_spelling() {
        assert_eq!(muse_effort("max"), "ultra");
        for shared in ["low", "medium", "high", "xhigh"] {
            assert_eq!(muse_effort(shared), shared, "the shared tiers pass through unchanged");
        }
    }

    // The terminal record shape captured from a real `muse exec --json` run.
    #[test]
    fn the_terminal_record_yields_the_final_text_and_error_state() {
        let completed = concat!(
            r#"{"payload_type":"run.lifecycle.started","payload":{"kind":"run_started","prompt":"go"}}"#,
            "\n",
            r#"{"payload_type":"run.terminal.completed","payload":{"kind":"run_terminal","terminal":"completed","text":"VERDICT: pass","reason":null}}"#,
        );
        let terminal = derive_terminal(completed).expect("a terminal record is present");
        assert!(!terminal.is_error);
        assert_eq!(terminal.text, "VERDICT: pass");

        // Any terminal that is not `completed` is an error — the ~8% server-side
        // flake exits fast and clean having changed nothing, and must not read
        // as a successful run that simply produced no candidate.
        let failed = r#"{"payload_type":"run.terminal.failed","payload":{"kind":"run_terminal","terminal":"failed","text":"","reason":"server_error"}}"#;
        assert!(derive_terminal(failed).expect("present").is_error);

        // A run that died before its terminal record has none at all.
        assert!(
            derive_terminal(r#"{"payload_type":"run.lifecycle.started","payload":{"kind":"run_started"}}"#).is_none()
        );
        assert!(derive_terminal("").is_none());
    }
}
