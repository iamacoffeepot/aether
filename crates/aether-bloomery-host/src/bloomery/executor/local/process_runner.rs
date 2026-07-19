//! The production spawn seam: `git worktree add` the sealed checkout, then spawn
//! `cargo xtask transform` in that worktree — the same two steps the wrapper
//! workflow runs, performed natively on the operator's machine.

use std::path::Path;
use std::process::{Child, Command};
use std::{fs, io};

use aether_bloomery::REVIEW_CRITIC_COMMAND;
use aether_bloomery_github::GitObjectId;

use super::error::LocalExecutorError;
use super::runner::{CapturedObjects, RunLifecycle, RunProcess, RunSpec, TransformRunner};
use crate::bloomery::CONSTRUCT_IMPLEMENT_COMMAND;

/// The production spawn seam: `git worktree add` the checkout, then spawn
/// `cargo xtask transform` in that worktree — the same two steps the wrapper
/// workflow runs, performed natively on the operator's machine.
pub struct ProcessTransformRunner;

impl TransformRunner for ProcessTransformRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
        // Materialize the sealed checkout into a detached scratch worktree. `--force`
        // reclaims a stale worktree dir left by a prior aborted run at the same nonce.
        let checkout = Command::new("git")
            .args(["worktree", "add", "--force", "--detach"])
            .arg(spec.worktree_dir)
            .arg(spec.checkout_hex)
            .output()
            .map_err(LocalExecutorError::Spawn)?;
        if !checkout.status.success() {
            return Err(LocalExecutorError::Worktree(tail(&String::from_utf8_lossy(&checkout.stderr), 1000)));
        }
        // The scratch worktree is a full checkout carrying the repo's interactive
        // `.claude/settings.json` hooks, and the construct lane spawns headless
        // `claude` in it — so the SessionStart worktree-rebind hook would fire and
        // strand the candidate diff in a nested session worktree (#3632). Neutralize
        // the hooks the same way the headless action does
        // (`.github/actions/headless-claude/action.yml`): strip the `hooks` key and
        // mark the file skip-worktree so the edit neither shows as a candidate nor
        // can be committed.
        neutralize_hooks(spec.worktree_dir)?;
        // Spawn the same portable entrypoint the wrappers run, in the checked-out
        // worktree, under the ambient local `claude` auth (ADR-0150).
        let mut cargo = Command::new("cargo");
        cargo
            .current_dir(spec.worktree_dir)
            .args(["xtask", "transform", spec.command, "--out"])
            .arg(spec.evidence_dir)
            .args(["--nonce", spec.nonce]);
        if spec.command == CONSTRUCT_IMPLEMENT_COMMAND || spec.command == REVIEW_CRITIC_COMMAND {
            cargo.args(["--subject", spec.checkout_hex]);
            if let Some(model) = spec.model {
                cargo.args(["--model", model]);
            }
            if let Some(effort) = spec.effort {
                cargo.args(["--effort", effort]);
            }
            if let Some(task) = spec.task {
                cargo.args(["--task", task]);
            }
        }
        let child = cargo.spawn().map_err(LocalExecutorError::Spawn)?;
        Ok(Box::new(ChildProcess { child }))
    }

    fn release(&self, worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        // Tear the scratch worktree back down on the run's terminal path. `--force`
        // discards the run's working-tree changes (the candidate has already been
        // captured and read) and drops the admin entry `git worktree add` registered,
        // so a long-lived backend does not leak one worktree per order.
        let removed = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(worktree_dir)
            .output()
            .map_err(LocalExecutorError::Spawn)?;
        if !removed.status.success() {
            return Err(LocalExecutorError::Worktree(tail(&String::from_utf8_lossy(&removed.stderr), 1000)));
        }
        Ok(())
    }

    fn capture(&self, worktree_dir: &Path) -> Result<Option<CapturedObjects>, LocalExecutorError> {
        // A clean worktree has nothing to capture — the caller fails the run
        // closed rather than minting an empty candidate.
        if git_in(worktree_dir, &["status", "--porcelain"])?.trim().is_empty() {
            return Ok(None);
        }
        git_in(worktree_dir, &["add", "--all"])?;
        // Commit under the bloomery's own fixed identity, in the host's trust
        // domain (ADR-0152: the child never stages, commits, or holds
        // credentials). `--no-verify` keeps repo hooks out of the capture path —
        // the run's own gates already judged the work.
        git_in(
            worktree_dir,
            &[
                "-c",
                "user.name=aether-bloomery",
                "-c",
                "user.email=bloomery@iamateapot.dev",
                "commit",
                "--no-verify",
                "--message",
                "bloomery: candidate capture",
            ],
        )?;
        let commit_hex = git_in(worktree_dir, &["rev-parse", "HEAD"])?;
        #[allow(clippy::literal_string_with_formatting_args, reason = "git revision syntax, not a format string")]
        let tree_hex = git_in(worktree_dir, &["rev-parse", "HEAD^{tree}"])?;
        let commit = GitObjectId::from_hex(commit_hex.trim()).ok_or_else(|| {
            LocalExecutorError::Worktree(format!("malformed capture commit sha `{}`", commit_hex.trim()))
        })?;
        let tree = GitObjectId::from_hex(tree_hex.trim())
            .ok_or_else(|| LocalExecutorError::Worktree(format!("malformed capture tree sha `{}`", tree_hex.trim())))?;
        Ok(Some(CapturedObjects { commit, tree }))
    }
}

// Run one git command inside `dir`, returning its stdout — the capture path's
// shell helper, error-shaped like the worktree add/remove shell-outs above.
fn git_in(dir: &Path, args: &[&str]) -> Result<String, LocalExecutorError> {
    let output = Command::new("git").current_dir(dir).args(args).output().map_err(LocalExecutorError::Spawn)?;
    if !output.status.success() {
        return Err(LocalExecutorError::Worktree(tail(&String::from_utf8_lossy(&output.stderr), 1000)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A live `cargo xtask transform` child.
struct ChildProcess {
    child: Child,
}

impl RunProcess for ChildProcess {
    fn poll(&mut self) -> RunLifecycle {
        match self.child.try_wait() {
            Ok(Some(status)) => RunLifecycle::Exited { success: status.success() },
            Ok(None) => RunLifecycle::Running,
            // A wait fault is not a live run; read it as a failed exit rather than
            // reporting an eternally-running child.
            Err(_) => RunLifecycle::Exited { success: false },
        }
    }

    fn kill(&mut self) -> Result<(), LocalExecutorError> {
        self.child.kill().map_err(LocalExecutorError::Io)?;
        // Reap so the killed child does not linger as a zombie; a reap fault is a
        // real failure folded into the returned error, not silently swallowed.
        self.child.wait().map_err(LocalExecutorError::Io)?;
        Ok(())
    }
}

/// The last `max` bytes of `s`, snapped forward to a char boundary — a bounded
/// stderr tail for a worktree failure.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_owned()
}

/// Strip the top-level `hooks` key from a `.claude/settings.json` body, returning
/// the re-serialized JSON (pretty, trailing newline, matching the checked-in
/// shape). The pure core of the interactive-hook neutralization (#3632) —
/// unit-tested in isolation; a body with no `hooks` key round-trips unchanged in
/// content, malformed JSON is an `Err`.
fn strip_hooks(settings: &str) -> Result<String, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(settings)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("hooks");
    }
    let mut rendered = serde_json::to_string_pretty(&value)?;
    rendered.push('\n');
    Ok(rendered)
}

/// Neutralize the interactive-session `.claude/settings.json` hooks in the scratch
/// worktree (#3632): strip the `hooks` key through [`strip_hooks`] and mark the
/// file skip-worktree so the edit neither reads as a candidate change nor can be
/// committed. Absence of `.claude/settings.json` is a clean no-op (mirroring the
/// headless action's absence handling); a read, parse, or write fault surfaces as
/// an error rather than silently proceeding with live hooks.
fn neutralize_hooks(worktree_dir: &Path) -> Result<(), LocalExecutorError> {
    let settings_path = worktree_dir.join(".claude/settings.json");
    let body = match fs::read_to_string(&settings_path) {
        Ok(body) => body,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(LocalExecutorError::Io(error)),
    };
    let stripped = strip_hooks(&body).map_err(|error| LocalExecutorError::Io(io::Error::other(error)))?;
    fs::write(&settings_path, stripped).map_err(LocalExecutorError::Io)?;
    // Mark the stripped file skip-worktree so it stays out of the scratch-root
    // `git status --porcelain` the candidate detection reads (#3632) and can never
    // be committed as part of a candidate.
    let marked = Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .args(["update-index", "--skip-worktree", ".claude/settings.json"])
        .output()
        .map_err(LocalExecutorError::Spawn)?;
    if !marked.status.success() {
        return Err(LocalExecutorError::Worktree(tail(&String::from_utf8_lossy(&marked.stderr), 1000)));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::strip_hooks;

    #[test]
    fn strip_hooks_removes_the_hooks_key() {
        let settings = r#"{"hooks": {"SessionStart": [{"command": "rebind"}]}, "model": "opus"}"#;
        let stripped = strip_hooks(settings).unwrap();
        let value: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        assert!(value.get("hooks").is_none(), "the hooks key must be gone");
        assert_eq!(value.get("model").and_then(serde_json::Value::as_str), Some("opus"), "other keys survive");
        assert!(stripped.ends_with('\n'), "the checked-in shape carries a trailing newline");
    }

    #[test]
    fn strip_hooks_leaves_a_hookless_body_unchanged_in_content() {
        let settings = r#"{"model": "opus", "permissions": {"allow": []}}"#;
        let stripped = strip_hooks(settings).unwrap();
        let before: serde_json::Value = serde_json::from_str(settings).unwrap();
        let after: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(before, after, "a body with no hooks key round-trips unchanged");
    }

    #[test]
    fn strip_hooks_errs_on_malformed_json() {
        assert!(strip_hooks("{not json").is_err(), "malformed JSON surfaces as an error, not a silent no-op");
    }
}
