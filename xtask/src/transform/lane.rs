//! What every model-lane harness arm shares: the spawn-and-capture, and the
//! result-record envelope the lanes read.
//!
//! The envelope is the load-bearing contract. Two consumers pin its shape and
//! neither knows which harness produced it:
//!
//! - the construct lane's completion gate reads `result_record.is_error`
//!   (`LocalExecutor::stream_evidence`, #3596),
//! - the review lane reads `result.result` for the critic's `VERDICT:` line
//!   on harnesses without tool injection; the Claude path reads the findings
//!   file the reviewer appended to instead.
//!
//! An arm that returned a differently-shaped record would fail the review gate
//! closed, which reads as a critic *finding* rather than a harness bug — so the
//! arms converge here rather than each assembling their own.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};
use std::{env, fs, io};

use anyhow::{Context, Result, bail};

use crate::transform::TransformArgs;
use crate::transform::peak_memory::PeakMemory;

/// What a harness reported when its run ended.
pub(super) struct Terminal {
    /// Whether the run ended in error. The construct gate demands `false`.
    pub is_error: bool,
    /// The run's final message text — what the non-Claude review path
    /// parses its `VERDICT:` line out of.
    pub text: String,
    /// The token counts the harness reported, or `None` when it reports none.
    /// `None` renders the token columns null rather than zero, so a study reads
    /// "unmeasured" instead of "free".
    pub usage: Option<Usage>,
}

/// The token counts a harness reported for a run: the billed totals, and the
/// per-call breakdown they were summed from.
pub(super) struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    /// One entry per model call, in the order the harness made them. Empty when
    /// the harness reports only aggregates, which renders the `calls` column
    /// null rather than as an empty run.
    pub calls: Vec<Call>,
}

/// One model call's token counts.
///
/// Two host-side readers need the calls and not the totals. The session pool
/// takes the *last* call's prompt — uncached input plus both cache classes — as
/// the context a resume would re-read (`session_reuse::parse_context_tokens`),
/// and refuses to deposit a lap that reports none; and the sealed price table
/// selects its long-context band per call, because the threshold is a
/// prompt-size cut and an aggregate sum crosses it for reasons no single call
/// did (`aether_bloomery::PriceTable::price_dispatch`).
pub(super) struct Call {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

/// Assemble the result-record envelope from a harness's `terminal`, or the
/// `no_result` record when the run died before reporting one.
///
/// `session` is the handle a later lap resumes this run's conversation with —
/// the muse arm's session uuid, the grok arm's session id — under the
/// `session_id` key the Anthropic-Messages derivation already writes, because
/// the session pool reads one key whichever arm produced the record.
///
/// `cost_usd` is always null: neither non-Claude harness reports a price, and
/// zero would read as free rather than unmeasured.
pub(super) fn record(terminal: Option<Terminal>, session: Option<String>) -> serde_json::Value {
    use serde_json::{Map, Value, json};

    let mut record = Map::new();
    record.insert("schema".to_owned(), json!(1));
    // Ahead of the `no_result` return below: a run that died mid-lap still has a
    // session to resume, and a retry that had to relaunch cold because the
    // handle was dropped here is exactly the spend the pool exists to avoid.
    record.insert("session_id".to_owned(), session.map_or(Value::Null, Value::String));
    // The ledger columns a non-Claude harness cannot fill. Present and null so a
    // downstream reader sees the same key set whichever arm ran.
    for field in [
        "task",
        "ref",
        "run_id",
        "conclusion",
        "model",
        "created_at",
        "pool",
        "first_call_model",
        "first_call_cache_read",
        "first_call_cache_write",
        "first_call_input",
        // Overwritten below when the harness reported per-call usage; null here
        // so the `no_result` record keeps the same key set.
        "calls",
    ] {
        record.insert(field.to_owned(), Value::Null);
    }

    let Some(terminal) = terminal else {
        // A run that died before reporting a terminal — legible, cost unknown.
        // Deliberately carries no `is_error`, so the construct gate's
        // `== Some(false)` test fails closed exactly as it does for Claude.
        record.insert("no_result".to_owned(), json!(true));
        return Value::Object(record);
    };

    record.insert("num_turns".to_owned(), Value::Null);
    record.insert("cost_usd".to_owned(), Value::Null);
    record.insert("duration_ms".to_owned(), Value::Null);
    record.insert("is_error".to_owned(), json!(terminal.is_error));
    let (input, cache_read, cache_write, output, calls) =
        terminal.usage.map_or((Value::Null, Value::Null, Value::Null, Value::Null, Value::Null), |usage| {
            (
                json!(usage.input),
                json!(usage.cache_read),
                json!(usage.cache_write),
                json!(usage.output),
                call_column(&usage.calls),
            )
        });
    record.insert("input".to_owned(), input);
    record.insert("cache_read".to_owned(), cache_read);
    record.insert("cache_write".to_owned(), cache_write);
    record.insert("cache_write_1h".to_owned(), Value::Null);
    record.insert("cache_write_5m".to_owned(), Value::Null);
    record.insert("output".to_owned(), output);
    record.insert("calls".to_owned(), calls);
    // The nested terminal the review lane reads its verdict text out of, shaped
    // like the Claude arm's carried-whole `result` event.
    record.insert("result".to_owned(), json!({ "is_error": terminal.is_error, "result": terminal.text }));
    Value::Object(record)
}

/// The `calls` column for `calls`, or null when the harness reported none.
///
/// Null rather than `[]`, and the two are read differently: the pool's
/// `parse_context_tokens` takes the last entry of a non-empty array and answers
/// `None` for an absent or empty one, which is the "unmeasured lap" branch that
/// skips the session deposit entirely. An empty array would claim a run that
/// made no model call.
///
/// The key names are the ones `aether_bloomery_github::parse_study` decodes
/// (`CallJson`), which is the same shape the Anthropic-Messages arms write. The
/// two cache-write TTL splits are absent, not zeroed: no harness here reports
/// them, the aggregate columns beside these are null for the same reason, and
/// the decoder defaults an absent one to zero anyway.
fn call_column(calls: &[Call]) -> serde_json::Value {
    use serde_json::{Value, json};

    if calls.is_empty() {
        return Value::Null;
    }
    Value::Array(
        calls
            .iter()
            .map(|call| {
                json!({
                    "input": call.input,
                    "cache_read": call.cache_read,
                    "cache_write": call.cache_write,
                    "output": call.output,
                })
            })
            .collect(),
    )
}

/// Write the assembled `prompt` to `<out>/prompt.md` and return its path — both
/// non-Claude arms hand their child a prompt file rather than piping stdin, so
/// neither repeats the pipe-on-a-thread dance the Claude arm needs, and the
/// exact prompt a run received stays on disk beside its transcript.
/// The cargo build directory a model lane's child must build into: the slot
/// target the coordinator lent this dispatch, stated on the child's environment
/// rather than left to inheritance (#5425).
///
/// The lane is handed the slot's target as `CARGO_TARGET_DIR` and the child has
/// to build into that one. Anything else is a cold build: `sccache` keys a
/// compilation partly on the paths cargo names on the `rustc` invocation, so a
/// per-run target directory misses on the whole dependency tree — measured at
/// 600–1500 misses per lap, most of the in-lap wall clock, and the disk that
/// filled on 2026-08-21. `AETHER_LANE_SCRATCH` remains what it was for
/// everything that is not a cargo build.
///
/// `None` when the lane itself has none, which is a developer running the arm by
/// hand: cargo's own default is the honest answer there, not a directory this
/// invented.
pub(super) fn build_dir(inherited: Option<OsString>) -> Option<OsString> {
    inherited.filter(|dir| !dir.is_empty())
}

/// Point `command`'s child at the lane's own build directory.
///
/// `CARGO_TARGET_DIR` is a host-supplied location rather than a capability's
/// configuration: the coordinator hands it to this process, and this passes the
/// same value on.
#[allow(clippy::disallowed_methods)] // the lane's own build directory is handed down by the host, not cap config.
pub(super) fn export_build_dir(command: &mut Command) {
    if let Some(dir) = build_dir(env::var_os("CARGO_TARGET_DIR")) {
        command.env("CARGO_TARGET_DIR", dir);
    }
}

pub(super) fn write_prompt(out: &Path, prompt: &str) -> Result<PathBuf> {
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let path = out.join("prompt.md");
    fs::write(&path, prompt).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Recognizes a harness's own end-of-run record on one transcript line.
///
/// An arm that names one is saying its CLI does not necessarily exit when the
/// turn the lane asked for is over — see [`execute_watched`]. An arm whose CLI
/// does exit there passes `None` and waits, which is the cheaper thing to do
/// and the historical behaviour.
pub(super) type Terminated = fn(&str) -> bool;

/// Run `command` to completion, capture its stdout to `<out>/transcript.jsonl`,
/// and return the captured text.
///
/// A non-zero exit is the CLI itself failing to run (auth, bad args, crash) — an
/// operational failure, distinct from a task-level error, which a completed run
/// records inside its transcript. It is surfaced rather than folded into an
/// empty record reported as success. The one exception is a run the lane itself
/// ended at the harness's own terminal record: the harness already answered, so
/// the exit status is this process's doing and says nothing about the run.
pub(super) fn capture(
    command: Command,
    out: &Path,
    harness: &str,
    peak: &PeakMemory,
    terminated: Option<Terminated>,
) -> Result<String> {
    let run = execute_watched(command, out, harness, peak, None, terminated)?;
    if !run.ended_at_terminal {
        exit_check(&run.output, harness)?;
    }
    Ok(String::from_utf8_lossy(&run.output.stdout).into_owned())
}

/// [`capture`] for a run whose argv carried a resume handle.
///
/// `Ok(None)` is its one extra outcome: the CLI refused the handle before
/// starting a billed turn, which the arm answers by relaunching cold. Every
/// other non-zero exit still fails the lane, because a crash that had already
/// spent tokens must not be paid for twice.
pub(super) fn capture_resumed(
    command: Command,
    out: &Path,
    harness: &str,
    peak: &PeakMemory,
) -> Result<Option<String>> {
    let run = execute(command, out, harness, peak, None)?;
    if resume_handle_rejected(run.status, &run.stdout, &run.stderr) {
        return Ok(None);
    }
    exit_check(&run, harness)?;
    Ok(Some(String::from_utf8_lossy(&run.stdout).into_owned()))
}

/// A finished harness run.
pub(super) struct Run {
    /// What the process produced and how it exited.
    output: Output,
    /// Whether the lane ended the run itself, because the harness reported its
    /// terminal and then kept running. When it did, the exit status is this
    /// process's own signal and says nothing about what the run produced.
    ended_at_terminal: bool,
}

/// How often [`settle`] re-asks whether the child has exited. Small enough that
/// ending a run is prompt, large enough that a twenty-minute lap costs nothing
/// to watch.
const SETTLE_POLL: Duration = Duration::from_millis(25);

/// How long a harness that has reported its terminal gets to exit on its own
/// before the lane ends the run for it.
///
/// A healthy CLI exits within a line or two of its terminal record — Muse emits
/// one more `session.workspace_branch.observed` and goes — so this window is
/// what keeps a normal run judged by its own exit status, including a CLI that
/// answers and *then* fails.
const TERMINAL_LINGER: Duration = Duration::from_secs(5);

/// How long the harness tree gets to fold after the lane signals it, before the
/// lane stops asking politely.
///
/// Long enough for a CLI to flush the session log its token counts are read back
/// from, short enough that no second model turn can conclude inside it and land
/// a terminal record the arm would then read as this run's answer.
const TERMINAL_GRACE: Duration = Duration::from_secs(5);

/// Spawn `command`, tee stdout into `<out>/transcript.jsonl` as it arrives,
/// drain stderr concurrently, and wait. The same primitive every harness arm
/// uses, including Claude's piped-stdin launch.
///
/// The transcript is created (truncated) before the child can emit, so a
/// heartbeat exists for the whole run. Each flushed chunk advances the file
/// modification time; there is no timer and no per-event `fsync`.
///
/// The child is always reaped before any pipe-thread error is returned. A
/// nonzero exit takes precedence over a broken stdin pipe — the child closed
/// its end because it died, and naming the broken pipe would hide the cause.
/// `peak` is read after the pipes drain for the same reason as before: a run
/// that died still peaked at something (#4912).
pub(super) fn execute(
    command: Command,
    out: &Path,
    harness: &str,
    peak: &PeakMemory,
    stdin: Option<Vec<u8>>,
) -> Result<Output> {
    Ok(execute_watched(command, out, harness, peak, stdin, None)?.output)
}

/// [`execute`] for a harness whose CLI may outlive the turn the lane asked for.
///
/// `terminated` recognizes the harness's own end-of-run record on a transcript
/// line. When one arrives, the lane stops waiting, signals the child's process
/// group, and reports `ended_at_terminal` — because the harness has already
/// said what the run produced, and everything after it is a process the lane
/// never commissioned.
///
/// Muse is why this exists (bloom `b7f0e4568d4a`, dispatch-4202). Its runtime
/// carries a background-terminal client that submits a *fresh turn* to the same
/// session when a backgrounded shell command finishes: the transcript reaches
/// `run.terminal.completed` for the lane's turn, and two records later a
/// `runtime.command.accepted` from `muse-runtime-background-terminal` opens
/// another run that no lane asked for and no lane can end. `muse exec` then does
/// not exit, and the lane waited 49 minutes on a turn that had answered in 28 —
/// with no bound of its own short of the coordinator's construct deadline.
///
/// The signal goes to the child's **process group**, not the handle this holds.
/// `PeakMemory::command` wraps the harness in `/usr/bin/time -v`, so the handle
/// names the wrapper, and the CLI — with every shell, sandbox and build under it
/// — sits below that. Killing the handle alone orphans the harness, still
/// running and still billing.
fn execute_watched(
    mut command: Command,
    out: &Path,
    harness: &str,
    peak: &PeakMemory,
    stdin: Option<Vec<u8>>,
    terminated: Option<Terminated>,
) -> Result<Run> {
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let path = out.join("transcript.jsonl");
    let file = File::create(&path).with_context(|| format!("write {}", path.display()))?;

    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    // One process group for the whole harness tree, so ending a run reaches the
    // CLI and everything it forked rather than only the wrapper.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = ChildReaper::new(command.spawn().map_err(|error| spawn_context(error, harness))?);
    let group = child.child()?.id();
    let writer_work = match stdin {
        Some(bytes) => {
            let pipe = child.child()?.stdin.take().with_context(|| format!("{harness} stdin was not captured"))?;
            Some((pipe, bytes))
        }
        None => None,
    };
    let stdout = child.child()?.stdout.take().with_context(|| format!("{harness} stdout was not captured"))?;
    let stderr = child.child()?.stderr.take().with_context(|| format!("{harness} stderr was not captured"))?;
    let (announce, terminal_seen) = mpsc::channel();

    // `thread::scope`, not `thread::spawn`: raw spawn is disallowed (settlement/trace
    // umbrella); this is infra below the actor/mail layer.
    let (status, stdout, stderr, written, ended_at_terminal) = thread::scope(|scope| {
        let writer = writer_work.map(|(mut pipe, bytes)| scope.spawn(move || pipe.write_all(&bytes)));
        let stdout_reader = scope.spawn(move || tee_stdout(stdout, file, terminated, &announce));
        let stderr_reader = scope.spawn(|| drain_stderr(stderr));

        let ended_at_terminal =
            settle(&mut child, &terminal_seen, group).with_context(|| format!("watch {harness}"))?;
        let status = child.wait().with_context(|| format!("await {harness}"))?;
        let stdout = stdout_reader.join().expect("stdout reader panicked");
        let stderr = stderr_reader.join().expect("stderr reader panicked");
        let written = writer.map(|handle| handle.join().expect("prompt-writer thread panicked"));
        Ok::<_, anyhow::Error>((status, stdout, stderr, written, ended_at_terminal))
    })?;

    let stderr = stderr.with_context(|| format!("read {harness} stderr"))?;
    peak.observe(&stderr);
    let stdout = stdout.with_context(|| format!("write {}", path.display()))?;

    if !status.success() {
        return Ok(Run { output: Output { status, stdout, stderr }, ended_at_terminal });
    }
    if let Some(written) = written {
        written.with_context(|| format!("pipe the assembled prompt to {harness}"))?;
    }
    Ok(Run { output: Output { status, stdout, stderr }, ended_at_terminal })
}

/// Wait for the child to exit, ending the run for it only if it reported its
/// terminal and then stayed up. `true` when the lane ended it.
///
/// The terminal is not itself the stopping point. A CLI that answers and exits —
/// including one that answers and then fails — must still be judged by its own
/// status, so the terminal only starts a [`TERMINAL_LINGER`] clock, and a
/// harness still alive when it runs out is one that is no longer working on this
/// lane's turn.
///
/// Polling rather than a second wait thread: the handle is `&mut` and the reader
/// that recognizes a terminal is a sibling thread, so the two meet on a channel
/// and this side has to be able to look at both. A disconnected channel is the
/// reader finishing with no terminal named — the child closed stdout, so its
/// exit is the next thing to happen and blocking on it is right.
fn settle(child: &mut ChildReaper, terminal_seen: &Receiver<()>, group: u32) -> io::Result<bool> {
    loop {
        if child.try_wait()?.is_some() {
            return Ok(false);
        }
        match terminal_seen.recv_timeout(SETTLE_POLL) {
            Ok(()) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(false),
        }
    }

    let deadline = Instant::now() + TERMINAL_LINGER;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(false);
        }
        thread::sleep(SETTLE_POLL);
    }
    end_run(child, group);
    Ok(true)
}

/// End the harness tree: ask it to fold, give it [`TERMINAL_GRACE`] to do so,
/// then stop asking.
///
/// The polite signal first, because a harness flushes the session log its token
/// counts are read back from as it goes down, and a run whose cost cannot be
/// read is one the ledger records as unmeasured.
fn end_run(child: &mut ChildReaper, group: u32) {
    signal_group(group, "TERM");
    let deadline = Instant::now() + TERMINAL_GRACE;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(SETTLE_POLL);
    }
    signal_group(group, "KILL");
    if let Ok(child) = child.child() {
        let _ = child.kill();
    }
}

/// Signal every process in `group`.
///
/// Through `kill(1)` rather than a syscall crate: the lane already shells out to
/// `git`, `/usr/bin/time` and `sccache`, and one more host binary is cheaper
/// than a dependency for a single call. Linux wants the negative group as an
/// operand after `--`; macOS takes it as the last argument.
#[cfg(unix)]
fn signal_group(group: u32, signal: &str) {
    let Ok(group) = i32::try_from(group) else {
        return;
    };
    let mut kill = Command::new("kill");
    kill.args(["-s", signal]);
    if cfg!(target_os = "linux") {
        kill.arg("--");
    }
    let _ = kill.arg(format!("-{group}")).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// A host with no process groups signals the handle alone — [`end_run`] does
/// that already, so this has nothing left to do.
#[cfg(not(unix))]
fn signal_group(_group: u32, _signal: &str) {}

/// Reap `child` on every path, including early returns before `wait`. `Child`'s
/// `Drop` neither waits nor kills, so a leaked handle is a zombie or a still-billing
/// process.
struct ChildReaper {
    child: Option<Child>,
}

impl ChildReaper {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child(&mut self) -> Result<&mut Child> {
        self.child.as_mut().context("child already reaped")
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.as_mut().map_or(Ok(None), Child::try_wait)
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.take().expect("child already reaped").wait()
    }
}

impl Drop for ChildReaper {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // The group, not the handle: an early return leaves the wrapper's
            // whole harness tree behind otherwise.
            signal_group(child.id(), "KILL");
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Copy the child's stdout into `file` and into the returned buffer as it
/// arrives, announcing on `terminal_seen` the first time a complete line
/// satisfies `terminated`.
///
/// Reading continues past the announcement. The lane's answer is the whole
/// transcript, and whatever the harness emits while it folds belongs in the
/// file the arm derives that answer from.
fn tee_stdout(
    mut reader: impl Read,
    mut file: File,
    terminated: Option<Terminated>,
    terminal_seen: &Sender<()>,
) -> io::Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buf = [0u8; 8192];
    let mut scanned = 0;
    let mut announced = false;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        captured.extend_from_slice(&buf[..n]);
        file.write_all(&buf[..n])?;
        file.flush()?;
        let Some(terminated) = terminated.filter(|_| !announced) else {
            continue;
        };
        while let Some(end) = captured[scanned..].iter().position(|byte| *byte == b'\n') {
            let line = String::from_utf8_lossy(&captured[scanned..scanned + end]).into_owned();
            scanned += end + 1;
            if terminated(&line) {
                announced = true;
                let _ = terminal_seen.send(());
                break;
            }
        }
    }
    Ok(captured)
}

fn drain_stderr(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    io::copy(&mut reader, &mut buf)?;
    Ok(buf)
}

/// Fail the lane when the harness exited non-zero, naming a bounded stderr tail.
fn exit_check(run: &Output, harness: &str) -> Result<()> {
    if !run.status.success() {
        bail!(
            "{harness} exited {}: {}",
            run.status.code().map_or_else(|| "by signal".to_owned(), |code| code.to_string()),
            tail(&String::from_utf8_lossy(&run.stderr), 1000),
        );
    }
    Ok(())
}

/// A copy of `args` with the resume handle cleared — the fresh-launch fallback
/// an arm relaunches under when its harness rejects the handle.
pub(super) fn without_resume(args: &TransformArgs) -> TransformArgs {
    let mut args = args.clone();
    args.resume = None;
    args
}

/// Did the CLI refuse the resume handle *before* starting a billed turn?
///
/// A non-zero exit after the CLI emitted a transcript is an operational
/// failure (auth, crash, SIGKILL) — not a missing session — and must not
/// launch a second full-cost cold run. Spawn failures never reach here.
///
/// Shared across the arms deliberately: each harness spells its refusal
/// differently, but the conservative shape that makes degrading safe — an empty
/// transcript plus a stderr that both names the handle and says it was refused —
/// is the same judgement, and a per-arm copy would drift into a looser one.
pub(super) fn resume_handle_rejected(status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> bool {
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

/// What state the tree is in when a turn resumes an earlier conversation —
/// the one thing that conversation cannot see for itself, and so the one thing
/// the prompt has to correct.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Resumed {
    /// A retry lap in a fresh dispatch: the working tree was reset between
    /// laps, so the files the previous turn edited are gone.
    AfterReset,
    /// A continuation inside the same dispatch: the tree is exactly as the
    /// previous turn left it, plus whatever the mechanical fixers rewrote.
    /// The construct lane's post-fixer lint repair resumes this way, and
    /// telling it the tree was reset would send it to redo work that is
    /// already on disk — and to redo it against findings taken from the disk
    /// it was told to distrust.
    SameTree,
}

/// The prompt a resumed lap receives: the assembled prompt plus whatever
/// [`Resumed`] says the conversation is wrong about. A cold launch, and a
/// continuation on the tree the previous turn left, both get the prompt
/// unchanged.
pub(super) fn resumed_prompt(prompt: &str, resume: Option<&str>, resumed: Resumed) -> String {
    if resume.is_none() || resumed == Resumed::SameTree {
        return prompt.to_owned();
    }
    format!(
        "{prompt}\nThe working tree was reset since the previous attempt; do not assume files you edited last time are still there.\n"
    )
}

// A missing harness binary is the failure an operator hits first when a stage is
// calibrated onto a CLI their machine does not have, so it is named rather than
// left as a bare "No such file or directory".
fn spawn_context(error: io::Error, harness: &str) -> anyhow::Error {
    if error.kind() == io::ErrorKind::NotFound {
        return anyhow::anyhow!(
            "`{harness}` is not on PATH — this stage is calibrated onto a harness this worker lacks"
        );
    }
    anyhow::Error::new(error).context(format!("run {harness}"))
}

/// The last `max` bytes of `s`, snapped forward to a char boundary — a bounded
/// stderr tail for an operational failure.
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

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitStatus};
    use std::time::{Duration, Instant};
    use std::{env, fs, process, thread};

    use std::ffi::{OsStr, OsString};

    use super::{
        Call, Resumed, Terminal, Usage, build_dir, capture, capture_resumed, record, resume_handle_rejected,
        resumed_prompt,
    };

    #[test]
    fn a_model_lane_child_builds_into_the_slot_target_the_lane_was_lent() {
        // Tripwire (#5425): the slot's target is warm — it has already compiled
        // this workspace — and lending it is the whole reason the slot outlived
        // the checkout. A child that built anywhere else would miss the compiler
        // cache on the entire dependency tree, which is measured at 600–1500
        // misses a lap and most of the time a lap spends.
        assert_eq!(
            build_dir(Some(OsString::from("/runs/slot-1-target"))).as_deref(),
            Some(OsStr::new("/runs/slot-1-target")),
            "the lane hands its own build directory down rather than letting the child pick one",
        );
        assert_eq!(build_dir(None), None, "a lane with none names none: cargo's default is the honest answer");
        assert_eq!(build_dir(Some(OsString::new())), None, "and so is an empty one");
    }
    use crate::transform::peak_memory;

    // Tripwire: the envelope's two cross-lane fields. `result_record.is_error`
    // is what the construct lane's completion gate tests `== Some(false)`, and
    // `result.result` is where the review lane finds the critic's VERDICT line.
    // A record that renamed or dropped either fails the review gate closed,
    // which surfaces as a critic finding rather than a harness bug.
    #[test]
    fn the_envelope_carries_the_two_fields_the_lanes_read() {
        let clean = record(
            Some(Terminal { is_error: false, text: "all pillars clean.\nVERDICT: pass".to_owned(), usage: None }),
            None,
        );
        assert_eq!(clean["is_error"], false, "the construct gate reads this");
        assert_eq!(clean["result"]["is_error"], false, "the review gate reads this");
        assert_eq!(clean["result"]["result"], "all pillars clean.\nVERDICT: pass", "the verdict text");

        let errored = record(Some(Terminal { is_error: true, text: String::new(), usage: None }), None);
        assert_eq!(errored["is_error"], true);
        assert_eq!(errored["result"]["is_error"], true);
    }

    // Tripwire: the session handle rides the same `session_id` key the
    // Anthropic-Messages derivation writes, because the pool's deposit reads
    // exactly one key whichever arm produced the record — a handle parked under
    // an arm-specific name is a handle the pool never stores, so every lap
    // relaunches cold at full price while reporting success.
    #[test]
    fn the_session_handle_rides_the_key_the_pool_reads() {
        let terminal = || Some(Terminal { is_error: false, text: "ok".to_owned(), usage: None });
        assert_eq!(record(terminal(), Some("019f-thread".to_owned()))["session_id"], "019f-thread");
        assert!(record(terminal(), None)["session_id"].is_null(), "an arm that names no session says so");

        // A run that died mid-lap is exactly the one a later lap wants to
        // resume, so the handle survives the `no_result` row.
        let partial = record(None, Some("019f-thread".to_owned()));
        assert_eq!(partial["no_result"], true);
        assert_eq!(partial["session_id"], "019f-thread");
    }

    #[test]
    fn a_missing_session_is_a_resume_reject_and_a_crash_after_tokens_is_not() {
        // Tripwire: a non-zero exit used to relaunch cold whenever a resume
        // handle was on argv, so an auth failure or a crash that had already
        // billed doubled the spend. Only a handle the CLI refused *before*
        // emitting a transcript degrades.
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

    // A resumed conversation is wrong about the tree in exactly one direction,
    // and which one depends on why it was resumed. A retry lap remembers
    // editing files the lap-boundary reset removed, and a lap that trusts that
    // memory reports work it never redid. The construct lane's post-fixer lint
    // repair is the opposite case: it continues inside the dispatch that wrote
    // the tree, and telling *it* the tree was reset would send it to redo the
    // whole work order against findings taken from the disk it was told to
    // distrust. So the notice is posture-driven, not resume-driven.
    #[test]
    fn a_resumed_lap_is_told_its_tree_was_reset_only_when_it_was() {
        for posture in [Resumed::AfterReset, Resumed::SameTree] {
            assert_eq!(resumed_prompt("build it", None, posture), "build it", "a cold launch is never told either way");
        }

        let retried = resumed_prompt("build it", Some("sess-1"), Resumed::AfterReset);
        assert!(retried.starts_with("build it\n"));
        assert!(retried.contains("working tree was reset"), "a retry lap's tree really was reset");

        assert_eq!(
            resumed_prompt("build it", Some("sess-1"), Resumed::SameTree),
            "build it",
            "a continuation on the tree the previous turn wrote must not be told it was reset",
        );
    }

    // A harness that reports no counts renders them null, not zero: a study
    // grading such a bloom must read "unmeasured" rather than "free".
    #[test]
    fn an_unmetered_harness_renders_null_columns_never_zero() {
        let unmetered = record(Some(Terminal { is_error: false, text: "ok".to_owned(), usage: None }), None);
        for column in ["cost_usd", "input", "output", "cache_read", "cache_write", "calls"] {
            assert!(unmetered[column].is_null(), "{column} must be null, not zero, when unmeasured");
        }

        let metered = record(
            Some(Terminal {
                is_error: false,
                text: "ok".to_owned(),
                usage: Some(Usage { input: 16147, cache_read: 11008, cache_write: 0, output: 5, calls: Vec::new() }),
            }),
            None,
        );
        assert_eq!(metered["input"], 16147);
        assert_eq!(metered["output"], 5);
        assert_eq!(metered["cache_write"], 0, "a reported zero is a zero");
        assert!(metered["cost_usd"].is_null(), "no harness here reports a price");
        assert!(metered["calls"].is_null(), "an empty breakdown is unmeasured, never a run that made no call");
    }

    // Tripwire: the `calls` column is what the host reads, and its key names
    // belong to a decoder in another crate — `aether_bloomery_github::parse_study`
    // (`CallJson`). The session pool takes the last entry's prompt as the context
    // a resume would re-read (`session_reuse::parse_context_tokens`) and skips
    // the deposit for a record that names none; the sealed price table selects
    // its long-context band per call. A record that kept only the totals is what
    // left every Muse lap cold across bloom b7f0e4568d4a.
    #[test]
    fn the_breakdown_rides_the_envelope_under_the_keys_the_pool_decodes() {
        let metered = record(
            Some(Terminal {
                is_error: false,
                text: "ok".to_owned(),
                usage: Some(Usage {
                    input: 21460,
                    cache_read: 20465,
                    cache_write: 7,
                    output: 1244,
                    calls: vec![
                        Call { input: 20475, cache_read: 0, cache_write: 0, output: 955 },
                        Call { input: 985, cache_read: 20465, cache_write: 7, output: 289 },
                    ],
                }),
            }),
            None,
        );

        let calls = metered["calls"].as_array().expect("the breakdown is an array, so the pool can take its last");
        assert_eq!(calls.len(), 2, "one entry per model call, not one for the run");
        assert_eq!(calls[1]["input"], 985);
        assert_eq!(calls[1]["cache_read"], 20465);
        assert_eq!(calls[1]["cache_write"], 7);
        assert_eq!(calls[1]["output"], 289);
        // The pool's own reading over that last entry: uncached input plus both
        // cache classes, which is smaller than the billed aggregate the record
        // also carries. Deposit the aggregate instead and every later acquire
        // misses on the context cap.
        let last = &calls[1];
        let context: u64 =
            ["input", "cache_read", "cache_write"].iter().map(|column| last[column].as_u64().unwrap_or_default()).sum();
        assert!(
            context
                < metered["input"].as_u64().unwrap_or_default() + metered["cache_read"].as_u64().unwrap_or_default(),
            "the resumable context is one call's prompt, never the run's billed sum",
        );
    }

    // A run that died before its terminal is a legible `no_result` row carrying
    // no `is_error`, so the construct gate's `== Some(false)` test fails closed —
    // the same shape the Claude arm produces for a died-early run.
    #[test]
    fn a_died_early_run_is_a_no_result_row_that_fails_the_gate_closed() {
        let partial = record(None, None);
        assert_eq!(partial["no_result"], true);
        assert!(partial.get("is_error").is_none(), "no is_error means the construct gate fails closed");
        assert!(partial.get("result").is_none(), "and the review lane finds no verdict text");
    }

    /// A per-test evidence directory, unique per call so concurrent test threads
    /// never collide — the sibling lanes' convention.
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-lane-stream-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn wait_for(path: &Path, pred: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(bytes) = fs::read(path)
                && pred(&bytes)
            {
                return bytes;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {}", path.display());
            thread::sleep(Duration::from_millis(20));
        }
    }

    // The heartbeat is the file's presence (and later its mtime). Creating it
    // only after wait returns leaves the documented signal absent for the whole
    // run — the interval Bloomery most needs a liveness reading.
    #[test]
    fn the_transcript_is_created_before_the_child_can_emit() {
        let out = scratch_dir("created");
        let transcript = out.join("transcript.jsonl");
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!("test -f {} || exit 7", transcript.display()));

        let text = capture(command, &out, "fixture", &peak_memory::detect(), None).expect("child saw the transcript");
        assert_eq!(text, "");
        assert_eq!(fs::read(&transcript).expect("transcript remains"), b"");
    }

    // Buffering stdout and writing once at exit is the bug this replaces: an
    // observer must see the first flushed chunk while the child is still alive.
    #[test]
    fn the_transcript_grows_while_the_child_is_still_running() {
        let out = scratch_dir("grows");
        let go = out.join("go");
        let transcript = out.join("transcript.jsonl");
        let script = format!(
            "printf 'one\\n' | dd bs=4 count=1 2>/dev/null; \
             while [ ! -f {} ]; do :; done; \
             printf 'two\\n' | dd bs=4 count=1 2>/dev/null",
            go.display()
        );
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);

        thread::scope(|scope| {
            let worker = scope.spawn(|| capture(command, &out, "fixture", &peak_memory::detect(), None));
            assert_eq!(wait_for(&transcript, |bytes| bytes == b"one\n"), b"one\n");
            assert!(!worker.is_finished(), "the first chunk must land before wait returns");
            fs::write(&go, b"").expect("release the child");
            let text = worker.join().expect("capture thread panicked").expect("capture");
            assert_eq!(text, "one\ntwo\n");
            assert_eq!(fs::read(&transcript).expect("read transcript"), b"one\ntwo\n");
        });
    }

    // Reading stdout to completion before touching stderr deadlocks once the
    // child fills the unattended pipe (typically 64 KiB). Interleaved volume on
    // both sides must drain.
    #[test]
    fn stdout_and_stderr_volume_cannot_deadlock_the_child() {
        let out = scratch_dir("deadlock");
        let mut command = Command::new("sh");
        command.arg("-c").arg(
            r#"
i=0
while [ "$i" -lt 256 ]; do
  dd if=/dev/zero bs=1024 count=1 2>/dev/null
  dd if=/dev/zero bs=1024 count=1 2>/dev/null >&2
  i=$((i + 1))
done
"#,
        );

        let text = capture(command, &out, "fixture", &peak_memory::detect(), None).expect("drained both pipes");
        assert_eq!(text.len(), 256 * 1024);
        assert_eq!(fs::read(out.join("transcript.jsonl")).expect("read transcript").len(), 256 * 1024);
    }

    // The file keeps the child's exact bytes so result derivation can reread
    // nothing; the returned String is still the lossy conversion callers already
    // used. A UTF-8 "fixup" written to disk would change the transcript.
    #[test]
    fn returned_text_is_lossy_and_the_file_keeps_the_raw_bytes() {
        let out = scratch_dir("raw");
        let blob = out.join("blob");
        let raw = b"ok\xffmore";
        fs::write(&blob, raw).expect("write fixture bytes");
        let mut command = Command::new("cat");
        command.arg(&blob);

        let text = capture(command, &out, "fixture", &peak_memory::detect(), None).expect("capture");
        assert_eq!(text, String::from_utf8_lossy(raw));
        assert_eq!(fs::read(out.join("transcript.jsonl")).expect("read transcript"), raw);
    }

    // A nonzero exit after any streamed byte has already spent (or might have).
    // capture_resumed must fail the lane, not return Ok(None) and invite a
    // second paid launch.
    #[test]
    fn a_partial_transcript_is_not_a_resume_refusal() {
        let out = scratch_dir("partial");
        let mut command = Command::new("sh");
        command.arg("-c").arg("printf '{\"type\":\"result\"}\\n'; echo 'No conversation found' >&2; exit 1");

        let err = capture_resumed(command, &out, "fixture", &peak_memory::detect()).expect_err("must not degrade");
        assert!(err.to_string().contains("exited 1"), "{err}");
        assert_eq!(fs::read(out.join("transcript.jsonl")).expect("read transcript"), b"{\"type\":\"result\"}\n");
    }

    // The conservative degrade stays: empty captured stdout plus a stderr that
    // both names the handle and says it was refused.
    #[test]
    fn an_empty_transcript_with_a_refusal_stays_resume_eligible() {
        let out = scratch_dir("refuse");
        let mut command = Command::new("sh");
        command.arg("-c").arg("echo 'No conversation found with session ID sess-1' >&2; exit 1");

        let result = capture_resumed(command, &out, "fixture", &peak_memory::detect()).expect("eligible degrade");
        assert!(result.is_none(), "empty + refusal signature relaunches cold");
        assert_eq!(fs::read(out.join("transcript.jsonl")).expect("empty transcript was still created"), b"");
    }
}
