//! The `aether.process` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;`
//! declaration in the parent carries the gate), so a transport-only build
//! of the [`ProcessCapability`](super::ProcessCapability) identity never
//! names these types nor pulls `aether_substrate`.
//!
//! Each `run` request dispatches the blocking spawn-and-capture through
//! ADR-0093's hold-until-resolve primitive (via the cap-level
//! [`TaskQueue`] concurrency bound, exactly as the content-gen caps drive
//! their provider calls): the closure owns the resolved command and runs
//! the deadline / drain / reap loop (the `runner` module) off the
//! dispatcher on a worker thread, and a `#[handler(task)]` completion
//! re-replies the `run_result` through the caller's held reply target.
//! The caller's settlement chain stays held across the whole run, so
//! `send_mail_traced` observes it as one in-flight unit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::kinds::{EnvVar, ProcessError, Run, RunResult};
use super::runner::{RunOutcome, run_to_completion};
use super::{ProcessCapability, ProcessConfig};

use aether_actor::runtime;
use aether_actor::{Manual, OutboundReply};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone, TaskQueue};
pub use aether_substrate::chassis::error::BootError;

/// Composer-supplied construction params for the `aether.process` cap
/// (ADR-0156 §3): the working-directory confinement root. Resolved at
/// chassis boot from the `aether.fs` `save` namespace root — the same
/// value the fs cap owns — so a run starts inside the sandbox the
/// filesystem capability already governs (ADR-0157 §Security,
/// working-directory confinement) rather than the substrate's cwd. It is
/// a value computed at Compose from another resolved member, not
/// operator-typed config, so it rides `Params` while the cap's `Config`
/// stays the derive-`Config` [`ProcessConfig`].
pub struct ProcessParams {
    pub work_root: PathBuf,
}

/// `aether.process` runtime state (ADR-0157). Owns the resolved allowlist
/// (logical name → absolute path), the default per-run deadline, the
/// confinement root, and the cap-level concurrency queue over the
/// ADR-0093 dispatch primitive. Single-threaded post-ADR-0038, so the
/// queue state lives in plain fields with no lock. The addressing
/// identity is the distinct ZST [`ProcessCapability`].
pub struct ProcessCapabilityState {
    allowlist: HashMap<String, PathBuf>,
    default_timeout: Duration,
    work_root: PathBuf,
    tasks: TaskQueue,
}

#[cfg(test)]
impl ProcessCapabilityState {
    /// Test-only constructor. Production boots through
    /// `Builder::with_actor::<ProcessCapability>(params)`; tests hand in a
    /// resolved allowlist + roots directly.
    fn from_parts(allowlist: HashMap<String, PathBuf>, work_root: PathBuf, max_in_flight: usize) -> Self {
        Self { allowlist, default_timeout: Duration::from_secs(30), work_root, tasks: TaskQueue::new(max_in_flight) }
    }

    /// White-box accessor for the queue's in-flight counter (e.g. that a
    /// refused request never spawned work).
    fn test_in_flight(&self) -> usize {
        self.tasks.in_flight()
    }
}

#[runtime]
impl NativeActor for ProcessCapability {
    /// The runtime state this identity boots into (ADR-0122 split).
    type State = ProcessCapabilityState;

    type Config = ProcessConfig;
    type Params = ProcessParams;

    /// ADR-0157 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.process";

    /// Resolve the allowlist tokens into a name → absolute-path map and
    /// capture the confinement root + concurrency bound. A malformed
    /// allowlist token is dropped (deny-by-default: a bad entry never
    /// becomes a permitted binary).
    fn init(
        config: ProcessConfig,
        params: ProcessParams,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<ProcessCapabilityState, BootError> {
        let allowlist = resolve_allowlist(&config.allowlist);
        tracing::info!(
            target: "aether_process",
            permitted = allowlist.len(),
            work_root = %params.work_root.display(),
            default_timeout_millis = config.default_timeout.as_millis(),
            "process exec capability configured",
        );
        Ok(ProcessCapabilityState {
            allowlist,
            default_timeout: config.default_timeout,
            work_root: params.work_root,
            tasks: TaskQueue::new(config.max_in_flight),
        })
    }

    /// Run a permitted binary to completion off the dispatcher thread.
    ///
    /// # Agent
    /// Reply: `run_result`. Resolves `binary` against the allowlist
    /// synchronously (`Err { NotPermitted }` on a miss, before any
    /// spawn), then dispatches the blocking spawn-and-capture on a worker
    /// thread; the reply lands when the run completes, times out, or
    /// fails to spawn/reap.
    #[handler::manual]
    fn on_run(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: Run) {
        // Deny-by-default: an unlisted binary is refused before any spawn.
        let Some(path) = state.allowlist.get(&mail.binary).cloned() else {
            OutboundReply::reply(ctx, &RunResult::Err { error: ProcessError::NotPermitted });
            return;
        };

        let timeout = if mail.timeout_millis == 0 {
            state.default_timeout
        } else {
            Duration::from_millis(u64::from(mail.timeout_millis))
        };
        let Run { args, env, stdin, .. } = mail;
        let work_root = state.work_root.clone();

        state.tasks.submit(ctx, move || {
            let command = build_command(&path, &args, &env, &work_root);
            outcome_to_result(run_to_completion(command, stdin, timeout))
        });
    }

    /// ADR-0093 completion for a finished run: re-reply the worker's
    /// `run_result` to the original caller (drops the hold), then free
    /// the in-flight slot (draining the next queued run).
    #[handler(task)]
    fn on_run_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<RunResult>) {
        done.resolve(ctx);
        state.tasks.on_complete(ctx);
    }
}

/// Build the child `Command` from the resolved absolute `path`, the
/// request's `args` (argv, verbatim — never a shell string), and its
/// explicit `env` entries. The environment is **constructed, not
/// inherited**: `env_clear` drops the substrate's own environment (which
/// holds provider API keys and other fleet secrets) so the child receives
/// exactly the variables the caller named and nothing else. The working
/// directory is confined to `work_root` (ADR-0157 §Security).
fn build_command(path: &Path, args: &[String], env: &[EnvVar], work_root: &Path) -> Command {
    let mut command = Command::new(path);
    command.args(args).env_clear().current_dir(work_root);
    for EnvVar { key, value } in env {
        command.env(key, value);
    }
    command
}

/// Map the pure loop's [`RunOutcome`] onto the wire [`RunResult`]. A
/// completed run (including a non-zero exit) is `Ok`; a deadline overrun
/// is `TimedOut`; a spawn `NotFound` is `BinaryNotFound` and any other
/// spawn failure is `SpawnFailed`; a wait error is `WaitFailed`.
fn outcome_to_result(outcome: RunOutcome) -> RunResult {
    match outcome {
        RunOutcome::Completed { exit_code, stdout, stderr } => RunResult::Ok { exit_code, stdout, stderr },
        RunOutcome::TimedOut { stdout, stderr } => RunResult::TimedOut { stdout, stderr },
        RunOutcome::SpawnFailed { not_found: true, .. } => RunResult::Err { error: ProcessError::BinaryNotFound },
        RunOutcome::SpawnFailed { not_found: false, detail } => {
            RunResult::Err { error: ProcessError::SpawnFailed { detail } }
        }
        RunOutcome::WaitFailed { detail } => RunResult::Err { error: ProcessError::WaitFailed { detail } },
    }
}

/// Split the allowlist tokens (`"name=/absolute/path"`) into a name →
/// path map. A token with no `=`, an empty name, or an empty path is
/// dropped with a warning — deny-by-default demands a malformed entry
/// never silently become a permitted binary.
fn resolve_allowlist(tokens: &HashSet<String>) -> HashMap<String, PathBuf> {
    let mut map = HashMap::with_capacity(tokens.len());
    for token in tokens {
        match token.split_once('=') {
            Some((name, path)) if !name.is_empty() && !path.is_empty() => {
                map.insert(name.to_owned(), PathBuf::from(path));
            }
            _ => {
                tracing::warn!(
                    target: "aether_process",
                    token = %token,
                    "dropping malformed allowlist entry (expected name=/absolute/path)",
                );
            }
        }
    }
    map
}

#[cfg(all(test, feature = "runtime", unix))]
mod tests {
    use super::{ProcessCapability, ProcessCapabilityState, RunOutcome, outcome_to_result, resolve_allowlist};
    use crate::kinds::{ProcessError, Run, RunResult};
    use aether_data::{MailId, MailboxId, SessionToken, Source, SourceAddr, Uuid};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::ctx::NativeCtx;
    use aether_substrate::testing::{decode_session_reply, drive_task_completion, test_mailer_and_rx};
    use std::collections::{HashMap, HashSet};
    use std::env;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn session_sender() -> Source {
        Source::to(SourceAddr::Session(SessionToken(Uuid::nil())))
    }

    fn run(binary: &str, stdin: &[u8]) -> Run {
        Run { binary: binary.to_owned(), args: Vec::new(), env: Vec::new(), stdin: stdin.to_vec(), timeout_millis: 0 }
    }

    /// The allowlist parse is the security boundary: a well-formed
    /// `name=path` token maps, and a malformed token is dropped rather
    /// than silently admitted as a permitted binary.
    #[test]
    fn resolve_allowlist_maps_valid_and_drops_malformed() {
        let tokens: HashSet<String> =
            ["cat=/bin/cat", "noequalssign", "=/bin/orphan", "empty="].iter().map(|s| (*s).to_owned()).collect();
        let map = resolve_allowlist(&tokens);
        assert_eq!(map.get("cat"), Some(&PathBuf::from("/bin/cat")));
        assert_eq!(map.len(), 1, "the three malformed tokens are dropped");
    }

    /// The `not_found` spawn split is load-bearing: a missing path is
    /// `BinaryNotFound`, any other spawn failure is `SpawnFailed`.
    #[test]
    fn outcome_maps_spawn_not_found_to_binary_not_found() {
        assert!(matches!(
            outcome_to_result(RunOutcome::SpawnFailed { not_found: true, detail: "x".into() }),
            RunResult::Err { error: ProcessError::BinaryNotFound }
        ));
        assert!(matches!(
            outcome_to_result(RunOutcome::SpawnFailed { not_found: false, detail: "denied".into() }),
            RunResult::Err { error: ProcessError::SpawnFailed { .. } }
        ));
    }

    /// An unlisted binary is refused synchronously with `NotPermitted`
    /// and spawns no work — the deny-by-default boundary never reaches the
    /// dispatch helper.
    #[test]
    fn unlisted_binary_replies_not_permitted_without_dispatch() {
        let (mailer, rx) = test_mailer_and_rx();
        let mut state = ProcessCapabilityState::from_parts(HashMap::new(), env::temp_dir(), 4);
        let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
        let mut ctx = NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);

        ProcessCapability::on_run(&mut state, &mut ctx, run("cat", b""));

        match decode_session_reply::<RunResult>(&rx) {
            RunResult::Err { error: ProcessError::NotPermitted } => {}
            other => panic!("expected NotPermitted, got {other:?}"),
        }
        assert_eq!(state.test_in_flight(), 0, "a refused request spawns no in-flight work");
    }

    /// An allowlisted benign binary runs end-to-end through the ADR-0093
    /// dispatch: the cap submits to the queue, the real worker runs
    /// `/bin/cat` (echoing stdin), pushes a completion wake, and the
    /// cap's `#[handler(task)]` re-replies `Ok` with the captured stdout
    /// to the caller. Crosses the actor + dispatch + reply + settlement
    /// boundary without a GPU (the process cap is headless).
    #[test]
    fn allowlisted_cat_runs_and_replies_ok_with_stdout() {
        let (mailer, rx) = test_mailer_and_rx();
        let allowlist = HashMap::from([("cat".to_owned(), PathBuf::from("/bin/cat"))]);
        let mut state = ProcessCapabilityState::from_parts(allowlist, env::temp_dir(), 4);
        let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
        let mut ctx = NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);

        ProcessCapability::on_run(&mut state, &mut ctx, run("cat", b"hello aether"));
        // The worker runs cat against the piped stdin and pushes the
        // completion wake; route it through the cap's task handler.
        drive_task_completion::<ProcessCapability>(&mut state, &transport, &rx);

        match decode_session_reply::<RunResult>(&rx) {
            RunResult::Ok { exit_code, stdout, stderr } => {
                assert_eq!(exit_code, Some(0));
                assert_eq!(stdout, b"hello aether");
                assert!(stderr.is_empty());
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }
}
