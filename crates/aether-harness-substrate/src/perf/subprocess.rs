//! Re-exec'ing this binary to measure one thing in a fresh process.
//!
//! Two perf lanes need the same boundary for different reasons, and both need
//! it for the same underlying fact: **process history is a hidden variable in a
//! perf measurement**. ADR-0085 §1 replicates whole runs in fresh processes so
//! the band covers between-run condition variance rather than within-run
//! sampling spread; iamacoffeepot/aether#4177 found that a dispatch cell boots
//! into one of two execution modes decided by the process state it inherits.
//! The dispatch sweep isolates per *cell* ([`super::isolate`]) and the registry
//! replication per *trial* ([`super::registry::band`]), but the mechanism is
//! one: set an env selector, re-exec, read the child's JSON off stdout.
//!
//! Only stdout is the result channel. The child's stderr is inherited so its
//! `warn` about a failed boot or a lapped ring lands where an in-process run's
//! would, instead of being swallowed into a captured buffer and lost with the
//! cell that produced it.

use std::path::Path;
use std::process::{Command, Stdio};

use serde::de::DeserializeOwned;

/// Run `exe` with `env` layered over the inherited environment and decode its
/// stdout as `T`.
///
/// The child inherits this process's whole environment — every `AETHER_PERF_*`
/// knob the parent already parsed — so only the selector in `env` has to cross
/// the boundary and there is no second copy of the run's parameters to drift.
///
/// # Errors
///
/// A description of what went wrong, for the caller to report against whatever
/// the child was measuring: the spawn failed, the child exited non-zero, its
/// stdout was not UTF-8 or not decodable, or it wrote nothing at all. Empty
/// stdout is an error rather than a default value — the child measured nothing
/// and has already said why on the shared stderr.
pub fn run_child_json<T: DeserializeOwned>(exe: &Path, env: &[(&str, String)]) -> Result<T, String> {
    let out = Command::new(exe)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!("exited with {}", out.status));
    }
    let stdout = String::from_utf8(out.stdout).map_err(|e| format!("stdout was not utf-8: {e}"))?;
    let json = stdout.trim();
    if json.is_empty() {
        return Err("produced no result".to_owned());
    }
    serde_json::from_str(json).map_err(|e| format!("undecodable result: {e}"))
}
