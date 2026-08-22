//! The Grok harness arm: fork Grok Build headless and derive the shared
//! result-record envelope from its Anthropic-Messages NDJSON transcript.
//!
//! Grok Build's `--output-format streaming-messages-json` is the Anthropic
//! Messages API wire format, terminal `result` record included, so this arm
//! derives its record through [`super::messages::derive_result_record`] — the same
//! path the Claude arm reads — rather than a second parse of the same shape.
//! The price the terminal reports rides that record as evidence; what a bloom
//! is charged is computed host-side from the sealed price table over the token
//! columns, never from a harness's own figure.
//!
//! Auth is ambient. The child inherits this process's environment, so a
//! host whose operator is logged in (`apiKeySource: "oauth"`) and a host
//! carrying only a `GROK_CODE_XAI_API_KEY` both resolve their own credential —
//! the lane handles no secret, exactly as the Claude arm does not.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::{env, fs};

use anyhow::Result;

use crate::transform::TransformArgs;
use crate::transform::lane::{
    capture, capture_resumed, export_build_dir, resumed_prompt, without_resume, write_prompt,
};
use crate::transform::messages::derive_result_record;
use crate::transform::peak_memory::PeakMemory;
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

/// Tool schemas a headless lane never calls, deleted from the child's
/// advertised set so they do not ride the cached prefix of every request.
///
/// Scheduler create/delete/list, `monitor`, and `workflow` are Grok's own
/// orchestration surface — a bloom lap is a single fork the bloomery owns, not
/// a recurring task, a watch, or a nested pipeline. `update_goal` and
/// `ask_user_question` assume an operator conversation this session does not
/// have. Enter/exit plan mode would re-plan a work order the lane is already
/// implementing.
///
/// `search_tool` and `use_tool` are absent on purpose: they are the MCP
/// bridge, and denying them would remove the lane's only route to
/// harness-registered tools.
const DISALLOWED_TOOLS: &[&str] = &[
    "scheduler_create",
    "scheduler_delete",
    "scheduler_list",
    "monitor",
    "workflow",
    "update_goal",
    "ask_user_question",
    "enter_plan_mode",
    "exit_plan_mode",
];

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
/// `--verbatim` bypasses the harness's large-prompt offload: without it, a
/// prompt over 25,000 bytes is replaced with an excerpt plus a notice telling
/// the model to `read_file` the staged copy, so the conversation carries both
/// and every later call of the lap replays them. Every assembled lane prompt
/// is over that line by construction. Present on cold and resumed launches
/// alike — a resumed lap still delivers a full turn through `--prompt-file`.
///
/// `--disallowed-tools` physically deletes the schemas in [`DISALLOWED_TOOLS`]
/// from the child's advertised set. A denylist is chosen over `--tools`
/// because the harness's allowlist fails open on a typo while the denylist
/// refuses nothing silently.
///
/// `--resume` continues the session a previous lap left behind, and rides
/// *alongside* `--prompt-file` rather than replacing it — the resumed run is
/// still a headless single turn, it just starts with the prior conversation in
/// context. Absent on a cold launch.
///
/// `--cwd` states the checkout this lane is working in rather than leaving it to
/// whatever directory the process was forked in. The two agree — the coordinator
/// spawns the lane with `current_dir` set to the member's checkout — and saying
/// it is what makes the working directory a property of the dispatch instead of
/// an inheritance, which is the thing a session handle is bound to.
///
/// No turn cap rides here: nothing upstream seals one, and a number invented at
/// the arm would truncate a long repair lap into a `no_result` row.
fn grok_argv(
    prompt_file: &str,
    cwd: &Path,
    model: Option<&str>,
    effort: Option<&str>,
    resume: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        "--prompt-file".to_owned(),
        prompt_file.to_owned(),
        "--cwd".to_owned(),
        cwd.to_string_lossy().into_owned(),
        "--output-format".to_owned(),
        "streaming-messages-json".to_owned(),
        "--permission-mode".to_owned(),
        "bypassPermissions".to_owned(),
        "--no-subagents".to_owned(),
        "--no-plan".to_owned(),
        "--no-memory".to_owned(),
        "--disable-web-search".to_owned(),
        "--verbatim".to_owned(),
        "--disallowed-tools".to_owned(),
        DISALLOWED_TOOLS.join(","),
    ];
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    if let Some(effort) = effort {
        argv.push("--reasoning-effort".to_owned());
        argv.push(grok_effort(effort).to_owned());
    }
    if let Some(session) = resume {
        argv.push("--resume".to_owned());
        argv.push(session.to_owned());
    }
    argv
}

/// Run a model lane under Grok and return the shared result record.
///
/// The session handle a later lap resumes needs no lifting here: Grok's terminal
/// `result` record carries its own `session_id`, and the shared
/// [`derive_result_record`] already reads it onto the envelope.
pub(super) fn run(
    prompt: &str,
    args: &TransformArgs,
    scratch: &Scratch,
    cache: Option<&CompilerCache>,
    peak: &PeakMemory,
) -> Result<serde_json::Value> {
    run_at(GROK, prompt, args, scratch, cache, peak)
}

/// [`run`] against an explicit `program` — production passes [`GROK`]; tests
/// pass a grammar-recording stand-in.
fn run_at(
    program: &str,
    prompt: &str,
    args: &TransformArgs,
    scratch: &Scratch,
    cache: Option<&CompilerCache>,
    peak: &PeakMemory,
) -> Result<serde_json::Value> {
    let Some(transcript) = launch(program, prompt, args, scratch, cache, peak)? else {
        // Grok refused the handle before starting a billed turn — the session is
        // gone, so this lap is a cold one.
        return run_at(program, prompt, &without_resume(args), scratch, cache, peak);
    };
    Ok(derive_result_record(&transcript))
}

/// Fork Grok once and hand back its transcript, or `None` when a resumed launch
/// was refused its handle before spending anything.
fn launch(
    program: &str,
    prompt: &str,
    args: &TransformArgs,
    scratch: &Scratch,
    cache: Option<&CompilerCache>,
    peak: &PeakMemory,
) -> Result<Option<String>> {
    let cwd = env::current_dir()?;
    let resume = usable_resume(&cwd, args.resume.as_deref(), grok_sessions_root().as_deref());
    let prompt_file = write_prompt(&args.out, &resumed_prompt(prompt, resume))?;
    let mut command = peak.command(program);
    command
        .args(grok_argv(&prompt_file.to_string_lossy(), &cwd, args.model.as_deref(), args.effort.as_deref(), resume))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scratch.export(&mut command);
    sccache::export(cache, &mut command);
    export_build_dir(&mut command);

    if resume.is_some() {
        return capture_resumed(command, &args.out, GROK, peak);
    }
    capture(command, &args.out, GROK, peak).map(Some)
}

/// The resume handle this launch may actually use: `session` when Grok will
/// continue it *here*, `None` when it would continue it somewhere else.
///
/// Measured, not assumed. Grok stores a session under the directory it was born
/// in (`~/.grok/sessions/<percent-encoded cwd>/<session id>`) and a resume by id
/// returns to that directory — it announces `found locally (originally in …)`
/// and works there — whatever `--cwd` says. So a resumed lap launched against a
/// different checkout does not fail: it silently edits the tree of whatever the
/// old directory now holds, leaves its own checkout clean, and is recorded as a
/// lane that produced nothing (dispatch-2374 / dispatch-2379).
///
/// With the checkout keyed to the workpiece (#5425) every lane of a member is
/// born and resumed in one directory, so this refuses nothing on the ordinary
/// path. It is the backstop for the case that made the shape visible, and it is
/// deliberately one-directional: a handle whose home cannot be established —
/// no session root, an unreadable one, a layout that is not this one — is used
/// as given, because refusing on an unrecognized layout would launch every lap
/// cold on the strength of a guess.
fn usable_resume<'a>(cwd: &Path, session: Option<&'a str>, sessions_root: Option<&Path>) -> Option<&'a str> {
    let session = session?;
    let Some(home) = sessions_root.and_then(|root| session_home(root, session)) else {
        return Some(session);
    };
    if same_directory(&home, cwd) {
        return Some(session);
    }
    eprintln!(
        "grok lane: session {session} belongs to {} and would resume there rather than in {}; launching fresh",
        home.display(),
        cwd.display(),
    );
    None
}

/// Where Grok would resume `session`: the directory its session store is keyed
/// by, decoded from the store's own layout. `None` when no directory under the
/// store holds it, which includes every case where the store is not readable in
/// the shape this knows.
fn session_home(sessions_root: &Path, session: &str) -> Option<PathBuf> {
    fs::read_dir(sessions_root)
        .ok()?
        .flatten()
        .find(|entry| entry.path().join(session).is_dir())
        .and_then(|entry| decode_session_key(entry.file_name().to_str()?))
}

/// The root Grok keys its session directories under.
///
/// `HOME` is a process-level location, not capability config: it names where
/// this host's user keeps its agent state, and no bloom seals it.
#[allow(clippy::disallowed_methods, reason = "HOME is a host location, not a capability's configuration")]
fn grok_sessions_root() -> Option<PathBuf> {
    env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".grok")))
        .map(|root| root.join("sessions"))
}

/// Decode one session-directory name back into the path it stands for. The
/// encoding is percent-escaping over the absolute path, so a name that does not
/// decode is not one of these.
fn decode_session_key(name: &str) -> Option<PathBuf> {
    let mut decoded = Vec::with_capacity(name.len());
    let mut bytes = name.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            decoded.push(byte);
            continue;
        }
        let high = char::from(bytes.next()?).to_digit(16)?;
        let low = char::from(bytes.next()?).to_digit(16)?;
        decoded.push(u8::try_from(high * 16 + low).ok()?);
    }
    let path = PathBuf::from(String::from_utf8(decoded).ok()?);
    path.is_absolute().then_some(path)
}

/// Whether two paths name the same directory, resolved through symlinks where
/// they exist. A path that cannot be canonicalized is compared as written.
fn same_directory(left: &Path, right: &Path) -> bool {
    let resolve = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolve(left) == resolve(right)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use std::path::{Path, PathBuf};
    use std::{env, process};

    use super::{DISALLOWED_TOOLS, decode_session_key, grok_argv, grok_effort, run_at, usable_resume};

    // One session store laid out the way Grok lays one out: a directory per
    // working directory, percent-encoded, holding a directory per session. Under
    // a process-and-sequence-tagged root, the sibling lanes' convention, so
    // concurrent tests never collide.
    fn sessions_root(tag: &str, entries: &[(&str, &str)]) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!("aether-grok-sessions-{tag}-{}-{seq}", process::id())).join("sessions");
        for (key, session) in entries {
            fs::create_dir_all(root.join(key).join(session)).expect("the session store fixture writes");
        }
        root
    }

    #[test]
    fn a_resume_handle_born_in_another_checkout_is_dropped() {
        // Tripwire (#5425): Grok resumes a session in the directory it was born
        // in whatever `--cwd` says — measured, not assumed: it announces `found
        // locally (originally in …)` and writes there. So a lap resumed against
        // a different checkout edits someone else's tree and leaves its own
        // clean, which reads downstream as a lane that produced nothing
        // (dispatch-2374 / dispatch-2379). Launching cold is the only honest
        // answer left.
        let root = sessions_root("born-elsewhere", &[("%2Fruns%2Fworktrees%2Fissue-5140", "sess-1")]);

        assert_eq!(
            usable_resume(Path::new("/runs/worktrees/issue-5140"), Some("sess-1"), Some(&root)),
            Some("sess-1"),
            "a session born in this checkout resumes here",
        );
        assert_eq!(
            usable_resume(Path::new("/runs/worktrees/issue-5141"), Some("sess-1"), Some(&root)),
            None,
            "a session that would resume in another member's checkout is not used",
        );
    }

    #[test]
    fn an_unrecognized_session_store_leaves_the_handle_alone() {
        // Tripwire: the guard is one-directional. A store this cannot read —
        // absent, a different layout, a session it does not hold — must leave
        // the handle as given, or an unrecognized layout would launch every lap
        // cold at full price on the strength of a guess.
        let root = sessions_root("unrecognized", &[("not-percent-encoded", "sess-1")]);

        assert_eq!(usable_resume(Path::new("/runs/a"), Some("sess-1"), Some(&root)), Some("sess-1"));
        assert_eq!(usable_resume(Path::new("/runs/a"), Some("sess-2"), Some(&root)), Some("sess-2"));
        assert_eq!(usable_resume(Path::new("/runs/a"), Some("sess-1"), None), Some("sess-1"));
    }

    #[test]
    fn a_session_directory_name_decodes_to_the_path_it_stands_for() {
        // Tripwire: the key is the whole basis for deciding where a handle
        // would resume. A decoder that dropped the escapes would compare
        // `%2Fruns%2Fa` against `/runs/a` and refuse every resume.
        assert_eq!(
            decode_session_key("%2Fruns%2Fworktrees%2Fissue-5140").as_deref(),
            Some(Path::new("/runs/worktrees/issue-5140"))
        );
        assert_eq!(decode_session_key("relative%2Fpath"), None, "a session key is an absolute path");
        assert_eq!(decode_session_key("%2"), None, "a truncated escape is not a key");
    }
    use crate::transform::TransformArgs;
    use crate::transform::construct::CONSTRUCT_IMPLEMENT;
    use crate::transform::harness_stub::{self, Stub};
    use crate::transform::lane::resumed_prompt;
    use crate::transform::messages::derive_result_record;
    use crate::transform::peak_memory;
    use crate::transform::scratch::Scratch;

    #[test]
    fn argv_runs_headless_and_carries_the_resolved_profile() {
        let argv =
            grok_argv("/run/prompt.md", Path::new("/runs/worktrees/issue-5140"), Some("grok-4.6"), Some("high"), None);
        // Tripwire (#5425): the lane states the checkout it works in. Without
        // it the working directory is whatever the fork inherited, which is the
        // one property a resumed session is bound to.
        assert!(
            argv.windows(2).any(|w| w == ["--cwd", "/runs/worktrees/issue-5140"]),
            "the lane names the checkout it is working in",
        );
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
        // Tripwire: an argv without `--verbatim` is delivered as an excerpt plus a self-read.
        assert!(argv.contains(&"--verbatim".to_owned()));
        assert!(
            argv.windows(2).any(|w| w == ["--disallowed-tools", DISALLOWED_TOOLS.join(",").as_str()]),
            "the denylist deletes the unused schemas from the advertised set",
        );
        assert!(argv.windows(2).any(|w| w == ["--model", "grok-4.6"]), "the resolved model rides argv");
        assert!(argv.windows(2).any(|w| w == ["--reasoning-effort", "high"]), "the resolved effort rides argv");
        assert!(!argv.iter().any(|flag| flag == "--resume"), "a cold launch names no session");
    }

    #[test]
    fn argv_omits_the_profile_flags_when_none_falls_back_to_ambient() {
        let argv = grok_argv("/run/prompt.md", Path::new("/runs/worktrees/issue-5140"), None, None, None);
        assert!(!argv.iter().any(|flag| flag == "--model"), "no resolved model means the operator's default");
        assert!(!argv.iter().any(|flag| flag == "--reasoning-effort"), "no resolved effort means the same");
        assert!(argv.windows(2).any(|w| w == ["--prompt-file", "/run/prompt.md"]), "still headless");
    }

    // Tripwire: a resumed Grok lap keeps `--prompt-file`. `--resume` names the
    // conversation to continue, not the turn to run, so an argv that swapped one
    // for the other would resume the session and then hand it nothing to do —
    // and the lane would read the empty lap as a member that produced no
    // candidate rather than as a broken invocation.
    #[test]
    fn argv_threads_the_resume_handle_alongside_the_prompt_file() {
        let argv = grok_argv(
            "/run/prompt.md",
            Path::new("/runs/worktrees/issue-5140"),
            Some("grok-4.6"),
            Some("high"),
            Some("sess-1"),
        );
        assert!(argv.windows(2).any(|w| w == ["--resume", "sess-1"]), "the handle follows its flag");
        assert!(argv.windows(2).any(|w| w == ["--prompt-file", "/run/prompt.md"]), "a resumed lap still has a turn");
        // Tripwire: an argv without `--verbatim` is delivered as an excerpt plus a self-read.
        assert!(argv.contains(&"--verbatim".to_owned()));
    }

    // Tripwire: `search_tool` and `use_tool` are the MCP bridge; denying them
    // removes the lane's only route to harness-registered tools.
    #[test]
    fn disallowed_tools_does_not_include_the_mcp_bridge() {
        assert!(
            DISALLOWED_TOOLS.iter().all(|tool| *tool != "search_tool" && *tool != "use_tool"),
            "the MCP bridge stays on the advertised set",
        );
    }

    // Tripwire: the handle the pool deposits comes off Grok's own terminal
    // record through the shared derivation. A `session_id` that failed to reach
    // the envelope leaves the pool with nothing to key a warm lap on, so every
    // retry relaunches cold at full price while still reporting success.
    #[test]
    fn the_terminals_session_id_reaches_the_envelope_the_pool_deposits() {
        let transcript = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"session_id":"2f0c8b1e-grok","usage":{"input_tokens":13525,"output_tokens":33}}"#,
            "\n",
        );
        assert_eq!(derive_result_record(transcript)["session_id"], "2f0c8b1e-grok");
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
        assert_eq!(record["cost_usd"], 0.027_376);
        assert_eq!(record["first_call_model"], "grok-4.6", "no model here is filtered as a side model");
    }

    fn drive(stub: &Stub, args: &TransformArgs, prompt: &str) -> anyhow::Result<serde_json::Value> {
        let scratch = Scratch::prepare(&args.out, args.nonce.as_deref()).expect("scratch");
        run_at(stub.program(), prompt, args, &scratch, None, &peak_memory::detect())
    }

    // Tripwire: `--prompt-file` is the prompt channel. An argv test that only
    // checks the flag names a path still passes when `write_prompt` is handed
    // an empty string, and every Grok lane then runs an empty turn.
    #[test]
    fn a_cold_launch_puts_the_prompt_in_the_file_not_on_argv_or_stdin() {
        let stub = Stub::succeed();
        let args = harness_stub::args(CONSTRUCT_IMPLEMENT, stub.out());
        let prompt = "assembled grok prompt";
        let record = drive(&stub, &args, prompt).expect("cold grok");

        let launches = stub.launches();
        assert_eq!(launches.len(), 1, "a cold launch forks once");
        let path = launches[0].flag("--prompt-file").expect("headless grok names a prompt file");
        assert_eq!(fs::read_to_string(path).expect("read prompt file"), prompt);
        assert!(
            launches[0].argv.iter().all(|arg| !arg.contains(prompt)),
            "the assembled prompt must not leak onto argv"
        );
        assert!(launches[0].stdin.is_empty(), "grok reads the prompt from the file, with stdin closed");
        assert!(!launches[0].has("--resume"), "a cold launch names no session");
        assert_eq!(record["session_id"], "stub-session-1");
    }

    // Tripwire: `--resume` continues the conversation; it does not replace the
    // turn. An argv that dropped `--prompt-file` on a warm lap would resume and
    // then hand the child nothing to do.
    #[test]
    fn a_resumed_launch_keeps_the_prompt_file_alongside_the_handle() {
        let stub = Stub::succeed();
        let mut args = harness_stub::args(CONSTRUCT_IMPLEMENT, stub.out());
        args.resume = Some("sess-1".to_owned());
        let prompt = "assembled grok prompt";
        drive(&stub, &args, prompt).expect("resumed grok");

        let launches = stub.launches();
        assert_eq!(launches.len(), 1, "a live handle forks once");
        assert_eq!(launches[0].flag("--resume"), Some("sess-1"));
        let path = launches[0].flag("--prompt-file").expect("a resumed lap still has a turn");
        assert_eq!(fs::read_to_string(path).expect("read prompt file"), resumed_prompt(prompt, Some("sess-1")));
        assert!(launches[0].stdin.is_empty(), "the prompt still rides the file");
    }

    #[test]
    fn a_refused_handle_relaunches_once_without_resume() {
        let stub = Stub::first_launch_refuses();
        let mut args = harness_stub::args(CONSTRUCT_IMPLEMENT, stub.out());
        args.resume = Some("sess-1".to_owned());
        let record = drive(&stub, &args, "assembled grok prompt").expect("refused resume falls back cold");

        let launches = stub.launches();
        assert_eq!(launches.len(), 2, "the refused handle is one extra fork, not a loop");
        assert_eq!(launches[0].flag("--resume"), Some("sess-1"));
        assert!(launches[0].has("--prompt-file"), "the refused lap still named a turn");
        assert!(!launches[1].has("--resume"), "the fallback is a cold launch");
        assert!(launches[1].has("--prompt-file"));
        assert_eq!(record["session_id"], "stub-session-2");
    }
}
