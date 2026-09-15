//! Token usage recovered from a lane that left no result record (issue 6029).
//!
//! The ledger prices a dispatch only from the evidence a lane deposits, so a
//! lane withdrawn, deadline-cancelled, or aborted mid-run spent tokens the
//! ledger never saw: its `evidence.json` result record — the one object every
//! priced path reads — was never written. But the spend is still observable.
//! The harness session log holds the per-call tokens (Muse), or the streamed
//! transcript does (the Claude/Grok arms derive their record from it,
//! including the `no_result` shape for a run that died early).
//!
//! [`recover_evidence_usage`] reads both, off the dispatch's own evidence
//! directory, and returns the unpriced token columns. Pricing stays where it
//! has always been — the intake prices measured tokens against the bloom's
//! sealed table — so a recovered usage rides the same study path as a
//! lane-reported one and `None` stays the honest unmeasured answer.

use std::path::{Path, PathBuf};
use std::{env, fs};

use aether_bloomery::{StudyCall, StudyCost};

/// The streamed provider transcript a model lane tees under its evidence
/// directory — the same file the heartbeat read owns, opened here read-only.
const TRANSCRIPT_FILE: &str = "transcript.jsonl";

/// The `YYYY/MM/DD` levels between `sessions` and a session's own directory —
/// the walk's bound, mirroring the xtask Muse reader, so a large history costs
/// a directory listing per level rather than a full tree traversal.
const DATE_DEPTH: usize = 3;

/// Measured tokens for one dispatch, unpriced: the price is a policy the
/// intake applies against the sealed table, never a number recovered here.
pub struct RecoveredUsage {
    /// Aggregate token columns, with a zero dollar column for the intake to price.
    pub cost: StudyCost,
    /// Per-call columns when the source reported them, so a long-context band
    /// can charge each call at the rate its own prompt selects.
    pub calls: Option<Vec<StudyCall>>,
}

/// Recover the tokens a dispatch spent from its evidence directory: the Muse
/// session log when the transcript names a session that has one, else the
/// usage the transcript itself carries (the Claude/Grok arms' derivation,
/// which survives a run that died before its terminal record).
///
/// `session_root_override` names the harness data root outright; `None` reads
/// it from the environment the way the lane's own reader does. `None` when
/// neither source yields a call — unmeasured, never free.
pub fn recover_evidence_usage(evidence_dir: &Path, session_root_override: Option<&Path>) -> Option<RecoveredUsage> {
    let transcript = fs::read_to_string(evidence_dir.join(TRANSCRIPT_FILE)).ok()?;
    recover_from_transcript(&transcript, session_root_override)
}

/// [`recover_evidence_usage`] over transcript text already in hand — the seam
/// the backend's cancel path reads the session id through before it persists
/// it on the order, and the shape the unit suite drives without a directory.
pub fn recover_from_transcript(transcript: &str, session_root_override: Option<&Path>) -> Option<RecoveredUsage> {
    if let Some(session) = session_id_from_transcript(transcript)
        && let Some(usage) = muse_session_usage(&session, session_root_override)
    {
        return Some(usage);
    }
    transcript_usage(transcript)
}

/// The harness session id a transcript names, when it names one: the Muse
/// stream id off the first record (a run that dies early still names its
/// session, so a partial run's tokens stay recoverable), else the terminal
/// result's handle, else the init record's.
///
/// A line that does not parse is skipped rather than ending the search: the
/// transcript this reads is the one a killed lane left, so its last line is
/// routinely a half-written record, and a fatal read there would lose the id
/// the earlier lines already named.
pub fn session_id_from_transcript(transcript: &str) -> Option<String> {
    let events = || transcript.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok());
    let tagged = |pointer: &'static str, tag: &'static str| {
        events().find_map(move |event| {
            (event.pointer(pointer).and_then(serde_json::Value::as_str) == Some(tag))
                .then(|| event.pointer("/session_id")?.as_str().map(str::to_owned))
                .flatten()
        })
    };
    events()
        .find_map(|event| event.pointer("/stream/id").and_then(serde_json::Value::as_str).map(str::to_owned))
        .or_else(|| tagged("/type", "result"))
        .or_else(|| tagged("/subtype", "init"))
}

/// Total the tokens Muse recorded for `session`, across the run and every
/// subagent it spawned — the xtask reader's arithmetic, over an explicit root
/// so the suite drives it without touching the process environment.
fn muse_session_usage(session: &str, session_root_override: Option<&Path>) -> Option<RecoveredUsage> {
    let session_dir = session_dir(&session_root(session_root_override)?, session)?;
    let mut cost = StudyCost::default();
    let mut calls = Vec::new();
    add_muse_log(&session_dir.join("session.jsonl"), &mut cost, &mut calls);
    for subagent in read_dir_sorted(&session_dir.join("subagent")) {
        add_muse_log(&subagent.join("session.jsonl"), &mut cost, &mut calls);
    }
    (!calls.is_empty()).then_some(RecoveredUsage { cost, calls: Some(calls) })
}

/// Muse's data root: the override when the caller named one, else
/// `$XDG_DATA_HOME/muse`, else `$HOME/.local/share/muse` — the directory Muse
/// honours, so a caller that redirected it still finds the log the lane wrote.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: the harness session log lives under the operator's data home, not cap config
fn session_root(session_root_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = session_root_override {
        return Some(root.to_owned());
    }
    if let Ok(xdg) = env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("muse"));
    }
    Some(PathBuf::from(env::var("HOME").ok()?).join(".local/share/muse"))
}

/// Find `sessions/*/*/*/<session>` under `root`, walking the date levels
/// rather than computing them: a run that starts before midnight and is read
/// after it would miss on a date computed at read time.
fn session_dir(root: &Path, session: &str) -> Option<PathBuf> {
    let mut days = vec![root.join("sessions")];
    for _ in 0..DATE_DEPTH {
        days = days.iter().flat_map(|dir| read_dir_sorted(dir)).collect();
    }
    days.iter().flat_map(|day| read_dir_sorted(day)).find(|dir| dir.file_name().is_some_and(|name| name == session))
}

/// The subdirectories of `dir`, sorted, or empty when it cannot be read.
///
/// Sorted so a total is assembled in the same order twice — the sum does not
/// depend on it, but a dump beside it reads the same on a re-run.
fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    entries
}

/// Add every `model_completed` step in the log at `path` into `cost`/`calls`.
///
/// A missing or malformed log contributes nothing rather than failing the
/// recovery: the attempt itself is already lost, and losing its cost row must
/// not turn a best-effort pricing into a fault.
fn add_muse_log(path: &Path, cost: &mut StudyCost, calls: &mut Vec<StudyCall>) {
    if let Ok(log) = fs::read_to_string(path) {
        add_muse_steps(&log, cost, calls);
    }
}

/// Add every `model_completed` step in the `log` text into `cost`/`calls`.
///
/// Muse's `input_tokens` counts the whole prompt, cached tokens included, so
/// the cache read is subtracted to leave the uncached input the other arms
/// report; `reasoning_tokens` is billed as output on top of `output_tokens`,
/// which is what the vendor meter charges.
fn add_muse_steps(log: &str, cost: &mut StudyCost, calls: &mut Vec<StudyCall>) {
    for step in log.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()) {
        let Some(usage) = step.pointer("/payload/event/usage").filter(|_| {
            step.pointer("/payload/event/kind").and_then(serde_json::Value::as_str) == Some("model_completed")
        }) else {
            continue;
        };
        let count = |field: &str| usage.get(field).and_then(serde_json::Value::as_u64).unwrap_or(0);
        let call = StudyCall {
            input_tokens: count("input_tokens").saturating_sub(count("cache_read_tokens")),
            cache_read_tokens: count("cache_read_tokens"),
            cache_write_tokens: count("cache_write_tokens"),
            output_tokens: count("output_tokens").saturating_add(count("reasoning_tokens")),
            ..StudyCall::default()
        };
        cost.input_tokens = cost.input_tokens.saturating_add(call.input_tokens);
        cost.cache_read_tokens = cost.cache_read_tokens.saturating_add(call.cache_read_tokens);
        cost.cache_write_tokens = cost.cache_write_tokens.saturating_add(call.cache_write_tokens);
        cost.output_tokens = cost.output_tokens.saturating_add(call.output_tokens);
        calls.push(call);
    }
}

/// The usage a transcript carries itself, in the Anthropic-Messages NDJSON
/// the Claude/Grok arms derive their record from: per-call columns off the
/// assistant messages (side models skipped), totals off the terminal result.
///
/// A transcript with no terminal still yields its calls summed — the
/// `no_result` shape, whose tokens are measured even though the run never
/// concluded. `None` when no line carried usage at all.
fn transcript_usage(transcript: &str) -> Option<RecoveredUsage> {
    let mut calls = Vec::new();
    let mut terminal: Option<StudyCost> = None;
    let mut turns = 0;
    let mut duration_millis = 0;
    for line in transcript.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match event.pointer("/type").and_then(serde_json::Value::as_str) {
            Some("assistant") => {
                let message = event.pointer("/message").cloned().unwrap_or(serde_json::Value::Null);
                if message.get("model").and_then(serde_json::Value::as_str).unwrap_or_default().contains("haiku") {
                    continue;
                }
                let Some(usage) = message.get("usage") else {
                    continue;
                };
                let creation = usage.get("cache_creation").cloned().unwrap_or(serde_json::Value::Null);
                calls.push(StudyCall {
                    input_tokens: count(usage, "input_tokens"),
                    cache_write_tokens: count(usage, "cache_creation_input_tokens"),
                    cache_write_1h_tokens: count(&creation, "ephemeral_1h_input_tokens"),
                    cache_write_5m_tokens: count(&creation, "ephemeral_5m_input_tokens"),
                    cache_read_tokens: count(usage, "cache_read_input_tokens"),
                    output_tokens: count(usage, "output_tokens"),
                });
            }
            Some("result") => {
                let usage = event.pointer("/usage").cloned().unwrap_or(serde_json::Value::Null);
                let creation = usage.get("cache_creation").cloned().unwrap_or(serde_json::Value::Null);
                turns = event.get("num_turns").and_then(serde_json::Value::as_u64).unwrap_or(0);
                duration_millis = event.get("duration_ms").and_then(serde_json::Value::as_u64).unwrap_or(0);
                terminal = Some(StudyCost {
                    input_tokens: count(&usage, "input_tokens"),
                    cache_write_tokens: count(&usage, "cache_creation_input_tokens"),
                    cache_write_1h_tokens: count(&creation, "ephemeral_1h_input_tokens"),
                    cache_write_5m_tokens: count(&creation, "ephemeral_5m_input_tokens"),
                    cache_read_tokens: count(&usage, "cache_read_input_tokens"),
                    output_tokens: count(&usage, "output_tokens"),
                    ..StudyCost::default()
                });
            }
            _ => {}
        }
    }
    if calls.is_empty() && terminal.is_none() {
        return None;
    }
    let concluded = terminal.is_some();
    let mut cost = terminal.unwrap_or_default();
    // A terminal that stated no token column is not a measurement — an
    // errored run's bare `is_error` must read as unmeasured, never as free.
    if calls.is_empty()
        && cost.input_tokens == 0
        && cost.cache_write_tokens == 0
        && cost.cache_write_1h_tokens == 0
        && cost.cache_write_5m_tokens == 0
        && cost.cache_read_tokens == 0
        && cost.output_tokens == 0
    {
        return None;
    }
    if !concluded {
        for call in &calls {
            cost.input_tokens = cost.input_tokens.saturating_add(call.input_tokens);
            cost.cache_write_tokens = cost.cache_write_tokens.saturating_add(call.cache_write_tokens);
            cost.cache_write_1h_tokens = cost.cache_write_1h_tokens.saturating_add(call.cache_write_1h_tokens);
            cost.cache_write_5m_tokens = cost.cache_write_5m_tokens.saturating_add(call.cache_write_5m_tokens);
            cost.cache_read_tokens = cost.cache_read_tokens.saturating_add(call.cache_read_tokens);
            cost.output_tokens = cost.output_tokens.saturating_add(call.output_tokens);
        }
    }
    cost.turns = turns;
    cost.duration_millis = duration_millis;
    Some(RecoveredUsage { cost, calls: (!calls.is_empty()).then_some(calls) })
}

/// One token column, or zero when the harness left it absent or null — the
/// lanes render an unreported column as an explicit null rather than omitting
/// the key, so only `as_u64` reads the shape they actually emit.
fn count(usage: &serde_json::Value, field: &str) -> u64 {
    usage.get(field).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;
    use std::{env, fs, process};

    use super::{recover_evidence_usage, recover_from_transcript, session_id_from_transcript};

    /// A per-test scratch directory under the system temp dir, unique per call
    /// so concurrent test threads never collide.
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = process::id();
        let dir = env::temp_dir().join(format!("aether-harness-usage-{tag}-{pid}-{seq}"));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// A fake Muse data root holding one session log with a single
    /// `model_completed` step, filed under the walked date levels.
    fn muse_root(session: &str, step: &str) -> PathBuf {
        let root = scratch_dir("muse-root");
        let dir = root.join("sessions/2026/09/14").join(session);
        fs::create_dir_all(&dir).expect("create session dir");
        fs::write(dir.join("session.jsonl"), format!("{step}\n")).expect("write session log");
        root
    }

    const MUSE_STEP: &str = r#"{"payload":{"event":{"kind":"model_completed","usage":{"input_tokens":21450,"output_tokens":289,"cache_write_tokens":100,"cache_read_tokens":20465,"reasoning_tokens":63}}}}"#;

    // The plausible bug: a faulted lane's tokens are dropped because only the
    // result record is read, so a dispatch killed mid-run prices at nothing.
    // A Muse transcript naming a session with a log recovers uncached input
    // (raw input minus the cached read) and reasoning-billed output.
    #[test]
    fn a_muse_transcript_recovers_its_session_logs_tokens() {
        let session = "2a2aeda2-6f38-4462-b519-2bf30e59a52e";
        let root = muse_root(session, MUSE_STEP);
        let transcript = format!(
            "{{\"stream\":{{\"kind\":\"session\",\"id\":\"{session}\"}},\"payload_type\":\"run.lifecycle.started\"}}\n"
        );

        let usage = recover_from_transcript(&transcript, Some(&root)).expect("the session log prices the run");
        assert_eq!(usage.cost.input_tokens, 985, "uncached input only: 21450 - 20465");
        assert_eq!(usage.cost.cache_read_tokens, 20465);
        assert_eq!(usage.cost.cache_write_tokens, 100);
        assert_eq!(usage.cost.output_tokens, 352, "output plus reasoning: 289 + 63");
        assert_eq!(usage.calls.map(|calls| calls.len()), Some(1));
    }

    // The plausible bug: recovery keys on the requested session rather than
    // the transcript's own id, so a lane Muse continued under a handle of its
    // own prices against a log that is not its run.
    #[test]
    fn the_transcripts_own_session_id_wins_for_the_log_lookup() {
        let session = "aaaaaaaa-bbbb-8ccc-8ddd-eeeeeeeeeeee";
        let root = muse_root(session, MUSE_STEP);
        let transcript = format!(
            "{{\"stream\":{{\"kind\":\"session\",\"id\":\"{session}\"}},\"payload_type\":\"run.lifecycle.started\"}}\n"
        );

        assert_eq!(session_id_from_transcript(&transcript).as_deref(), Some(session));
        assert!(recover_from_transcript(&transcript, Some(&root)).is_some());
        assert!(recover_from_transcript(&transcript, Some(&scratch_dir("empty-root"))).is_none());
    }

    // The plausible bug: a Claude/Grok run killed before its terminal record
    // prices at nothing, even though every assistant turn already reported its
    // tokens onto the transcript. The partial calls sum to the totals.
    #[test]
    fn a_transcript_without_a_terminal_still_yields_its_calls_summed() {
        let transcript = concat!(
            "{\"type\":\"assistant\",\"message\":{\"model\":\"claude-opus-4-8\",\"content\":[],\"usage\":{",
            "\"input_tokens\":100,\"cache_read_input_tokens\":40,\"cache_creation_input_tokens\":7,",
            "\"output_tokens\":12}}}\n",
            "{\"type\":\"assistant\",\"message\":{\"model\":\"claude-3-5-haiku\",\"content\":[],\"usage\":{",
            "\"input_tokens\":9999,\"output_tokens\":9999}}}\n",
        );

        let usage = recover_from_transcript(transcript, None).expect("the partial calls are measured");
        assert_eq!(usage.cost.input_tokens, 100, "the haiku side model is skipped");
        assert_eq!(usage.cost.cache_read_tokens, 40);
        assert_eq!(usage.cost.cache_write_tokens, 7);
        assert_eq!(usage.cost.output_tokens, 12);
        assert_eq!(usage.calls.map(|calls| calls.len()), Some(1));
    }

    // The plausible bug: a completed transcript's totals are re-summed from
    // the calls, double-billing the run when the terminal already stated them.
    #[test]
    fn a_terminal_result_states_the_totals_its_calls_itemize() {
        let transcript = concat!(
            "{\"type\":\"assistant\",\"message\":{\"model\":\"grok-4.6\",\"content\":[],\"usage\":{",
            "\"input_tokens\":13525,\"output_tokens\":33,\"cache_read_input_tokens\":256,",
            "\"cache_creation_input_tokens\":0}}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":1,\"duration_ms\":2055,",
            "\"session_id\":\"2f0c8b1e-grok\",\"usage\":{\"input_tokens\":13525,\"output_tokens\":33,",
            "\"cache_read_input_tokens\":256,\"cache_creation_input_tokens\":0}}\n",
        );

        assert_eq!(session_id_from_transcript(transcript).as_deref(), Some("2f0c8b1e-grok"));
        let usage = recover_from_transcript(transcript, None).expect("the terminal states the totals");
        assert_eq!(usage.cost.input_tokens, 13525, "stated once, not summed again");
        assert_eq!(usage.cost.output_tokens, 33);
        assert_eq!(usage.cost.turns, 1);
        assert_eq!(usage.cost.duration_millis, 2055);
    }

    // The plausible bug: a half-written last line — the ordinary tail of a
    // transcript whose lane was killed mid-write — ends the session search, so
    // the very runs this recovery exists for lose the id their earlier lines
    // already named.
    #[test]
    fn a_half_written_tail_does_not_lose_the_session_the_transcript_named() {
        let transcript = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"9c1d-truncated\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"model\":\"claude-opus-4-8\",\"content\":[],\"usage\":{",
            "\"input_tokens\":100,\"output_tokens\":12}}}\n",
            "{\"type\":\"assist",
        );

        assert_eq!(session_id_from_transcript(transcript).as_deref(), Some("9c1d-truncated"));
        let usage = recover_from_transcript(transcript, None).expect("the parsed turns are still measured");
        assert_eq!(usage.cost.input_tokens, 100);
        assert_eq!(usage.cost.output_tokens, 12);
    }

    // The plausible bug: recovery invents a zero usage for a transcript that
    // carried none, turning an unmeasured attempt into a free one on the
    // ledger. No call, no terminal: no usage.
    #[test]
    fn a_transcript_with_no_usage_is_unmeasured_not_free() {
        assert!(recover_from_transcript("not json\n", None).is_none());
        assert!(recover_from_transcript("{\"type\":\"result\",\"is_error\":true}\n", None).is_none());
    }

    // The plausible bug: the evidence directory is read for a record that was
    // never streamed, so a faulted run's transcript beside its missing
    // `evidence.json` is never opened. The directory layout is the contract.
    #[test]
    fn recovery_reads_the_transcript_beside_the_evidence() {
        let session = "2a2aeda2-6f38-4462-b519-2bf30e59a52e";
        let root = muse_root(session, MUSE_STEP);
        let evidence = scratch_dir("evidence");
        fs::write(
            evidence.join("transcript.jsonl"),
            format!(
                "{{\"stream\":{{\"kind\":\"session\",\"id\":\"{session}\"}},\"payload_type\":\"run.lifecycle.started\"}}\n"
            ),
        )
        .expect("write transcript");

        let usage =
            recover_evidence_usage(&evidence, Some(&root)).expect("the transcript beside the evidence prices the run");
        assert_eq!(usage.cost.input_tokens, 985);
        assert!(recover_evidence_usage(&scratch_dir("bare-evidence"), Some(&root)).is_none());
    }
}
