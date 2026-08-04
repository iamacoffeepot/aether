//! Wire kinds for the `aether.process` capability (ADR-0157, ADR-0121).
//!
//! The cap owns its mail vocabulary in-crate rather than parking it in
//! `aether-kinds`: a guest that wants to shell out depends on this crate
//! directly and names [`ProcessCapability`](super::ProcessCapability) +
//! these kinds together (ADR-0121 kind ownership).
//!
//! One request kind ([`Run`]) and one reply kind ([`RunResult`]). The
//! reply is a closed enum, not a flat struct wearing boolean flags,
//! because the outcomes are enumerable and this repository enumerates
//! them by rule (the neighboring edge caps do — `FsError`'s typed
//! variants, `AnthropicError`'s taxonomy). `run` is request/reply and
//! the reply always arrives.

use serde::{Deserialize, Serialize};

/// One explicit child-environment entry. The child's environment is
/// built solely from the request's `env` list — the substrate's own
/// environment (which holds provider API keys and other fleet secrets)
/// is never inherited (ADR-0157 §Security). A child receives exactly the
/// variables the caller names and nothing else.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

/// Why a [`Run`] could not run or reap the child (ADR-0157 §Mail
/// surface). A closed taxonomy — a completed run (including a non-zero
/// exit) is [`RunResult::Ok`], and a deadline overrun is
/// [`RunResult::TimedOut`], so this enum names only the capability's own
/// inability to run or reap the child. A future distinction adds an arm
/// through an ADR amendment rather than widening a free-form string.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ProcessError {
    /// The requested `binary` is absent from the allowlist; refused
    /// before any spawn. The allowlist is empty by default, so a
    /// freshly booted capability refuses every request until an operator
    /// names the binaries it may run.
    NotPermitted,
    /// The allowlisted path did not resolve to an executable file (the
    /// `ErrorKind::NotFound` spawn path).
    BinaryNotFound,
    /// Exec failed for another reason (not executable, permission denied
    /// at exec). Carries the OS detail.
    SpawnFailed { detail: String },
    /// The OS returned an error while waiting on the child. Carries the
    /// OS detail.
    WaitFailed { detail: String },
}

/// `aether.process.run` — run a permitted binary to completion and
/// capture its output (ADR-0157). Mailed to the `"aether.process"`
/// mailbox; the reply lands as [`RunResult`] on the caller's settlement
/// chain (the run rides ADR-0093 dispatch, so no correlation id is
/// carried — the held reply target routes the reply).
///
/// - `binary` is a **logical name** resolved against the operator's
///   allowlist, never a filesystem path — the caller cannot reach an
///   arbitrary executable.
/// - `args` is argv, passed verbatim to the child. The capability never
///   interprets a shell string: shell metacharacters in an argument are
///   inert data (ADR-0157 §Security, "argv only, no shell, ever").
/// - `env` is the child's *entire* environment; the substrate's own
///   environment is not inherited.
/// - `stdin` is fed to the child's stdin, then the pipe is closed (EOF).
/// - `timeout_millis` is the deadline; the child (and its process group)
///   is killed and reaped on overrun. `0` selects the configured default
///   timeout.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.process.run")]
pub struct Run {
    pub binary: String,
    pub args: Vec<String>,
    pub env: Vec<EnvVar>,
    #[serde(with = "aether_data::bytes")]
    pub stdin: Vec<u8>,
    pub timeout_millis: u32,
}

/// Reply to [`Run`] (ADR-0157). A closed enum over the three distinct
/// outcomes.
///
/// - `Ok` is a run that reached completion, **including a run that
///   exited non-zero** — a non-zero exit is a completed run whose result
///   the consumer judges, not a capability failure. `exit_code` is
///   `None` when the child died by signal.
/// - `TimedOut` carries the partial `stdout` / `stderr` drained before
///   the deadline kill; it is a distinct outcome, not an `Ok` wearing a
///   boolean flag.
/// - `Err` carries the closed [`ProcessError`] taxonomy — only the
///   capability's own inability to run or reap the child.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.process.run_result")]
pub enum RunResult {
    Ok {
        exit_code: Option<i32>,
        #[serde(with = "aether_data::bytes")]
        stdout: Vec<u8>,
        #[serde(with = "aether_data::bytes")]
        stderr: Vec<u8>,
    },
    TimedOut {
        #[serde(with = "aether_data::bytes")]
        stdout: Vec<u8>,
        #[serde(with = "aether_data::bytes")]
        stderr: Vec<u8>,
    },
    Err {
        error: ProcessError,
    },
}
