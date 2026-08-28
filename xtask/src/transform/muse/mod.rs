//! The Muse harness arm: fork `muse exec` headless and derive the shared
//! result-record envelope from its JSONL transcript.

mod usage;

use std::process::{self, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use aether_data::Uuid;
use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::transform::TransformArgs;
use crate::transform::lane::{Resumed, Terminal, capture, record, resumed_prompt, write_prompt};
use crate::transform::peak_memory::PeakMemory;
use crate::transform::sccache::{self, CompilerCache};
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
///
/// `--session-id` is unconditional, and that is the arm's one inversion: Muse
/// addresses a *new* and a *continued* session through the same flag, so the
/// handle has to exist before the first lap rather than being read back from
/// it. A cold lap is launched under a minted uuid and a warm one under the
/// handle the pool held. (`muse resume` is the TUI's entry point and cannot run
/// headless, so it is not the path here.)
fn muse_argv(prompt_file: &str, model: Option<&str>, effort: Option<&str>, session: &str) -> Vec<String> {
    let mut argv =
        vec!["exec".to_owned(), "--json".to_owned(), "--disable-approval".to_owned(), "--prompt-file".to_owned()];
    argv.push(prompt_file.to_owned());
    argv.push("--session-id".to_owned());
    argv.push(session.to_owned());
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

/// Mint the session uuid a cold Muse lap runs under.
///
/// Because Muse names both a new and a continued session with the same
/// `--session-id`, the handle a later lap resumes has to be chosen by the caller
/// before the first lap starts — there is no id to read back and no second lap
/// without one. Derived from the dispatch's idempotency `nonce` so the session
/// is a function of the run that owns it: one dispatch, one session, nameable
/// from the work order without opening a transcript. A hand-run lane carries no
/// nonce, so it falls back to this process and the wall clock — unique per run
/// without pretending to be reproducible.
fn mint_session_id(nonce: Option<&str>) -> String {
    let seed = nonce.map_or_else(
        || {
            let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
            format!("{}-{}", process::id(), since_epoch.as_nanos())
        },
        str::to_owned,
    );
    uuid_from_seed(&seed)
}

/// A uuid derived from `seed`: the sha256 digest's first sixteen bytes, stamped
/// with RFC-4122's version-8 (custom) and variant nibbles.
///
/// Version 8 is the honest one — the bytes are a digest, not the randomness a
/// v4 claims — and Muse validates the shape rather than the version, so the id
/// parses as the uuid the flag demands.
fn uuid_from_seed(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
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
///
/// A rejected handle fails the lane rather than relaunching cold, unlike the
/// sibling arms: Muse's refusal shape is unprobed, and a guess at it would
/// either miss (leaving the lane dead anyway) or over-match an auth failure or a
/// crash after tokens into a second full-price run. A legible failure is the
/// conservative reading until the shape is known.
pub(super) fn run(
    prompt: &str,
    args: &TransformArgs,
    resumed: Resumed,
    scratch: &Scratch,
    cache: Option<&CompilerCache>,
    peak: &PeakMemory,
) -> Result<serde_json::Value> {
    run_at(MUSE, prompt, args, resumed, scratch, cache, peak)
}

/// [`run`] against an explicit `program` — production passes [`MUSE`]; tests
/// pass a grammar-recording stand-in.
fn run_at(
    program: &str,
    prompt: &str,
    args: &TransformArgs,
    resumed: Resumed,
    scratch: &Scratch,
    cache: Option<&CompilerCache>,
    peak: &PeakMemory,
) -> Result<serde_json::Value> {
    let session = args.resume.clone().unwrap_or_else(|| mint_session_id(args.nonce.as_deref()));
    let prompt_file = write_prompt(&args.out, &resumed_prompt(prompt, args.resume.as_deref(), resumed))?;
    let mut command = peak.command(program);
    command
        .args(muse_argv(&prompt_file.to_string_lossy(), args.model.as_deref(), args.effort.as_deref(), &session))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scratch.export(&mut command);
    sccache::export(cache, &mut command);

    let transcript = capture(command, &args.out, MUSE, peak)?;
    // The transcript's own id wins over the one that was asked for: if Muse ever
    // declined the requested session and opened its own, that is the id the log
    // is filed under and the id a later lap has to resume.
    let session = usage::session_id(&transcript).unwrap_or(session);
    Ok(record(
        derive_terminal(&transcript).map(|terminal| Terminal { usage: usage::from_session_log(&session), ..terminal }),
        Some(session),
    ))
}

#[cfg(test)]
mod tests {
    use super::{derive_terminal, mint_session_id, muse_argv, muse_effort, run_at, uuid_from_seed};
    use crate::transform::TransformArgs;
    use crate::transform::construct::CONSTRUCT_IMPLEMENT;
    use crate::transform::harness_stub::{self, Stub};
    use crate::transform::lane::Resumed;
    use crate::transform::peak_memory;
    use crate::transform::scratch::Scratch;

    #[test]
    fn argv_runs_headless_and_carries_the_resolved_profile() {
        let argv = muse_argv("/out/prompt.md", Some("muse-spark-1.2-contributor"), Some("high"), "sess-uuid");
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
        let bare = muse_argv("/out/prompt.md", None, None, "sess-uuid");
        assert!(!bare.iter().any(|a| a == "--model"));
        assert!(!bare.iter().any(|a| a == "--reasoning-effort"));
    }

    // Tripwire: the arm's inversion. Muse opens and continues a session through
    // the same `--session-id`, so a cold lap that omitted the flag would let
    // Muse mint an id the pool never sees — and every later lap would relaunch
    // cold with nothing to resume, which is the exact spend the pool exists to
    // cut. Both laps name the handle; only the source of it differs.
    #[test]
    fn both_a_cold_and_a_resumed_lap_name_the_session() {
        let cold = muse_argv("/out/prompt.md", None, None, &mint_session_id(Some("nonce-1")));
        let at = cold.iter().position(|a| a == "--session-id").expect("a cold lap names its session too");
        assert_eq!(cold[at + 1], uuid_from_seed("nonce-1"), "the minted handle is the dispatch's own");

        let resumed = muse_argv("/out/prompt.md", None, None, "2a2aeda2-6f38-4462-b519-2bf30e59a52e");
        let at = resumed.iter().position(|a| a == "--session-id").expect("a resumed lap names its session");
        assert_eq!(resumed[at + 1], "2a2aeda2-6f38-4462-b519-2bf30e59a52e", "the pool's handle rides unchanged");
    }

    // Tripwire: the minted handle is a well-formed uuid, computed from the
    // nonce. Muse refuses `--session-id` that does not parse as one, so a
    // digest handed over as raw hex would kill the lane at fork; and an id that
    // did not vary with the nonce would collide two concurrent dispatches onto
    // one session.
    #[test]
    fn the_minted_handle_is_a_uuid_derived_from_the_dispatchs_nonce() {
        let minted = uuid_from_seed("nonce-1");
        assert_eq!(minted, mint_session_id(Some("nonce-1")), "the nonce is the seed");
        assert_eq!(minted, uuid_from_seed("nonce-1"), "and the derivation is a function of it");
        assert_ne!(minted, uuid_from_seed("nonce-2"), "a second dispatch gets a second session");

        let fields: Vec<&str> = minted.split('-').collect();
        assert_eq!(fields.iter().map(|field| field.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12], "8-4-4-4-12");
        assert!(minted.chars().all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "lowercase hex");
        assert_eq!(fields[2].as_bytes()[0], b'8', "version 8: derived bytes, not a claimed v4");
        assert!(matches!(fields[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'), "the RFC-4122 variant nibble");
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

    fn drive(stub: &Stub, args: &TransformArgs, prompt: &str) -> anyhow::Result<serde_json::Value> {
        let scratch = Scratch::prepare(&args.out, args.nonce.as_deref()).expect("scratch");
        run_at(stub.program(), prompt, args, Resumed::AfterReset, &scratch, None, &peak_memory::detect())
    }

    // Tripwire: Muse names a new session and a continued one with the same
    // `--session-id`, so a cold lap that omitted the flag would let Muse mint
    // an id the pool never sees. The transcript's own id wins over the one
    // that was asked for, which is what a later lap has to resume.
    #[test]
    fn a_cold_launch_names_a_uuid_session_and_records_the_transcripts_id() {
        let stub = Stub::succeed();
        let args = harness_stub::args(CONSTRUCT_IMPLEMENT, stub.out());
        let record = drive(&stub, &args, "assembled muse prompt").expect("cold muse");

        let launches = stub.launches();
        assert_eq!(launches.len(), 1, "a cold launch forks once");
        assert_eq!(launches[0].argv.first().map(String::as_str), Some("exec"));
        assert!(launches[0].has("--json"), "the transcript is what the record derives from");
        assert!(launches[0].has("--disable-approval"), "headless needs the approval prompt gone");
        let requested = launches[0].flag("--session-id").expect("a cold lap names its session too");
        let fields: Vec<&str> = requested.split('-').collect();
        assert_eq!(fields.iter().map(|field| field.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert_eq!(
            record["session_id"], "aaaaaaaa-bbbb-8ccc-8ddd-eeeeeeeeeeee",
            "the transcript's stream id wins over the requested handle"
        );
        assert_ne!(record["session_id"], requested, "a declined request must not be what the pool deposits");
    }

    // Tripwire: Muse does not relaunch cold on a rejected handle. A stub that
    // exits nonzero after emitting output must fail the lane after one fork,
    // not pay for a second full-price run.
    #[test]
    fn a_nonzero_exit_fails_the_lane_after_exactly_one_launch() {
        let stub = Stub::fail_after_output();
        let args = harness_stub::args(CONSTRUCT_IMPLEMENT, stub.out());
        drive(&stub, &args, "assembled muse prompt").expect_err("a nonzero muse exit fails the lane");
        assert_eq!(stub.launches().len(), 1, "muse does not fall back cold");
    }
}
