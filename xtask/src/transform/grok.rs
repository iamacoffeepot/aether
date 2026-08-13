//! The Grok harness arm: fork Grok Build headless and derive the shared
//! result-record envelope from its Anthropic-Messages NDJSON transcript.
//!
//! Grok Build's `--output-format streaming-messages-json` is the Anthropic
//! Messages API wire format, terminal `result` record included, so this arm
//! derives its record through [`messages::derive_result_record`] — the same
//! path the Claude arm reads — rather than a second parse of the same shape.
//! The price the terminal reports rides that record as evidence; what a bloom
//! is charged is computed host-side from the sealed price table over the token
//! columns, never from a harness's own figure.
//!
//! Auth is ambient. The child inherits this process's environment, so a
//! host whose operator is logged in (`apiKeySource: "oauth"`) and a host
//! carrying only a `GROK_CODE_XAI_API_KEY` both resolve their own credential —
//! the lane handles no secret, exactly as the Claude and Codex arms do not.

use std::process::{Command, Stdio};

use anyhow::Result;

use crate::transform::TransformArgs;
use crate::transform::lane::{capture, write_prompt};
use crate::transform::messages::derive_result_record;
use crate::transform::sccache::{self, CompilerCache};
use crate::transform::scratch::Scratch;

/// The harness's runner-facing name, for the binary and for error text.
const GROK: &str = "grok";

/// Grok's reasoning-effort spelling for a resolved tier. Its vocabulary is
/// `low|medium|high|xhigh`, which matches `ReasoningEffort::as_str` except at
/// the top: the ladder's `max` has no Grok counterpart, so it renders as the
/// deepest tier Grok does offer. Rendered rather than passed through because
/// Grok *refuses* an effort it does not know ("unknown effort level 'max'")
/// and exits before the run starts — a stage calibrated at `Max` would fail at
/// the child, having done no work, rather than reasoning as deeply as it can.
fn grok_effort(effort: &str) -> &str {
    match effort {
        "max" => "xhigh",
        other => other,
    }
}

/// The `grok` argv for a model-lane run.
///
/// `--prompt-file` is both the prompt source and what makes the run headless —
/// single-turn, printing to stdout and exiting — so the lane's assembled prompt
/// stays out of argv (and out of any process listing) while `-p`'s interactive
/// sibling never starts.
///
/// `--permission-mode bypassPermissions` is the write gate: a lane that cannot
/// edit investigates for turns and leaves a clean worktree, which reads
/// downstream as `produced_candidate: false`. The narrower `--always-approve`
/// covers tool execution only.
///
/// The four hygiene flags keep the run a single deterministic worker. Grok's
/// own subagent and plan machinery would fan the lane out inside a checkout the
/// bloomery owns, cross-session memory would carry state between two runs the
/// ledger treats as independent, and web search would source the work from
/// outside the sealed subject.
///
/// No turn cap rides here: nothing upstream seals one, and a number invented at
/// the arm would truncate a long repair lap into a `no_result` row.
fn grok_argv(prompt_file: &str, model: Option<&str>, effort: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "--prompt-file".to_owned(),
        prompt_file.to_owned(),
        "--output-format".to_owned(),
        "streaming-messages-json".to_owned(),
        "--permission-mode".to_owned(),
        "bypassPermissions".to_owned(),
        "--no-subagents".to_owned(),
        "--no-plan".to_owned(),
        "--no-memory".to_owned(),
        "--disable-web-search".to_owned(),
    ];
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    if let Some(effort) = effort {
        argv.push("--reasoning-effort".to_owned());
        argv.push(grok_effort(effort).to_owned());
    }
    argv
}

/// Run a model lane under Grok and return the shared result record.
pub(super) fn run(
    prompt: &str,
    args: &TransformArgs,
    scratch: &Scratch,
    cache: Option<&CompilerCache>,
) -> Result<serde_json::Value> {
    let prompt_file = write_prompt(&args.out, prompt)?;
    let mut command = Command::new(GROK);
    command
        .args(grok_argv(&prompt_file.to_string_lossy(), args.model.as_deref(), args.effort.as_deref()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scratch.export(&mut command);
    sccache::export(cache, &mut command);

    Ok(derive_result_record(&capture(command, &args.out, GROK)?))
}

#[cfg(test)]
mod tests {
    use super::{grok_argv, grok_effort};
    use crate::transform::messages::derive_result_record;

    #[test]
    fn argv_runs_headless_and_carries_the_resolved_profile() {
        let argv = grok_argv("/run/prompt.md", Some("grok-4.6"), Some("high"));
        assert!(
            argv.windows(2).any(|w| w == ["--prompt-file", "/run/prompt.md"]),
            "the prompt is read from the file, never handed to argv",
        );
        // Any other output format emits no `result` record at all, so every run
        // would derive the fail-closed `no_result` row however well it went.
        assert!(
            argv.windows(2).any(|w| w == ["--output-format", "streaming-messages-json"]),
            "the Anthropic-Messages transcript is what the record derives from",
        );
        // Tripwire: without the write gate open, a headless lane investigates and
        // then leaves a clean worktree — the `produced_candidate: false` wedge the
        // Claude arm hit before it carried its own bypass (#4874).
        assert!(
            argv.windows(2).any(|w| w == ["--permission-mode", "bypassPermissions"]),
            "headless needs the write gate open",
        );
        for hygiene in ["--no-subagents", "--no-plan", "--no-memory", "--disable-web-search"] {
            assert!(argv.iter().any(|flag| flag == hygiene), "a lane is a single deterministic worker: {hygiene}");
        }
        assert!(argv.windows(2).any(|w| w == ["--model", "grok-4.6"]), "the resolved model rides argv");
        assert!(argv.windows(2).any(|w| w == ["--reasoning-effort", "high"]), "the resolved effort rides argv");
    }

    #[test]
    fn argv_omits_the_profile_flags_when_none_falls_back_to_ambient() {
        let argv = grok_argv("/run/prompt.md", None, None);
        assert!(!argv.iter().any(|flag| flag == "--model"), "no resolved model means the operator's default");
        assert!(!argv.iter().any(|flag| flag == "--reasoning-effort"), "no resolved effort means the same");
        assert!(argv.windows(2).any(|w| w == ["--prompt-file", "/run/prompt.md"]), "still headless");
    }

    // Tripwire: the sealed ladder's top tier against Grok's vocabulary. Grok
    // knows `xhigh, high, medium, low` and refuses anything else outright —
    // `grok --reasoning-effort max` exits with "unknown effort level 'max'"
    // before the run starts, so a `Max`-calibrated stage would burn an attempt
    // producing nothing at all.
    #[test]
    fn the_ladders_top_tier_renders_as_the_deepest_effort_grok_knows() {
        assert_eq!(grok_effort("max"), "xhigh");
        for tier in ["low", "medium", "high", "xhigh"] {
            assert_eq!(grok_effort(tier), tier, "{tier} is already Grok's own spelling");
        }
    }

    // The transcript shape a live `grok --output-format streaming-messages-json`
    // run emits (Grok Build 1.0.3), trimmed to the two records the derivation
    // reads. Grok reporting its meters under different keys — cost only inside
    // `modelUsage`, say — would leave the shared derivation silently filling the
    // ledger's cost and token columns with nulls and zeros, which prices the
    // attempt as free.
    #[test]
    fn a_live_grok_terminal_fills_the_ledger_columns_through_the_shared_derivation() {
        let transcript = concat!(
            r#"{"type":"assistant","message":{"id":"msg_0","type":"message","role":"assistant","model":"grok-4.6","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":13525,"output_tokens":33,"cache_read_input_tokens":256,"cache_creation_input_tokens":0}}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":2055,"duration_api_ms":1357,"num_turns":1,"result":"ok","total_cost_usd":0.027376,"usage":{"input_tokens":13525,"output_tokens":33,"cache_read_input_tokens":256,"cache_creation_input_tokens":0},"modelUsage":{"grok-4.6-build":{"inputTokens":13525,"outputTokens":33,"costUSD":0.027376}}}"#,
        );
        let record = derive_result_record(transcript);

        assert_eq!(record["is_error"], false, "the construct gate reads this");
        assert_eq!(record["result"]["result"], "ok", "the review lane reads its verdict text here");
        assert_eq!(record["num_turns"], 1);
        assert_eq!(record["duration_ms"], 2055);
        assert_eq!(record["input"], 13525, "the token columns the sealed price table is applied to");
        assert_eq!(record["cache_read"], 256);
        assert_eq!(record["output"], 33);
        // Recorded as evidence; the bloom's spend is computed from the sealed
        // price table over the columns above, never from this figure.
        assert_eq!(record["cost_usd"], 0.027376);
        assert_eq!(record["first_call_model"], "grok-4.6", "no model here is filtered as a side model");
    }
}
