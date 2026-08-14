//! The production spawn seam: bring the lane slot's checkout to the sealed
//! subject, then spawn `cargo xtask transform` in it — the same two steps the
//! wrapper workflow runs, performed natively on the operator's machine.
//!
//! The slot's checkout is a path rather than a directory: it belongs to the lane
//! slot and every dispatch that holds the slot builds in it, because that is
//! what makes a compilation cacheable across dispatches (#4904 — `sccache`
//! hashes the paths cargo names on each `rustc` invocation). So the first step
//! is a *reset* rather than a fresh `git worktree add`, and what it must
//! guarantee is that a dispatch never sees anything of the dispatch before it:
//! see [`materialize_checkout`].
//!
//! The slot's cargo target directory is its sibling rather than a directory
//! inside it, for that same reason turned around (#4912): the reset removes
//! ignored files, so a build tree under the checkout would be deleted on every
//! dispatch. It reaches the lane — and through it every verify gate the lane
//! spawns — as `CARGO_TARGET_DIR`; see [`export_build_env`].

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::{fs, io};

use aether_bloomery::{BackendObjectId, is_model_lane};

use super::error::LocalExecutorError;
use super::lane_env::{inherited_keys, scrub_coordinator_env};
use super::lane_program::LaneProgram;
use super::runner::{CapturedObjects, RunLifecycle, RunProcess, RunSpec, TransformRunner};

/// The operator a candidate capture is authored as (#4630), resolved from the
/// `AETHER_BLOOMERY_OPERATOR_*` knobs.
///
/// Both fields empty — the default — means *inherit the host's ambient git
/// identity*: a bloom is the operator's own work delegated to a machine, not a
/// separate contributor, so the commit history reads that way with no
/// configuration at all. A deployment that wants a distinct, stable identity
/// sets both knobs explicitly.
#[derive(Debug, Clone, Default)]
pub struct CaptureIdentity {
    /// The configured author name, or empty to inherit the host's.
    pub name: String,
    /// The configured author email, or empty to inherit the host's.
    pub email: String,
}

/// The identity a capture falls back to when neither the config nor the host
/// supplies one — losing attribution is cosmetic, losing a candidate after a
/// full model run is not.
const FALLBACK_IDENTITY: (&str, &str) = ("aether-bloomery", "bloomery@iamateapot.dev");

/// The subject a capture commit falls back to when the run's lane named none —
/// the flat literal every capture carried before the lane wrote its own message.
const FALLBACK_CAPTURE_SUBJECT: &str = "bloomery: candidate capture";

/// The interactive-session settings file a dispatch neutralizes in its checkout,
/// relative to the checkout root. Named once because two steps address it: the
/// neutralization that marks it skip-worktree, and the reset that clears the
/// mark before the next dispatch checks the file out again.
const SETTINGS_PATH: &str = ".claude/settings.json";

/// The subject line a capture commits under: the run's own message's first line
/// when the lane wrote one, otherwise [`FALLBACK_CAPTURE_SUBJECT`].
///
/// Only the first line, because a commit subject is a line: the rest of the
/// message is the landing proposal's body, assembled where the whole membership
/// is in view. A message whose first line is blank names nothing, so it takes
/// the fallback rather than committing under an empty subject git would refuse.
fn capture_subject(message: Option<&str>) -> &str {
    message
        .and_then(|message| message.lines().next())
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .unwrap_or(FALLBACK_CAPTURE_SUBJECT)
}

impl CaptureIdentity {
    /// The `-c user.name=… -c user.email=…` arguments this identity contributes
    /// to the capture commit, resolved against the host.
    ///
    /// Configured knobs win. Otherwise an empty list lets git resolve the host's
    /// ambient identity — unless the host has none either, in which case the
    /// capture would fail outright after the model has already run, so
    /// [`FALLBACK_IDENTITY`] stands in.
    ///
    /// Both knobs are required together: a half-configured identity is a
    /// misconfiguration rather than a request to mix a configured name with an
    /// ambient email.
    fn overrides(&self, worktree_dir: &Path) -> Vec<String> {
        let (name, email) = match (self.name.as_str(), self.email.as_str()) {
            ("", "") if host_identity_resolves(worktree_dir) => return Vec::new(),
            ("", "") => FALLBACK_IDENTITY,
            (name, email) => (name, email),
        };
        vec!["-c".to_owned(), format!("user.name={name}"), "-c".to_owned(), format!("user.email={email}")]
    }
}

/// Whether the host resolves a committer identity for this worktree — `git
/// config --get` exits non-zero when the key is unset, which [`git_in`] surfaces
/// as an error.
fn host_identity_resolves(worktree_dir: &Path) -> bool {
    ["user.name", "user.email"]
        .iter()
        .all(|key| git_in(worktree_dir, &["config", "--get", key]).is_ok_and(|value| !value.trim().is_empty()))
}

/// The production spawn seam: `git worktree add` the checkout, then spawn
/// `cargo xtask transform` in that worktree — the same two steps the wrapper
/// workflow runs, performed natively on the operator's machine.
#[derive(Debug, Clone, Default)]
pub struct ProcessTransformRunner {
    /// Who candidate captures are authored as.
    identity: CaptureIdentity,
    /// Which program a dispatch spawns in the scratch worktree (#4727).
    lane_program: LaneProgram,
}

impl ProcessTransformRunner {
    /// Build the runner over the capture identity and lane invocation the host
    /// resolved.
    #[must_use]
    pub fn new(identity: CaptureIdentity, lane_program: LaneProgram) -> Self {
        Self { identity, lane_program }
    }
}

impl TransformRunner for ProcessTransformRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
        fetch_subject_if_absent(Path::new("."), spec.checkout_hex)?;
        // Bring the slot's checkout to the sealed subject — reset in place when the
        // slot already holds one, created when it does not.
        materialize_checkout(Path::new("."), spec.worktree_dir, spec.checkout_hex)?;
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
        // worktree, under the ambient local `claude` auth (ADR-0150) — and under
        // the wrapper's environment rather than the coordinator's, which the
        // child would otherwise inherit wholesale and come up configured as a
        // second coordinator (#4714; see `lane_env`).
        let mut lane = self.lane_program.command();
        scrub_coordinator_env(&mut lane, inherited_keys());
        export_build_env(&mut lane, spec);
        lane.current_dir(spec.worktree_dir)
            .args([spec.command, "--out"])
            .arg(spec.evidence_dir)
            .args(["--nonce", spec.nonce]);
        // The diff base is not a model-lane detail: the critic reads it to see
        // a committed candidate, and the mechanical verify lane reads it to
        // narrow its compiling gates to that candidate's reverse-dependency
        // closure (#4890). An order naming none — the whole-bloom aggregate
        // verify, and every stage whose candidate is the working tree — leaves
        // both lanes exactly as they were.
        if let Some(diff_base) = spec.diff_base_hex {
            lane.args(["--diff-base", diff_base]);
        }
        if is_model_lane(spec.command) {
            lane.args(["--subject", spec.checkout_hex]);
            if let Some(harness) = spec.harness {
                lane.args(["--harness", harness]);
            }
            if let Some(model) = spec.model {
                lane.args(["--model", model]);
            }
            if let Some(effort) = spec.effort {
                lane.args(["--effort", effort]);
            }
            if let Some(task) = spec.task {
                lane.args(["--task", task]);
            }
            if let Some(session) = spec.resume {
                lane.args(["--resume", session]);
            }
        }
        let child = lane.spawn().map_err(LocalExecutorError::Spawn)?;
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

    fn registered_worktrees(&self) -> Result<Vec<PathBuf>, LocalExecutorError> {
        // `--porcelain` is the stable machine format: one stanza per worktree, each
        // opening with a `worktree <absolute path>` line. Everything else in the
        // stanza (HEAD, branch, detached, locked, prunable) says nothing about
        // which checkout the path is, so only that line is read.
        Ok(git_in(Path::new("."), &["worktree", "list", "--porcelain"])?
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect())
    }

    fn capture(
        &self,
        worktree_dir: &Path,
        message: Option<&str>,
    ) -> Result<Option<CapturedObjects>, LocalExecutorError> {
        // A clean worktree has nothing to capture — the caller fails the run
        // closed rather than minting an empty candidate.
        if git_in(worktree_dir, &["status", "--porcelain"])?.trim().is_empty() {
            return Ok(None);
        }
        // Normalize formatting before the candidate is minted (#4627). `Verify`
        // runs `cargo fmt -- --check`, so an agent that edits after its last
        // format pass fails a gate whose fix is deterministic and instant — and
        // pays a whole `Refine` re-entry, a retry off its budget, and a receipt
        // whose `actual_retries` then measures formatting discipline rather than
        // the difficulty of the change. Here rather than in the agent's prompt
        // or the xtask arm for the reason the capture path already exists: it is
        // the host's trust domain, downstream of every harness, so one call
        // normalizes every agent identically instead of depending on each one's
        // discipline. It is also the last point before the tree digest, so the
        // formatted tree is what gets attested.
        //
        // Best-effort: formatting is a convenience, never a verdict. A workspace
        // whose fmt fails (a parse error mid-edit, a missing toolchain) still
        // captures and still faces the real gate, which is `Verify`'s job.
        format_worktree(worktree_dir);
        git_in(worktree_dir, &["add", "--all"])?;
        // Commit in the host's trust domain (ADR-0152: the child never stages,
        // commits, or holds credentials), authored as the operator by default
        // (#4630 — see `CaptureIdentity`). `--no-verify` keeps repo hooks out of
        // the capture path; the run's own gates already judged the work.
        let mut commit = self.identity.overrides(worktree_dir);
        commit.extend(["commit", "--no-verify", "--message"].map(str::to_owned));
        commit.push(capture_subject(message).to_owned());
        git_in(worktree_dir, &commit.iter().map(String::as_str).collect::<Vec<_>>())?;
        let commit_hex = git_in(worktree_dir, &["rev-parse", "HEAD"])?;
        #[allow(clippy::literal_string_with_formatting_args, reason = "git revision syntax, not a format string")]
        let tree_hex = git_in(worktree_dir, &["rev-parse", "HEAD^{tree}"])?;
        let commit = decode_object_hex(commit_hex.trim()).ok_or_else(|| {
            LocalExecutorError::Worktree(format!("malformed capture commit sha `{}`", commit_hex.trim()))
        })?;
        let tree = decode_object_hex(tree_hex.trim())
            .ok_or_else(|| LocalExecutorError::Worktree(format!("malformed capture tree sha `{}`", tree_hex.trim())))?;
        Ok(Some(CapturedObjects { commit, tree }))
    }
}

/// Point a lane's build at its slot's own target directory and cap how much of
/// the host it may use doing so (#4912).
///
/// Both ride the child's environment rather than its argv because neither is the
/// lane's to know: the lane spawns `cargo xtask transform`, which spawns the
/// verify gates, which spawn cargo — and every one of them inherits this. That
/// inheritance is the mechanism, not an accident of it: the gate that judges a
/// candidate has to build in the same directory the lane that produced it did, or
/// the fingerprints it reuses are somebody else's.
///
/// The two are also set **after** the coordinator's own configuration is scrubbed
/// out ([`scrub_coordinator_env`]), so a coordinator that inherited a
/// `CARGO_TARGET_DIR` from its boot environment hands the lane the slot's rather
/// than its own.
///
/// A `build_jobs` of zero states no cap, leaving cargo's default of one job per
/// core — an explicit `CARGO_BUILD_JOBS=0` is a cargo error, not "unlimited".
fn export_build_env(lane: &mut Command, spec: &RunSpec<'_>) {
    lane.env("CARGO_TARGET_DIR", spec.target_dir);
    if spec.build_jobs > 0 {
        lane.env("CARGO_BUILD_JOBS", spec.build_jobs.to_string());
    }
}

/// Decode the hex object id `git rev-parse` printed into the opaque bytes the
/// domain correspondence stores.
///
/// This is the Git text boundary of the host capture path, and nothing more: it
/// converts a rendering, it does not judge the object. Which byte lengths name a
/// well-formed Git object (20-byte SHA-1, 32-byte SHA-256) is the adapter's
/// question, so any even length passes here and either hex case is accepted.
/// Empty, odd-length, or non-hex text is `None` — text git never produces, which
/// the caller reports as a malformed sha rather than recording bytes that
/// correspond to no object.
fn decode_object_hex(sha: &str) -> Option<BackendObjectId> {
    let raw = sha.as_bytes();
    if raw.is_empty() || !raw.len().is_multiple_of(2) {
        return None;
    }

    let mut bytes = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks_exact(2) {
        bytes.push((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?);
    }
    Some(BackendObjectId::new(bytes))
}

/// One hex character as its `0..=15` nibble, or `None` for a non-hex byte.
const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Fetch an order's subject when the coordinator does not already hold it (#4643).
///
/// `git worktree add` resolves its commit-ish against the **local** object
/// database only. `Construct` and `Verify` check out the sealed base — a
/// mainline commit this clone already has — so the resolution succeeds and the
/// omission is invisible. `AggregateReview` is the first stage whose subject the
/// coordinator neither produced nor already held: the integration commit is
/// assembled remotely and published as a ref, and without this the checkout
/// fails on an object that is genuinely absent, wedging the bloom one stage
/// short of a landing proposal.
///
/// The fetch names the exact sha rather than a ref namespace — it is precisely
/// what the checkout needs, and the bloom ref namespace grows without bound
/// across runs.
///
/// Unconditional, because `git fetch <remote> <sha>` is already a no-op when the
/// object is present: git satisfies the want locally and never opens a
/// connection. A `cat-file -e` guard in front of it would be unobservable — the
/// same outcome either way — so the common case costs one git process, not a
/// round trip.
fn fetch_subject_if_absent(repo_dir: &Path, checkout_hex: &str) -> Result<(), LocalExecutorError> {
    git_in(repo_dir, &["fetch", "--no-tags", "--quiet", "origin", checkout_hex]).map_err(|error| match error {
        // Name the subject in the failure: a bare "couldn't find remote ref" says
        // nothing about which order could not be materialized.
        LocalExecutorError::Worktree(detail) => {
            LocalExecutorError::Worktree(format!("fetching order subject {checkout_hex}: {detail}"))
        }
        other => other,
    })?;
    Ok(())
}

/// Bring the checkout at `worktree_dir` to `checkout_hex`, reusing the directory
/// when the slot already holds one of this repository's worktrees (#4904).
///
/// Reuse is the point: the path is what makes a lane's compilations cacheable
/// across dispatches, so a slot keeps its checkout instead of tearing it down at
/// the end of every run. Which makes hygiene the load-bearing half — the tree a
/// dispatch starts from is whatever the dispatch before it left, and a lane
/// judges (and the host captures) what it finds in that tree. A leftover file is
/// a candidate committed against work that was never done, and a leftover
/// modification is a verdict about a tree nobody sealed.
///
/// So the reset is total, not tidy: [`reset_checkout`] discards tracked
/// modifications by detaching onto the exact sha, then removes everything git
/// does not track — ignored build output included. A reused slot is
/// indistinguishable from a freshly created one, which is also the fallback: any
/// step that fails leaves the path cleared and re-created outright, so a
/// directory git will not reset cannot wedge every future dispatch that slot
/// takes.
fn materialize_checkout(repo_dir: &Path, worktree_dir: &Path, checkout_hex: &str) -> Result<(), LocalExecutorError> {
    if worktree_dir.exists() {
        match reset_checkout(worktree_dir, checkout_hex) {
            Ok(()) => return Ok(()),
            Err(error) => tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                worktree = %worktree_dir.display(),
                %error,
                "the lane slot's checkout could not be reset; re-creating it from scratch",
            ),
        }
    }
    reclaim_worktree_path(worktree_dir)?;
    add_worktree(repo_dir, worktree_dir, checkout_hex)
}

/// Reset a slot's existing checkout to exactly `checkout_hex`'s tree.
///
/// Three steps, each answering a way the previous dispatch left state behind:
///
/// - the skip-worktree mark [`neutralize_hooks`] set is cleared first, because
///   git deliberately does not update a skip-worktree path on checkout — leaving
///   the mark would carry one dispatch's stripped settings file into every later
///   one. Best-effort: a checkout whose index does not carry the path (the file
///   does not exist at that sha) has nothing to clear and is not a failure.
/// - `checkout --detach --force` moves HEAD onto the exact subject and throws
///   away tracked modifications and index state, including the capture commit a
///   construct lane left on the detached head.
/// - `clean --force --force -d -x` removes what remains: untracked files, nested
///   directories, and ignored ones — the last of which is what a stale `target`
///   tree is. Keeping it would trade the cache win for a build directory shared
///   across divergent source, which is the arrangement the lanes deliberately do
///   not use.
fn reset_checkout(worktree_dir: &Path, checkout_hex: &str) -> Result<(), LocalExecutorError> {
    let _ = git_in(worktree_dir, &["update-index", "--no-skip-worktree", SETTINGS_PATH]);
    git_in(worktree_dir, &["checkout", "--detach", "--force", checkout_hex])?;
    git_in(worktree_dir, &["clean", "--force", "--force", "-d", "-x"])?;
    Ok(())
}

/// Create the slot's checkout at `worktree_dir`, detached at `checkout_hex`.
///
/// `--force` relaxes the two refusals a reused path can still raise: a
/// `<commit-ish>` already checked out by another worktree, and a path assigned to
/// a worktree but missing from disk (what a `reclaim_worktree_path` above leaves).
fn add_worktree(repo_dir: &Path, worktree_dir: &Path, checkout_hex: &str) -> Result<(), LocalExecutorError> {
    let checkout = Command::new("git")
        .current_dir(repo_dir)
        .args(["worktree", "add", "--force", "--detach"])
        .arg(worktree_dir)
        .arg(checkout_hex)
        .output()
        .map_err(LocalExecutorError::Spawn)?;
    if !checkout.status.success() {
        return Err(LocalExecutorError::Worktree(tail(&String::from_utf8_lossy(&checkout.stderr), 1000)));
    }
    Ok(())
}

/// Clear a leftover scratch worktree directory so `git worktree add` cannot
/// refuse the path (#4633).
///
/// `add --force` relaxes exactly two refusals: a `<commit-ish>` already checked
/// out by another worktree, and a path *assigned* to a worktree but missing from
/// disk. A path that exists on disk is refused either way — `fatal: '<path>'
/// already exists`. Any leftover at a slot's path is precisely that, and the
/// path is reused by every dispatch that slot ever takes: without this, one
/// directory git can neither reset nor overwrite would refuse every one of them.
///
/// Removing the directory leaves behind the stale admin entry, which is the half
/// `--force` does handle — so the two together reclaim the path whichever half
/// survived. The slot is held by the dispatch doing the reclaim, so clearing it
/// races nothing.
fn reclaim_worktree_path(worktree_dir: &Path) -> Result<(), LocalExecutorError> {
    match fs::remove_dir_all(worktree_dir) {
        // A slot's first dispatch has no directory to reclaim, which is the
        // common case rather than an error.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(LocalExecutorError::Io(error)),
        Ok(()) => Ok(()),
    }
}

/// Run `cargo fmt` over the scratch worktree, logging rather than failing.
///
/// Deliberately infallible: the candidate's correctness is `Verify`'s call, and
/// a capture that refused over a formatter would turn a convenience into a new
/// way to lose work.
fn format_worktree(worktree_dir: &Path) {
    match Command::new("cargo").current_dir(worktree_dir).arg("fmt").output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            stderr = %tail(&String::from_utf8_lossy(&output.stderr), 500),
            "cargo fmt over the capture worktree exited non-zero; capturing the candidate unformatted",
        ),
        Err(error) => tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            %error,
            "could not spawn cargo fmt over the capture worktree; capturing the candidate unformatted",
        ),
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
    let settings_path = worktree_dir.join(SETTINGS_PATH);
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
        .args(["update-index", "--skip-worktree", SETTINGS_PATH])
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
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::{
        CaptureIdentity, FALLBACK_CAPTURE_SUBJECT, FALLBACK_IDENTITY, ProcessTransformRunner, SETTINGS_PATH,
        TransformRunner, capture_subject, decode_object_hex, fetch_subject_if_absent, git_in, materialize_checkout,
        neutralize_hooks, reclaim_worktree_path, reset_checkout, strip_hooks,
    };

    #[test]
    fn a_captures_subject_is_the_lanes_own_first_line_or_the_literal() {
        assert_eq!(
            capture_subject(Some("feat(crate:aether-text): shelf-pack the atlas\n\nThe glyphs arrive one at a time.")),
            "feat(crate:aether-text): shelf-pack the atlas",
            "the message's first line is the subject; the body is the landing proposal's, not the commit's",
        );
        assert_eq!(capture_subject(None), FALLBACK_CAPTURE_SUBJECT, "a lane that named nothing keeps the literal");
        assert_eq!(
            capture_subject(Some("\n\nbody with no subject")),
            FALLBACK_CAPTURE_SUBJECT,
            "a blank first line names nothing, so the literal stands rather than an empty subject git refuses",
        );
    }

    #[test]
    fn a_capture_commits_under_the_lanes_own_subject() {
        // The end of clause 3, over a real repo: the capture commit's subject is
        // what a reader of the history sees, and it is now the message the model
        // wrote rather than a flat literal every candidate shared. A message left
        // on disk *would* enter this tree, which is why the lane deletes its
        // deliverable before the host stages.
        let repo = repo_with_identity(Some(("operator", "operator@example.test")));
        run_git(repo.path(), &["commit", "--allow-empty", "--quiet", "--message", "base"]);
        fs::write(repo.path().join("candidate.txt"), "the change\n").unwrap();
        let runner = ProcessTransformRunner::default();

        runner
            .capture(repo.path(), Some("perf(crate:aether-mesh): fan-triangulate once\n\nThe mesher re-ran."))
            .unwrap()
            .expect("a dirty worktree captures");

        assert_eq!(
            git_in(repo.path(), &["log", "-1", "--format=%s"]).unwrap().trim(),
            "perf(crate:aether-mesh): fan-triangulate once",
            "the capture commit's subject is the lane's own first line",
        );

        fs::write(repo.path().join("candidate.txt"), "the next change\n").unwrap();
        runner.capture(repo.path(), None).unwrap().expect("a dirty worktree captures");
        assert_eq!(
            git_in(repo.path(), &["log", "-1", "--format=%s"]).unwrap().trim(),
            FALLBACK_CAPTURE_SUBJECT,
            "a lane that named nothing still captures, under the literal",
        );
    }

    // A repo whose *local* identity is set to `identity` — local config outranks
    // whatever the developer's global git config happens to hold, so these cases
    // read the same on any machine. An empty local value is how a host with no
    // committer identity presents: `git config --get` exits zero with no output.
    fn repo_with_identity(identity: Option<(&str, &str)>) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "--quiet"]);
        let (name, email) = identity.unwrap_or(("", ""));
        run_git(dir.path(), &["config", "--local", "user.name", name]);
        run_git(dir.path(), &["config", "--local", "user.email", email]);
        dir
    }

    fn run_git(dir: &Path, args: &[&str]) {
        assert!(Command::new("git").current_dir(dir).args(args).status().unwrap().success(), "git {args:?} failed");
    }

    #[test]
    fn a_configured_identity_is_passed_through_verbatim() {
        let repo = repo_with_identity(Some(("host", "host@example.test")));
        let identity = CaptureIdentity { name: "fleet".to_owned(), email: "fleet@example.test".to_owned() };

        assert_eq!(
            identity.overrides(repo.path()),
            ["-c", "user.name=fleet", "-c", "user.email=fleet@example.test"],
            "explicit knobs win over the host's own identity",
        );
    }

    #[test]
    fn an_unset_identity_defers_to_the_host() {
        // The #4630 default: no overrides at all, so git resolves the ambient
        // identity and the bloom is attributed to whoever runs the coordinator.
        let repo = repo_with_identity(Some(("operator", "operator@example.test")));

        assert!(
            CaptureIdentity::default().overrides(repo.path()).is_empty(),
            "an unset identity must add no -c overrides, or the host's own is never consulted",
        );
    }

    #[test]
    fn an_unset_identity_falls_back_when_the_host_has_none() {
        // Tripwire: without the fallback the capture commit fails outright on a
        // host with no committer identity — after a full model run has already
        // been paid for. Attribution is cosmetic; the candidate is not.
        let repo = repo_with_identity(None);

        let overrides = CaptureIdentity::default().overrides(repo.path());

        assert_eq!(
            overrides,
            [
                "-c".to_owned(),
                format!("user.name={}", FALLBACK_IDENTITY.0),
                "-c".to_owned(),
                format!("user.email={}", FALLBACK_IDENTITY.1),
            ],
            "an identity-less host still commits, under the bloomery's own name",
        );
    }

    // A repo with one real commit and **no** `origin` remote, so a fetch that
    // must reach the network fails predictably.
    fn repo_with_one_commit() -> (TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "--quiet"]);
        run_git(dir.path(), &["config", "--local", "user.name", "test"]);
        run_git(dir.path(), &["config", "--local", "user.email", "test@example.test"]);
        run_git(dir.path(), &["commit", "--quiet", "--allow-empty", "--message", "root"]);
        let head = git_in(dir.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_owned();
        (dir, head)
    }

    #[test]
    fn an_absent_subject_names_itself_in_the_failure() {
        // The bare git error is "couldn't find remote ref …", which says nothing
        // about which order could not be materialized. The dispatch that reports
        // this is one line in a busy log, so it has to carry the subject.
        let (repo, _) = repo_with_one_commit();
        let missing = "0".repeat(40);

        let error = fetch_subject_if_absent(repo.path(), &missing).expect_err("an absent subject cannot be fetched");

        assert!(format!("{error:?}").contains(&missing), "the failure names the subject it could not fetch");
    }

    #[test]
    fn a_leftover_worktree_directory_is_reclaimed() {
        // The #4633 collision: a run killed before teardown leaves a populated
        // directory at the nonce's path, and `git worktree add` refuses it
        // outright — `--force` does not clear a path that exists. Reclaiming has
        // to remove the directory *and its contents*, since the leftover is a
        // full checkout rather than an empty dir.
        let base = tempfile::tempdir().unwrap();
        let worktree = base.path().join("dispatch-1");
        fs::create_dir_all(worktree.join("crates")).unwrap();
        fs::write(worktree.join("crates/leftover.rs"), "// a prior run's checkout").unwrap();

        reclaim_worktree_path(&worktree).unwrap();

        assert!(!worktree.exists(), "a populated leftover worktree must be cleared, not left to collide again");
    }

    #[test]
    fn reclaiming_an_absent_path_is_not_an_error() {
        // Tripwire: the common case is a *first* dispatch at a nonce, where
        // there is nothing to reclaim. Treating `NotFound` as a failure would
        // invert the fix and break every clean dispatch instead of the stale one.
        let base = tempfile::tempdir().unwrap();

        reclaim_worktree_path(&base.path().join("never-created")).unwrap();
    }

    // A repository with two commits over the same tracked file, an ignored
    // build directory, and the interactive settings file a dispatch neutralizes
    // — plus a scratch root outside it to put slot checkouts in. The shape a
    // reused slot has to be reset across.
    fn repo_with_two_commits() -> (TempDir, TempDir, String, String) {
        let repo = repo_with_identity(Some(("test", "test@example.test")));
        fs::create_dir_all(repo.path().join(".claude")).unwrap();
        fs::write(repo.path().join(".gitignore"), "/target\n").unwrap();
        for subject in ["first", "second"] {
            fs::write(repo.path().join("tracked.txt"), format!("{subject}\n")).unwrap();
            fs::write(
                repo.path().join(SETTINGS_PATH),
                format!(r#"{{"hooks": {{"SessionStart": []}}, "model": "{subject}"}}"#),
            )
            .unwrap();
            run_git(repo.path(), &["add", "--all"]);
            run_git(repo.path(), &["commit", "--quiet", "--message", subject]);
        }
        let second = git_in(repo.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_owned();
        let first = git_in(repo.path(), &["rev-parse", "HEAD~1"]).unwrap().trim().to_owned();
        (repo, tempfile::tempdir().unwrap(), first, second)
    }

    #[test]
    fn a_reused_slot_checkout_is_reset_to_the_dispatchs_own_tree() {
        // Tripwire (#4904): a slot's path is reused across dispatches so the
        // compiler cache keeps hitting it, which means the tree a dispatch
        // starts from is whatever the dispatch before it left. Skip the reset
        // and the next lane judges — and the host's capture commits, since it
        // stages `--all` — files no order ever sealed: the stale-state
        // reversion class, once per dispatch. All three leftovers a real run
        // produces are covered, because each survives a different half of the
        // reset: a staged untracked file, a tracked modification, and the
        // ignored build tree.
        let (repo, scratch, first, second) = repo_with_two_commits();
        let slot = scratch.path().join("slot-0");

        materialize_checkout(repo.path(), &slot, &first).unwrap();
        fs::write(slot.join("leftover.txt"), "a previous dispatch's candidate").unwrap();
        fs::write(slot.join("tracked.txt"), "locally modified\n").unwrap();
        fs::create_dir_all(slot.join("target")).unwrap();
        fs::write(slot.join("target/build.log"), "a previous dispatch's build output").unwrap();
        run_git(&slot, &["add", "--all"]);

        materialize_checkout(repo.path(), &slot, &second).unwrap();

        assert_eq!(git_in(&slot, &["rev-parse", "HEAD"]).unwrap().trim(), second, "the slot holds this dispatch's sha");
        assert_eq!(
            fs::read_to_string(slot.join("tracked.txt")).unwrap(),
            "second\n",
            "a tracked file modified by the previous dispatch is restored",
        );
        assert!(!slot.join("leftover.txt").exists(), "the previous dispatch's untracked file is gone");
        assert!(!slot.join("target").exists(), "and so is its ignored build output");
        assert_eq!(
            git_in(&slot, &["status", "--porcelain"]).unwrap().trim(),
            "",
            "a reused slot presents exactly as a freshly created checkout, which is what the lane assumes",
        );
    }

    #[test]
    fn a_reset_clears_the_skip_worktree_mark_the_previous_dispatch_set() {
        // Tripwire: git refuses to update a skip-worktree path, by design, and
        // every dispatch marks the settings file (#3632) — so a reset that does
        // not clear the mark first fails outright, on `Entry
        // '.claude/settings.json' not uptodate`. Nothing above would say so:
        // `materialize_checkout` falls back to re-creating the checkout, which
        // is correct but pays a full checkout on every single dispatch and
        // leaves the reset permanently dead. Hence the assertion against the
        // reset itself rather than through the fallback that hides it.
        let (repo, scratch, first, second) = repo_with_two_commits();
        let slot = scratch.path().join("slot-0");
        materialize_checkout(repo.path(), &slot, &first).unwrap();
        neutralize_hooks(&slot).unwrap();

        reset_checkout(&slot, &second).expect("a slot whose settings file was marked still resets in place");

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(slot.join(SETTINGS_PATH)).unwrap()).unwrap();
        assert_eq!(
            settings.get("model").and_then(serde_json::Value::as_str),
            Some("second"),
            "the settings file is this dispatch's, not the copy the previous dispatch pinned",
        );
    }

    #[test]
    fn a_slot_path_that_is_not_a_checkout_is_recreated_rather_than_wedging_the_slot() {
        // The #4633 collision, made permanent by reuse: the path is the slot's
        // for the life of the host, so a directory git can neither reset nor
        // overwrite — a crash mid-`worktree add`, a half-deleted tree — would
        // refuse not one dispatch's retry but every dispatch that slot ever
        // takes.
        let (repo, scratch, first, _second) = repo_with_two_commits();
        let slot = scratch.path().join("slot-0");
        fs::create_dir_all(slot.join("nested")).unwrap();
        fs::write(slot.join("nested/junk.txt"), "not a checkout").unwrap();

        materialize_checkout(repo.path(), &slot, &first).unwrap();

        assert_eq!(
            fs::read_to_string(slot.join("tracked.txt")).unwrap(),
            "first\n",
            "the slot holds the dispatch's tree rather than refusing it",
        );
        assert!(!slot.join("nested").exists(), "and whatever was in the way is gone");
    }

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

    #[test]
    fn object_hex_decodes_either_case_at_any_even_length() {
        // Tripwire: the decoded bytes are what the candidate digests are taken
        // over and what the correspondence stores, so a nibble swapped, a case
        // rejected, or a length refused silently mints a candidate that resolves
        // to the wrong object. Both real git object formats are covered, plus a
        // third even length the host has no business ruling on.
        assert_eq!(decode_object_hex("00ff").unwrap().as_bytes(), [0x00, 0xff], "lowercase decodes");
        assert_eq!(decode_object_hex("00FF").unwrap().as_bytes(), [0x00, 0xff], "uppercase decodes identically");
        assert_eq!(decode_object_hex("aB3c").unwrap().as_bytes(), [0xab, 0x3c], "mixed case decodes");
        assert_eq!(decode_object_hex(&"a".repeat(40)).unwrap().as_bytes().len(), 20, "a SHA-1 sha is 20 bytes");
        assert_eq!(decode_object_hex(&"a".repeat(64)).unwrap().as_bytes().len(), 32, "a SHA-256 sha is 32 bytes");
        assert_eq!(decode_object_hex(&"a".repeat(24)).unwrap().as_bytes().len(), 12, "another even length passes too");
    }

    #[test]
    fn object_hex_rejects_text_git_never_prints() {
        // The complement: the capture reports these as a malformed sha rather
        // than recording bytes that correspond to no object. `+f` is the case a
        // radix parse would silently accept as 15.
        assert!(decode_object_hex("").is_none(), "empty text names no object");
        assert!(decode_object_hex(&"a".repeat(39)).is_none(), "an odd length is half a byte short");
        assert!(decode_object_hex(&"z".repeat(40)).is_none(), "a non-hex character is refused");
        assert!(decode_object_hex("+f").is_none(), "a sign is not a hex digit");
    }
}
