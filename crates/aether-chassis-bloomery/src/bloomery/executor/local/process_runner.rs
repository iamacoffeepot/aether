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
use aether_bloomery_git::command::{self, GitCommandError};

use super::error::LocalExecutorError;
use super::lane_env::{inherited_keys, scrub_coordinator_env};
use super::lane_program::LaneProgram;
use super::runner::{CapturedObjects, RunLifecycle, RunProcess, RunSpec, TransformRunner};
use super::task_argv;

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
    /// The coordinator repository worktrees are added to. An absolute path —
    /// the local authority, or the process current directory captured at
    /// construction — never `"."`.
    repo: PathBuf,
    /// Where [`fetch_subject_if_absent`] pulls a missing order identity from.
    /// Empty means the GitHub `origin` remote; a local authority stores the
    /// absolute repository path (no `file://` prefix).
    fetch_remote: String,
}

impl ProcessTransformRunner {
    /// Build the runner over the capture identity, lane invocation, and
    /// repository the host resolved.
    #[must_use]
    pub fn new(identity: CaptureIdentity, lane_program: LaneProgram, repo: impl Into<PathBuf>) -> Self {
        let repo = repo.into();
        let repo = repo.canonicalize().unwrap_or(repo);
        Self { identity, lane_program, repo, fetch_remote: String::new() }
    }

    /// Fetch missing order identities from `remote` instead of `origin`.
    ///
    /// A local authority passes its absolute path so a dispatch never depends
    /// on a network remote existing.
    #[must_use]
    pub fn with_fetch_remote(mut self, remote: impl Into<String>) -> Self {
        self.fetch_remote = remote.into();
        self
    }

    fn fetch_remote(&self) -> &str {
        if self.fetch_remote.is_empty() {
            "origin"
        } else {
            &self.fetch_remote
        }
    }
}

impl TransformRunner for ProcessTransformRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
        // Both sealed identities can exist only on the remote (#5057): the
        // checkout fetch does not pull an unrelated comparison base, and
        // `commitish_for` sees the local object database only. Fetch each
        // one that is absent, then resolve — either may still name a bare
        // tree (ADR-0196), which every git consumer past this point refuses.
        // The checkout verbs are guarded inside `materialize_checkout`, but
        // `--diff-base` travels to the lane's verify gates, whose
        // `rev-parse --verify <base>^{commit}` / `merge-base` fail on a
        // tree and wedge the member on verify.preflight (#5026).
        fetch_order_identities(&self.repo, spec.checkout_hex, spec.diff_base_hex, self.fetch_remote())?;
        let checkout = commitish_for(&self.repo, spec.checkout_hex)?;
        let diff_base = spec.diff_base_hex.map(|hex| commitish_for(&self.repo, hex)).transpose()?;
        // Bring the slot's checkout to the sealed subject — reset in place when the
        // slot already holds one, created when it does not.
        materialize_checkout(&self.repo, spec.worktree_dir, &checkout)?;
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
        lane.current_dir(spec.worktree_dir);
        // Command, `--out`, `--nonce`, and the optional `--diff-base` /
        // `--seeded` / model-lane flags. A Construct checkpoint is
        // `--seeded` only (#5052); Verify and Review still name a range.
        // The composed task routes through `task_argv` (#5161): one argv
        // string caps at 128KiB (`E2BIG` past it) and the task is unbounded,
        // so an over-budget task spills to the evidence dir and the argv
        // carries its head plus a pointer line.
        append_work_order_args(&mut lane, spec, &checkout, diff_base.as_deref()).map_err(LocalExecutorError::Io)?;
        // Own process group so a re-attached kill after a coordinator restart
        // can signal the lane *and* the harness it spawned, not just the head
        // pid this process recorded (issue #4999). `process_group(0)` is the
        // child's own pid as pgid; no new dependency.
        let child = spawn_isolated(&mut lane).map_err(LocalExecutorError::Spawn)?;
        super::identity::record_spawned(spec.evidence_dir, child.id());
        Ok(Box::new(ChildProcess { child }))
    }

    fn release(&self, worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        // Tear the scratch worktree back down on the run's terminal path. `--force`
        // discards the run's working-tree changes (the candidate has already been
        // captured and read) and drops the admin entry `git worktree add` registered,
        // so a long-lived backend does not leak one worktree per order.
        let path = worktree_dir.to_string_lossy();
        git_in(&self.repo, &["worktree", "remove", "--force", path.as_ref()]).map(|_| ())
    }

    fn registered_worktrees(&self) -> Result<Vec<PathBuf>, LocalExecutorError> {
        // `--porcelain` is the stable machine format: one stanza per worktree, each
        // opening with a `worktree <absolute path>` line. Everything else in the
        // stanza (HEAD, branch, detached, locked, prunable) says nothing about
        // which checkout the path is, so only that line is read.
        Ok(git_in(&self.repo, &["worktree", "list", "--porcelain"])?
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
        if command::porcelain_entries(worktree_dir).map_err(git_error)?.is_empty() {
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
        Ok(Some(CapturedObjects { commit, tree, diff: capture_diff(worktree_dir) }))
    }
}

/// The capture commit's own diff against the checkout it was built on (#4959) —
/// what the repair-lap triage reads.
///
/// Taken here rather than reconstructed later because this is the one moment the
/// two commits are both local and the worktree is still the lap's: the coordinator
/// holds the capture only as a correspondence row, and a later `git show` would
/// have to resolve it back through that.
///
/// Best-effort, exactly like the `cargo fmt` pass above: the triage passes a lap
/// whose diff it does not hold, so a failure here costs one mechanical check and
/// never a captured candidate. `--no-ext-diff` and `--no-color` keep a
/// developer's own git configuration out of text the host is going to parse.
fn capture_diff(worktree_dir: &Path) -> Option<String> {
    match command::run(worktree_dir, &["diff", "--no-ext-diff", "--no-color", "HEAD~1", "HEAD"]) {
        Ok(output) if output.status.success() => Some(String::from_utf8_lossy(&output.stdout).into_owned()),
        Ok(output) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                stderr = %tail(&String::from_utf8_lossy(&output.stderr), 500),
                "could not read the capture commit's diff; this lap's repair will not be triaged",
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                %error,
                "could not read the capture commit's diff; this lap's repair will not be triaged",
            );
            None
        }
    }
}

/// Append the work-order flags `cargo xtask transform` reads.
fn append_work_order_args(
    lane: &mut Command,
    spec: &RunSpec<'_>,
    checkout: &str,
    diff_base: Option<&str>,
) -> io::Result<()> {
    for arg in work_order_args(spec, checkout, diff_base)? {
        lane.arg(arg);
    }
    Ok(())
}

/// The argv tail after the lane program: command, `--out`, `--nonce`, and the
/// optional range / model / seeded flags.
///
/// A Construct checkpoint is `--seeded <checkout>` and never `--diff-base`
/// (#5052): the marker on the work order is provenance, not a range the
/// lane judges. A clean Construct emits neither.
fn work_order_args(spec: &RunSpec<'_>, checkout: &str, diff_base: Option<&str>) -> io::Result<Vec<String>> {
    let mut args = vec![spec.command.to_owned()];
    task_argv::push_value_flag(&mut args, "--out", spec.evidence_dir.display().to_string());
    task_argv::push_value_flag(&mut args, "--nonce", spec.nonce);
    if let Some(diff_base) = diff_base {
        task_argv::push_value_flag(&mut args, "--diff-base", diff_base);
    }
    if is_model_lane(spec.command) {
        task_argv::push_value_flag(&mut args, "--subject", checkout);
        if let Some(harness) = spec.harness {
            task_argv::push_value_flag(&mut args, "--harness", harness);
        }
        if let Some(model) = spec.model {
            task_argv::push_value_flag(&mut args, "--model", model);
        }
        if let Some(effort) = spec.effort {
            task_argv::push_value_flag(&mut args, "--effort", effort);
        }
        if let Some(task) = spec.task {
            task_argv::push_value_flag(
                &mut args,
                "--task",
                task_argv::argv_safe_task(task, spec.evidence_dir)?.into_owned(),
            );
        }
        if let Some(session) = spec.resume {
            task_argv::push_value_flag(&mut args, "--resume", session);
        }
        if spec.seeded.is_some() {
            task_argv::push_value_flag(&mut args, "--seeded", checkout);
        }
    }
    Ok(args)
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
/// question, so any even length of lowercase hex passes here.
/// Empty, odd-length, uppercase, or non-hex text is `None` — text git never produces, which
/// the caller reports as a malformed sha rather than recording bytes that
/// correspond to no object.
fn decode_object_hex(sha: &str) -> Option<BackendObjectId> {
    if sha.is_empty() {
        return None;
    }
    aether_bloomery::decode_hex(sha).map(BackendObjectId::new)
}

/// Fetch each sealed order identity the coordinator does not already hold.
///
/// Checkout and the comparison base are independent remotes-only commits
/// (#5057): fetching the subject does not make the base local, and the
/// resolve that follows ([`commitish_for`]) only sees the local object
/// database.
fn fetch_order_identities(
    repo_dir: &Path,
    checkout_hex: &str,
    diff_base_hex: Option<&str>,
    remote: &str,
) -> Result<(), LocalExecutorError> {
    fetch_subject_if_absent(repo_dir, checkout_hex, remote)?;
    if let Some(hex) = diff_base_hex {
        fetch_subject_if_absent(repo_dir, hex, remote)?;
    }
    Ok(())
}

/// Fetch an order identity when the coordinator does not already hold it
/// (#4643, #5057).
///
/// `git worktree add` and `git cat-file` resolve against the **local** object
/// database only. `Construct` and `Verify` check out a sealed identity this
/// clone already has — so the resolution succeeds and the omission is
/// invisible. `AggregateReview` is the first stage whose subject the
/// coordinator neither produced nor already held: the integration commit is
/// assembled remotely and published as a ref. An observed mainline advance
/// can name the same kind of gap for the comparison base: a valid dispatch
/// then retries `cat-file` on an object that is genuinely absent.
///
/// The fetch names the exact sha rather than a ref namespace — ADR-0152
/// requires the sealed identity, not a branch tip, and the bloom ref
/// namespace grows without bound across runs.
///
/// Already-local objects skip the fetch: `git fetch <remote> <sha>` still
/// needs the remote to exist even when git would satisfy the want from the
/// local database, and a dispatch whose identities this clone already holds
/// must not depend on the remote being reachable. `remote` is `origin` when
/// GitHub is authoritative, or an absolute repository path (no `file://`)
/// when the fleet-local authority holds the objects.
fn fetch_subject_if_absent(repo_dir: &Path, hex: &str, remote: &str) -> Result<(), LocalExecutorError> {
    if git_in(repo_dir, &["cat-file", "-e", hex]).is_ok() {
        return Ok(());
    }
    // The coordinator repository *is* the authority: a missing object is
    // not on another host, so reaching for a network remote would be a lie.
    if command::shares_object_database(repo_dir, Path::new(remote)) {
        return Err(LocalExecutorError::Worktree(format!(
            "fetching order subject {hex}: object is not in {}",
            repo_dir.display()
        )));
    }
    git_in(repo_dir, &["fetch", "--no-tags", "--quiet", remote, hex]).map_err(|error| match error {
        // Name the identity in the failure: a bare "couldn't find remote ref" says
        // nothing about which order could not be materialized.
        LocalExecutorError::Worktree(detail) => {
            LocalExecutorError::Worktree(format!("fetching order subject {hex}: {detail}"))
        }
        other => other,
    })?;
    Ok(())
}

/// Canonical path of `repo`'s object database. Alias for the command-layer
/// predicate so comments and tests keep the name they already use.
fn resolved_git_common_dir(repo: &Path) -> Option<PathBuf> {
    command::git_common_dir(repo)
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
///
/// Reuse is also ownership-sensitive (#5167). [`reset_checkout`] runs git
/// inside the slot, so it resolves against whatever repository the slot was
/// created from, not `repo_dir`. A slot whose [`resolved_git_common_dir`] is
/// not this repository's is reclaimed and re-created from `repo_dir` — the
/// same path as an absent directory — rather than reset against the wrong
/// object database.
///
/// The subject may name a bare tree rather than a commit — a spliced
/// dependency claim that resolved without a capture commit (ADR-0196) — so
/// [`commitish_for`] resolves it first; both the reset and the re-create
/// paths then stand on a subject git accepts.
fn materialize_checkout(repo_dir: &Path, worktree_dir: &Path, checkout_hex: &str) -> Result<(), LocalExecutorError> {
    let commitish = commitish_for(repo_dir, checkout_hex)?;
    if worktree_dir.exists() && slot_belongs_to_repo(worktree_dir, repo_dir) {
        match reset_checkout(worktree_dir, &commitish) {
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
    add_worktree(repo_dir, worktree_dir, &commitish)
}

/// Whether the slot at `worktree_dir` is a worktree of `repo_dir`.
///
/// Compared by canonical common-dir, not by path string: a linked worktree
/// reports the parent clone, a bare `repo_dir` reports itself, and those two
/// renderings only agree after [`resolved_git_common_dir`] joins and
/// canonicalizes. A slot that is not a checkout, or whose common dir cannot
/// be resolved, is treated as foreign so the from-scratch fallback runs.
fn slot_belongs_to_repo(worktree_dir: &Path, repo_dir: &Path) -> bool {
    match (resolved_git_common_dir(worktree_dir), resolved_git_common_dir(repo_dir)) {
        (Some(slot), Some(repo)) => slot == repo,
        _ => false,
    }
}

/// A subject git's checkout verbs accept: the order's own hex when it names a
/// commit, or a deterministic wrapper commit when it names a bare tree.
///
/// A dependent member's spliced construct base is its dependency claim's
/// candidate digest, and a claim that resolved without a capture commit — the
/// test door that jumps straight to Integrate — names a *tree* (ADR-0196),
/// which `git checkout` and `git worktree add` both refuse. The wrapper is
/// authored under [`WRAPPER_IDENTITY`]'s fixed name and epoch timestamp, so
/// every materialization of one tree names one commit: the journaled dispatch
/// replays onto the same wrapper sha instead of minting a fresh parentless
/// commit per attempt.
fn commitish_for(repo_dir: &Path, checkout_hex: &str) -> Result<String, LocalExecutorError> {
    if git_in(repo_dir, &["cat-file", "-t", checkout_hex])?.trim() != "tree" {
        return Ok(checkout_hex.to_owned());
    }
    let message = format!("bloomery: checkout of bare tree {checkout_hex}");
    let wrap = command::run_env(repo_dir, &["commit-tree", checkout_hex, "-m", &message], &WRAPPER_IDENTITY)
        .map_err(git_error)?;
    if !wrap.status.success() {
        return Err(LocalExecutorError::Worktree(tail(&String::from_utf8_lossy(&wrap.stderr), 1000)));
    }
    Ok(command::trim_bytes(&wrap.stdout))
}

/// The fixed author and committer a tree wrapper commit carries, timestamp
/// included — what keeps every wrap of one tree naming the same commit sha.
const WRAPPER_IDENTITY: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "bloomery"),
    ("GIT_AUTHOR_EMAIL", "bloomery@localhost"),
    ("GIT_AUTHOR_DATE", "1970-01-01T00:00:00Z"),
    ("GIT_COMMITTER_NAME", "bloomery"),
    ("GIT_COMMITTER_EMAIL", "bloomery@localhost"),
    ("GIT_COMMITTER_DATE", "1970-01-01T00:00:00Z"),
];

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
    let path = worktree_dir.to_string_lossy();
    git_in(repo_dir, &["worktree", "add", "--force", "--detach", path.as_ref(), checkout_hex]).map(|_| ())
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

// Run one git command inside `dir`, returning its trimmed stdout — domain
// error mapping over the crate-wide command layer.
fn git_in(dir: &Path, args: &[&str]) -> Result<String, LocalExecutorError> {
    command::run_ok(dir, args).map_err(git_error)
}

fn git_error(error: GitCommandError) -> LocalExecutorError {
    match error {
        GitCommandError::Spawn { source, .. } => LocalExecutorError::Spawn(source),
        GitCommandError::Failed { stderr, .. } => LocalExecutorError::Worktree(tail(&stderr, 1000)),
        GitCommandError::Encoding => LocalExecutorError::Worktree("git produced non-UTF-8 output".into()),
    }
}

/// Spawn `command` in its own process group on Unix, so a later re-attached
/// kill can signal the whole lane (head + harness grandchildren) rather than
/// the recorded pid alone.
fn spawn_isolated(command: &mut Command) -> io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn()
}

/// A live `cargo xtask transform` child.
struct ChildProcess {
    child: Child,
}

impl RunProcess for ChildProcess {
    fn poll(&mut self) -> RunLifecycle {
        // A wait fault is not a live run; it is an observation fault, not a
        // clean nonzero exit and not a signal — the backend has to tell those
        // three apart (ADR-0195 §2).
        RunLifecycle::from_try_wait(self.child.try_wait())
    }

    fn kill(&mut self) -> Result<(), LocalExecutorError> {
        // The spawn put this child in its own process group so teardown can
        // signal the lane *and* the harness grandchildren it forked. Signalling
        // the head pid alone leaves those grandchildren reparented to init.
        #[cfg(unix)]
        if let Some(identity) = super::identity::ProcessIdentity::observe(self.child.id()) {
            identity.terminate_group()?;
            self.child.wait().map_err(LocalExecutorError::Io)?;
            return Ok(());
        }
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
    git_in(worktree_dir, &["update-index", "--skip-worktree", SETTINGS_PATH]).map(|_| ())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    #[cfg(target_os = "linux")]
    use std::process::Stdio;
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::Duration;
    #[cfg(target_os = "linux")]
    use std::time::Instant;

    use tempfile::TempDir;

    use super::super::lane_program::LaneProgram;
    #[cfg(target_os = "linux")]
    use super::super::runner::RunProcess;
    use super::super::runner::{RunLifecycle, RunSpec};
    use super::{
        CaptureIdentity, FALLBACK_CAPTURE_SUBJECT, FALLBACK_IDENTITY, ProcessTransformRunner, SETTINGS_PATH,
        TransformRunner, capture_subject, commitish_for, decode_object_hex, fetch_order_identities,
        fetch_subject_if_absent, git_in, materialize_checkout, neutralize_hooks, reclaim_worktree_path, reset_checkout,
        resolved_git_common_dir, strip_hooks, work_order_args,
    };

    #[test]
    fn a_wait_fault_is_an_observation_fault_not_a_clean_failure() {
        // Tripwire (ADR-0195 §2): collapsing `try_wait` Err into
        // `Exited { success: false }` made a runner fault indistinguishable
        // from a lane that judged the subject and exited 1.
        let lifecycle = RunLifecycle::from_try_wait(Err(io::Error::other("waitid failed")));
        assert_eq!(lifecycle, RunLifecycle::ObservationFault);
        assert!(lifecycle.is_terminal());
        assert!(!lifecycle.clean_success());
    }

    // The plausible bug: `--seeded` is omitted on a checkpoint Construct, or
    // emitted on a clean start, or the Construct provenance marker is
    // forwarded as `--diff-base` and the prompt either stays silent or
    // treats an untrusted tree as a committed range (#5052).
    #[test]
    fn a_seeded_construct_emits_seeded_and_a_clean_one_does_not() {
        let evidence = Path::new("/tmp/evidence");
        let worktree = Path::new("/tmp/slot");
        let target = Path::new("/tmp/target");
        let checkout = "abc123def456";

        let clean = work_order_args(
            &spec("construct.implement", checkout, None, None, evidence, worktree, target),
            checkout,
            None,
        )
        .expect("work-order args assemble");
        assert!(!clean.iter().any(|arg| arg == "--seeded"), "a clean Construct must not name a checkpoint: {clean:?}");
        assert!(
            !clean.iter().any(|arg| arg == "--diff-base"),
            "Construct never forwards provenance as --diff-base: {clean:?}"
        );

        let seeded = work_order_args(
            &spec("construct.implement", checkout, None, Some(checkout), evidence, worktree, target),
            checkout,
            None,
        )
        .expect("work-order args assemble");
        let seeded_at =
            seeded.iter().position(|arg| arg == "--seeded").expect("a checkpoint Construct must emit --seeded");
        assert_eq!(
            seeded.get(seeded_at + 1).map(String::as_str),
            Some(checkout),
            "the prompt names the checkout it started from"
        );
        assert!(
            !seeded.iter().any(|arg| arg == "--diff-base"),
            "the Construct marker must not become --diff-base: {seeded:?}"
        );

        let verify = work_order_args(
            &spec("verify.check", checkout, Some("base000"), None, evidence, worktree, target),
            checkout,
            Some("base000"),
        )
        .expect("work-order args assemble");
        assert!(
            verify.windows(2).any(|pair| pair == ["--diff-base", "base000"]),
            "verify still names its range: {verify:?}"
        );
        assert!(!verify.iter().any(|arg| arg == "--seeded"), "verify is not a construct checkpoint: {verify:?}");
    }

    fn spec<'a>(
        command: &'a str,
        checkout_hex: &'a str,
        diff_base_hex: Option<&'a str>,
        seeded: Option<&'a str>,
        evidence_dir: &'a Path,
        worktree_dir: &'a Path,
        target_dir: &'a Path,
    ) -> RunSpec<'a> {
        RunSpec {
            command,
            checkout_hex,
            diff_base_hex,
            seeded,
            worktree_dir,
            target_dir,
            build_jobs: 1,
            evidence_dir,
            nonce: "n-1",
            harness: None,
            model: None,
            effort: None,
            task: None,
            resume: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn try_wait_keeps_a_clean_exit_and_a_terminating_signal_apart() {
        // The process runner used to fold both through `status.success()`, so
        // a SIGKILL'd child and `exit(1)` arrived at the backend as the same
        // boolean. The first is a host observation; the second may still carry
        // a valid authored verdict.
        let mut clean = Command::new("true").spawn().unwrap();
        let clean_lifecycle = loop {
            match RunLifecycle::from_try_wait(clean.try_wait()) {
                RunLifecycle::Running => thread::sleep(Duration::from_millis(5)),
                other => break other,
            }
        };
        assert_eq!(clean_lifecycle, RunLifecycle::Exited { success: true });

        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        child.kill().unwrap();
        let signaled = loop {
            match RunLifecycle::from_try_wait(child.try_wait()) {
                RunLifecycle::Running => thread::sleep(Duration::from_millis(5)),
                other => break other,
            }
        };
        assert_eq!(signaled, RunLifecycle::Signaled { signal: libc_sigkill() });
    }

    #[cfg(unix)]
    fn libc_sigkill() -> i32 {
        // libc is not a crate dependency; SIGKILL is 9 on every Unix this
        // coordinator runs on, and `Child::kill` sends exactly that.
        9
    }

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

        let error =
            fetch_subject_if_absent(repo.path(), &missing, "origin").expect_err("an absent subject cannot be fetched");

        assert!(format!("{error:?}").contains(&missing), "the failure names the subject it could not fetch");
    }

    #[test]
    fn a_remote_only_diff_base_is_fetched_and_resolved_exactly() {
        // Tripwire (#5057): start fetched the checkout then resolved the
        // comparison base with `cat-file` immediately. An observed mainline
        // that exists only on origin — the base, not the checkout — made
        // every dispatch retry that cat-file before the lane launched. The
        // fetch has to name the exact sealed sha: substituting a branch tip
        // would compare against whatever origin advanced to, not the
        // identity the order sealed.
        let (origin, checkout) = repo_with_one_commit();
        let clone = tempfile::tempdir().unwrap();
        run_git(clone.path(), &["clone", "--quiet", origin.path().to_str().unwrap(), "."]);
        run_git(origin.path(), &["commit", "--quiet", "--allow-empty", "--message", "advance"]);
        let diff_base = git_in(origin.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_owned();
        assert!(
            git_in(clone.path(), &["cat-file", "-e", &diff_base]).is_err(),
            "the comparison base must start remote-only, or the fetch is never exercised",
        );

        fetch_order_identities(clone.path(), &checkout, Some(&diff_base), "origin")
            .expect("a remote-only base is fetched before resolution");

        assert_eq!(
            commitish_for(clone.path(), &diff_base).expect("the fetched base must resolve"),
            diff_base,
            "the exact sealed identity, not a branch tip origin advanced to",
        );
        assert_eq!(
            commitish_for(clone.path(), &checkout).expect("the local checkout still resolves"),
            checkout,
            "fetching the base must not substitute a different checkout",
        );
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

        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&slot).unwrap().ino()
        };

        materialize_checkout(repo.path(), &slot, &second).unwrap();

        assert_eq!(
            resolved_git_common_dir(&slot),
            resolved_git_common_dir(repo.path()),
            "a same-repository slot stays a worktree of this repository",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(&slot).unwrap().ino(),
                inode,
                "reuse resets the existing checkout rather than tearing the directory down",
            );
        }
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
    fn a_slot_backed_by_another_repository_is_recreated_from_repo_dir() {
        // Tripwire (#5167): reset_checkout runs git inside the slot, so a slot
        // left as a worktree of another repository still resets when the sealed
        // subject exists in both histories, and the capture commit lands in the
        // wrong object database. After the ADR-0199 authority cutover that was
        // the pre-cutover clone vs the fleet authority — Verify then
        // retry-looped on a subject the authority did not hold, with no wedge.
        let (root, authority, stale, head) = authority_and_clone("authority.git");
        let slot = root.path().join("slot-0");

        materialize_checkout(&stale, &slot, &head).unwrap();
        assert_eq!(
            resolved_git_common_dir(&slot),
            resolved_git_common_dir(&stale),
            "precondition: the slot starts as a worktree of the other repository",
        );

        materialize_checkout(&authority, &slot, &head).unwrap();
        assert_eq!(
            resolved_git_common_dir(&slot),
            resolved_git_common_dir(&authority),
            "the slot is re-created as a worktree of repo_dir",
        );
        assert_eq!(
            git_in(&slot, &["rev-parse", "HEAD"]).unwrap().trim(),
            head,
            "the re-created slot stands on the shared subject",
        );

        fs::write(slot.join("candidate.txt"), "the change\n").unwrap();
        let runner = ProcessTransformRunner::new(
            CaptureIdentity { name: "test".to_owned(), email: "test@example.test".to_owned() },
            LaneProgram::default(),
            &authority,
        );
        runner.capture(&slot, Some("fix: candidate")).unwrap().expect("a dirty worktree captures");
        let captured = git_in(&slot, &["rev-parse", "HEAD"]).unwrap();
        let captured = captured.trim();

        git_in(&authority, &["cat-file", "-e", captured]).expect("the capture must land in repo_dir's object database");
        assert!(
            git_in(&stale, &["cat-file", "-e", captured]).is_err(),
            "a capture in the re-created slot must not write into the repository the slot used to belong to",
        );
    }

    #[test]
    fn a_current_dependency_checkout_stays_on_the_supplied_ancestral_commit() {
        // The plausible bug (#5079): a dependent construct whose splice named
        // the dependency's capture commit still ran it through the bare-tree
        // wrapper, so git saw a parentless epoch instead of the real
        // ancestry and a clean patch entered Reconcile. A current captured
        // candidate is already a commit; `commitish_for` must return it
        // unchanged and materialize it with its parent intact.
        let (repo, scratch, first, second) = repo_with_two_commits();
        let slot = scratch.path().join("slot-0");

        assert_eq!(
            commitish_for(repo.path(), &second).expect("a capture commit resolves as itself"),
            second,
            "a current captured candidate must not enter the bare-tree wrapper",
        );

        materialize_checkout(repo.path(), &slot, &second).unwrap();
        assert_eq!(
            git_in(&slot, &["rev-parse", "HEAD"]).unwrap().trim(),
            second,
            "the slot stands on the supplied ancestral commit",
        );
        assert_eq!(
            git_in(&slot, &["rev-parse", "HEAD^"]).unwrap().trim(),
            first,
            "the capture keeps its real parent; a wrapper would be parentless",
        );
    }

    #[test]
    fn a_bare_tree_subject_checks_out_under_one_deterministic_wrapper_commit() {
        // Tripwire (ADR-0196 splice basing): a dependency claim that resolved
        // without a capture commit names its candidate by *tree*, and both
        // checkout verbs refuse a tree — the dispatch wedges and re-drives
        // forever, stalling the whole ack prefix behind it. The wrapper must
        // also be deterministic: the failing order is journaled, so every
        // replay wraps the same tree and has to land on the same commit sha
        // rather than minting a fresh parentless commit per attempt.
        let (repo, scratch, first, second) = repo_with_two_commits();
        let tree = git_in(repo.path(), &["show", "-s", "--format=%T", &second]).unwrap().trim().to_owned();
        let slot = scratch.path().join("slot-0");

        materialize_checkout(repo.path(), &slot, &tree).unwrap();
        assert_eq!(
            git_in(&slot, &["show", "-s", "--format=%T", "HEAD"]).unwrap().trim(),
            tree,
            "the slot stands on exactly the ordered tree",
        );

        let wrapper = git_in(&slot, &["rev-parse", "HEAD"]).unwrap();
        materialize_checkout(repo.path(), &slot, &first).unwrap();
        materialize_checkout(repo.path(), &slot, &tree).unwrap();
        assert_eq!(
            git_in(&slot, &["rev-parse", "HEAD"]).unwrap(),
            wrapper,
            "re-driving the journaled order wraps the same tree into the same commit",
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
    fn object_hex_decodes_lowercase_at_any_even_length() {
        // Tripwire: the decoded bytes are what the candidate digests are taken
        // over and what the correspondence stores, so a nibble swapped, a case
        // accepted, or a length refused silently mints a candidate that resolves
        // to the wrong object. Both real git object formats are covered, plus a
        // third even length the host has no business ruling on.
        assert_eq!(decode_object_hex("00ff").unwrap().as_bytes(), [0x00, 0xff], "lowercase decodes");
        assert!(decode_object_hex("00FF").is_none(), "uppercase is refused");
        assert!(decode_object_hex("aB3c").is_none(), "mixed case is refused");
        assert_eq!(decode_object_hex(&"a".repeat(40)).unwrap().as_bytes().len(), 20, "a SHA-1 sha is 20 bytes");
        assert_eq!(decode_object_hex(&"a".repeat(64)).unwrap().as_bytes().len(), 32, "a SHA-256 sha is 32 bytes");
        assert_eq!(decode_object_hex(&"a".repeat(24)).unwrap().as_bytes().len(), 12, "another even length passes too");
    }

    // Live identity observation reads `/proc/<pid>/stat`. A host without that
    // filesystem cannot record pgid, so this tripwire is Linux-only — the same
    // bound as `ProcessIdentity::observe`.
    #[cfg(target_os = "linux")]
    #[test]
    fn spawn_isolated_makes_the_child_its_own_process_group_leader() {
        // Tripwire: a re-attached kill signals the group. If spawn forgets
        // `process_group(0)`, the child stays in the coordinator's group and
        // the recorded pgid is not a group this process can isolate —
        // grandchildren survive the head's death, which is the expensive half
        // of a leaked lane.
        let mut child = super::spawn_isolated(Command::new("sleep").arg("30")).unwrap();
        let identity = super::super::identity::ProcessIdentity::observe(child.id()).expect("the child is live");
        assert_eq!(identity.pid, child.id());
        assert_eq!(identity.pgid, child.id(), "process_group(0) makes the child its own group leader");
        let _ = child.kill();
        let _ = child.wait();
    }

    // Live identity observation and group membership read `/proc`. Off Linux
    // those return none, the grandchild never appears, and the sleeps leak.
    #[cfg(target_os = "linux")]
    struct GroupGuard(u32);

    #[cfg(target_os = "linux")]
    impl Drop for GroupGuard {
        fn drop(&mut self) {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", self.0)])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn killing_a_lane_child_terminates_its_grandchildren() {
        // Tripwire: spawn isolates the lane in its own process group, but
        // teardown used to signal only the head pid. A harness grandchild
        // inherits the group and survives `Child::kill`, reparented to init.
        let child = super::spawn_isolated(Command::new("sh").args(["-c", "sleep 60 & exec sleep 60"])).unwrap();
        let identity = super::super::identity::ProcessIdentity::observe(child.id()).expect("the head is live");
        let pgid = identity.pgid;
        let _guard = GroupGuard(pgid);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut members = 0;
        while Instant::now() < deadline {
            members = count_group(pgid);
            if members >= 2 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(members >= 2, "the head must have spawned a grandchild in its group, found {members}");

        let mut process = super::ChildProcess { child };
        process.kill().expect("group terminate reports success");
        assert_eq!(count_group(pgid), 0, "no member of the lane group survives teardown");
    }

    #[cfg(target_os = "linux")]
    fn count_group(pgid: u32) -> usize {
        fs::read_dir("/proc")
            .expect("/proc")
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u32>().ok())
                    .and_then(super::super::identity::ProcessIdentity::observe)
                    .is_some_and(|live| live.pgid == pgid)
            })
            .count()
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

    /// A bare authority at `name` (which may contain spaces) plus a clone of
    /// it. The clone has no `origin` remote, so a fetch has to name the
    /// authority by path.
    fn authority_and_clone(name: &str) -> (TempDir, PathBuf, PathBuf, String) {
        let root = tempfile::tempdir().unwrap();
        let seed = root.path().join("seed");
        fs::create_dir(&seed).unwrap();
        run_git(&seed, &["init", "--quiet", "-b", "main"]);
        run_git(&seed, &["config", "--local", "user.name", "test"]);
        run_git(&seed, &["config", "--local", "user.email", "test@example.test"]);
        run_git(&seed, &["commit", "--quiet", "--allow-empty", "--message", "root"]);
        let head = git_in(&seed, &["rev-parse", "HEAD"]).unwrap().trim().to_owned();

        let authority = root.path().join(name);
        assert!(
            Command::new("git")
                .args(["clone", "--bare", "--quiet"])
                .arg(&seed)
                .arg(&authority)
                .status()
                .unwrap()
                .success(),
            "clone --bare into {authority:?}"
        );

        let clone = root.path().join("clone");
        assert!(
            Command::new("git").args(["clone", "--quiet"]).arg(&authority).arg(&clone).status().unwrap().success(),
            "clone from {authority:?} without a file:// prefix"
        );
        run_git(&clone, &["remote", "remove", "origin"]);
        (root, authority, clone, head)
    }

    #[test]
    fn a_local_authority_path_with_spaces_supplies_a_missing_object() {
        // Tripwire (ADR-0199): fetch names the authority as an absolute
        // filesystem path. A `file://` prefix is not required, and a space in
        // the path is not a reason to invent one — `git fetch` takes the
        // path as an argv element.
        let (_root, authority, clone, checkout) = authority_and_clone("authority with spaces.git");
        let tree = git_in(&authority, &["rev-parse", &format!("{checkout}^{{tree}}")]).unwrap();
        let advanced = {
            let output = Command::new("git")
                .current_dir(&authority)
                .args(["commit-tree", tree.trim(), "-p", &checkout, "-m", "advance"])
                .envs([
                    ("GIT_AUTHOR_NAME", "test"),
                    ("GIT_AUTHOR_EMAIL", "test@example.test"),
                    ("GIT_COMMITTER_NAME", "test"),
                    ("GIT_COMMITTER_EMAIL", "test@example.test"),
                ])
                .output()
                .unwrap();
            assert!(output.status.success(), "commit-tree: {}", String::from_utf8_lossy(&output.stderr));
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        run_git(&authority, &["update-ref", "refs/heads/main", &advanced]);
        assert!(
            git_in(&clone, &["cat-file", "-e", &advanced]).is_err(),
            "the advance must start absent from the clone, or the fetch is never exercised",
        );

        fetch_subject_if_absent(&clone, &advanced, authority.to_str().expect("utf-8 path"))
            .expect("an absolute authority path with spaces is a valid fetch remote");

        assert_eq!(
            commitish_for(&clone, &advanced).expect("the fetched subject must resolve"),
            advanced,
            "the exact sealed identity arrived through the path remote",
        );
    }

    #[test]
    fn a_repo_path_with_spaces_materializes_a_worktree() {
        let (root, authority, _clone, head) = authority_and_clone("repo with spaces.git");
        let slot = root.path().join("slot-0");

        materialize_checkout(&authority, &slot, &head).expect("git worktree add from a spaced bare path");

        assert_eq!(
            git_in(&slot, &["rev-parse", "HEAD"]).unwrap().trim(),
            head,
            "the slot stands on the authority's subject",
        );
    }

    #[test]
    fn registered_worktrees_read_the_configured_repo_not_the_process_cwd() {
        // The plausible bug: listing still shells `git worktree list` against
        // `"."`, so a coordinator whose cwd is not the authority reports the
        // wrong set (or the developer's own checkout).
        let (root, authority, _clone, head) = authority_and_clone("authority.git");
        let slot = root.path().join("slot-0");
        materialize_checkout(&authority, &slot, &head).unwrap();

        let runner = ProcessTransformRunner::new(CaptureIdentity::default(), LaneProgram::default(), &authority);
        let listed = runner.registered_worktrees().expect("list the authority's worktrees");

        assert!(
            listed.iter().any(|path| path == &slot),
            "the configured repository's worktree is listed, not whatever cwd happens to be: {listed:?}"
        );
    }

    #[test]
    fn an_absent_subject_in_the_authority_itself_does_not_fetch_origin() {
        // When the runner *is* the local authority, a missing object is not
        // on a network remote. Reaching for `origin` would be the old cwd
        // assumption leaking back in.
        let (_root, authority, _clone, _head) = authority_and_clone("authority.git");
        let missing = "0".repeat(40);

        let error = fetch_subject_if_absent(&authority, &missing, authority.to_str().expect("utf-8 path"))
            .expect_err("a self-authority miss is not a network fetch");

        let detail = format!("{error:?}");
        assert!(detail.contains(&missing), "the failure names the subject: {detail}");
        assert!(!detail.contains("origin"), "a local authority must not name the GitHub remote: {detail}");
    }
}
