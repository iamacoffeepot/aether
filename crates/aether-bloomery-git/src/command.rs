//! One place that spawns `git` and classifies what came back.
//!
//! Chassis, xtask bloom helpers, and this crate's backends all go through this
//! module so load-bearing flags (`--no-renames -z` on a name-only diff, `-z` on
//! a porcelain status parse) cannot drift per call site.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::client::{GitCommit, GitDataError};

/// Who a locally minted commit is authored by. The bloomery is its own author,
/// at an address that resolves: the roll keeps the author side verbatim when it
/// linearizes the day onto main, so this is what a reader of landed history
/// sees, and an `.invalid` domain renders there as an unattributed commit.
pub const BLOOMERY_AUTHOR: (&str, &str) = ("bloomery", "bloomery@iamateapot.dev");

/// Who a locally minted commit is committed by, timestamp included. Pinned, and
/// never seen: the roll rewrites the committer side onto the operator identity
/// and the real moment of the linearization, so this value reaches no landed
/// commit. Holding it fixed is half of what keeps a re-mint byte-identical.
pub const BLOOMERY_COMMITTER: [(&str, &str); 3] = [
    ("GIT_COMMITTER_NAME", "bloomery"),
    ("GIT_COMMITTER_EMAIL", "bloomery@aether.invalid"),
    ("GIT_COMMITTER_DATE", "@0 +0000"),
];

/// The oldest git that honours the `attr.tree` source `merge()` depends on.
/// Fail boot below this rather than paying for the driver's absence one fold
/// at a time.
pub const MIN_GIT: (u32, u32, u32) = (2, 40, 0);

/// Flags every name-only diff that feeds closure or containment logic carries.
///
/// `--no-renames` keeps a file moved between crates attributed to both of them:
/// rename detection reports only the destination, and the crate the file left
/// is exactly the one whose build the move can have broken. `-z` makes the
/// listing a NUL split rather than a parse — git quotes unusual bytes in every
/// other format. `--no-ext-diff` keeps a developer's external diff driver out
/// of a path list the host is going to match against package prefixes.
pub const NAME_ONLY_DIFF_FLAGS: &[&str] = &["--name-only", "--no-renames", "--no-ext-diff", "-z"];

/// `git status` argv every porcelain parse uses. `-z` is what makes a path
/// with unusual bytes survive; newline porcelain quotes them.
pub const PORCELAIN_STATUS: &[&str] = &["status", "--porcelain", "-z"];

/// Why a git spawn or its output could not be used.
#[derive(Debug)]
pub enum GitCommandError {
    /// The process could not be started.
    Spawn {
        /// The argv after `git` (and after `-C <repo>` when one was set).
        args: String,
        /// The `-C` repository, when the spawn was repo-scoped.
        repo: Option<String>,
        /// The IO fault from `Command::output` / `spawn`.
        source: io::Error,
    },
    /// git ran and exited non-zero.
    Failed {
        /// The argv after `git`.
        args: String,
        /// Trimmed stderr git printed.
        stderr: String,
    },
    /// stdout was required to be UTF-8 and was not.
    Encoding,
}

impl GitCommandError {
    /// Trimmed stderr of a failed git, or empty for spawn/encoding faults.
    #[must_use]
    pub fn stderr(&self) -> &str {
        match self {
            Self::Failed { stderr, .. } => stderr,
            Self::Spawn { .. } | Self::Encoding => "",
        }
    }
}

impl fmt::Display for GitCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { args, repo: Some(repo), source } => write!(f, "git {args} in {repo}: {source}"),
            Self::Spawn { args, repo: None, source } => write!(f, "git {args}: {source}"),
            Self::Failed { args, stderr } if stderr.is_empty() => write!(f, "git {args} failed"),
            Self::Failed { args, stderr } => write!(f, "git {args}: {stderr}"),
            Self::Encoding => write!(f, "git produced non-UTF-8 output"),
        }
    }
}

impl Error for GitCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::Failed { .. } | Self::Encoding => None,
        }
    }
}

impl From<GitCommandError> for GitDataError {
    fn from(error: GitCommandError) -> Self {
        Self::Command(error.to_string())
    }
}

/// Argv for a name-only diff of `from..to` that feeds closure or containment.
///
/// Always includes [`NAME_ONLY_DIFF_FLAGS`]. Callers do not pass those flags.
#[must_use]
pub fn name_only_diff_argv<'a>(from: &'a str, to: &'a str) -> Vec<&'a str> {
    let mut args = vec!["diff"];
    args.extend(NAME_ONLY_DIFF_FLAGS);
    args.push(from);
    args.push(to);
    args
}

/// Repository-relative paths `from..to` changed, NUL-split.
///
/// # Errors
/// Spawn failed, `git diff` exited non-zero, or stdout was not UTF-8.
pub fn name_only_paths(repo: &Path, from: &str, to: &str) -> Result<Vec<String>, GitCommandError> {
    let argv = name_only_diff_argv(from, to);
    let output = run(repo, &argv)?;
    if !output.status.success() {
        return Err(failed(&argv, &output));
    }
    String::from_utf8(output.stdout).map_or(Err(GitCommandError::Encoding), |stdout| Ok(split_nul(&stdout)))
}

/// Paths `git status --porcelain -z` names in `repo`.
///
/// # Errors
/// Spawn failed or `git status` exited non-zero.
pub fn porcelain_entries(repo: &Path) -> Result<Vec<String>, GitCommandError> {
    let output = run(repo, PORCELAIN_STATUS)?;
    if !output.status.success() {
        return Err(failed(PORCELAIN_STATUS, &output));
    }
    Ok(split_nul(&String::from_utf8_lossy(&output.stdout)))
}

/// The repository-relative paths `git status --porcelain -z` reports as
/// written in `repo` — the observation ADR-0204's write leases are acquired
/// from.
///
/// # Errors
/// Spawn failed or `git status` exited non-zero.
pub fn written_paths(repo: &Path) -> Result<Vec<String>, GitCommandError> {
    Ok(porcelain_path_tokens(porcelain_entries(repo)?))
}

/// Strip the two-letter status field from each `-z` porcelain entry.
///
/// A rename emits two tokens: the entry proper (`R  <new>`) and a bare
/// following token holding the old name. The bare token is a path a lane
/// touched just as much as the new name is, so it is kept as written rather
/// than dropped — a rename out of a file another member is editing is exactly
/// the contention a lease exists to catch. An entry is recognized by the
/// separating space git puts at index 2; anything shorter, or without it, is
/// the bare form.
fn porcelain_path_tokens(entries: Vec<String>) -> Vec<String> {
    entries
        .into_iter()
        .map(|entry| match entry.as_bytes().get(2) {
            Some(b' ') if entry.len() > 3 => entry[3..].to_owned(),
            _ => entry,
        })
        .collect()
}

/// Split a `-z` path list. Empty tokens (a trailing NUL) are dropped.
#[must_use]
pub fn split_nul(stdout: &str) -> Vec<String> {
    stdout.split('\0').filter(|path| !path.is_empty()).map(str::to_owned).collect()
}

/// Trimmed lossy UTF-8 of `bytes`.
#[must_use]
pub fn trim_bytes(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

/// Whether `remote` names the same object database as `repo`.
///
/// A non-absolute `remote` (`origin`) never does — the absolute-path guard
/// lives here so a caller cannot forget it. An absolute path does when it is
/// the repository itself, or a worktree that shares its common dir.
#[must_use]
pub fn shares_object_database(repo: &Path, remote: &Path) -> bool {
    if !remote.is_absolute() {
        return false;
    }
    match (git_common_dir(repo), git_common_dir(remote)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Canonical path of `repo`'s object database (`rev-parse --git-common-dir`).
///
/// `--absolute-git-common-dir` is not on the git this host ships (2.43),
/// which treats the unknown flag as a revision name and prints it back —
/// every repository then "shares" the same database.
#[must_use]
pub fn git_common_dir(repo: &Path) -> Option<PathBuf> {
    let output = run(repo, &["rev-parse", "--git-common-dir"]).ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = trim_bytes(&output.stdout);
    let path = if Path::new(&raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        repo.join(raw)
    };
    path.canonicalize().ok()
}

/// Ask the installed `git` what it is.
///
/// # Errors
/// The binary could not be spawned or did not print a parseable version.
pub fn detect_version() -> Result<(u32, u32, u32), GitDataError> {
    let output = run_global(&["version"])?;
    if !output.status.success() {
        return Err(GitDataError::Command(format!("git version: {}", trim_bytes(&output.stderr))));
    }
    parse_version(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `git version` stdout (`git version 2.43.0`, optionally with a suffix).
///
/// # Errors
/// The text is not a `git version A.B[.C]` line.
pub fn parse_version(stdout: &str) -> Result<(u32, u32, u32), GitDataError> {
    let rest = stdout
        .trim()
        .strip_prefix("git version ")
        .ok_or_else(|| GitDataError::Command(format!("unrecognised git version output: {}", stdout.trim())))?;
    let token = rest.split_whitespace().next().unwrap_or("");
    let mut parts = token.split('.');
    let major = parse_component(parts.next(), token)?;
    let minor = parse_component(parts.next(), token)?;
    let patch = parts.next().map_or(0, |part| leading_digits(part).parse().unwrap_or(0));
    Ok((major, minor, patch))
}

fn parse_component(part: Option<&str>, token: &str) -> Result<u32, GitDataError> {
    part.and_then(|part| leading_digits(part).parse().ok())
        .ok_or_else(|| GitDataError::Command(format!("unrecognised git version token: {token}")))
}

fn leading_digits(part: &str) -> String {
    part.chars().take_while(char::is_ascii_digit).collect()
}

/// Refuse a git older than [`MIN_GIT`], naming the version we found.
///
/// # Errors
/// `have` is below the attribute-source floor.
pub fn require_min(have: (u32, u32, u32)) -> Result<(), GitDataError> {
    if have < MIN_GIT {
        return Err(GitDataError::Command(format!(
            "git {}.{} or newer is required for the merge-tree attribute source; found {}.{}.{}",
            MIN_GIT.0, MIN_GIT.1, have.0, have.1, have.2
        )));
    }
    Ok(())
}

/// Run `git args…` with no `-C` (version probes, clones that name the path).
///
/// # Errors
/// The process could not be spawned.
pub fn run_global(args: &[&str]) -> Result<Output, GitCommandError> {
    spawn(None, args, &[], None)
}

/// Run `git -C repo args…` and return the raw output.
///
/// # Errors
/// The process could not be spawned.
pub fn run(repo: &Path, args: &[&str]) -> Result<Output, GitCommandError> {
    spawn(Some(repo), args, &[], None)
}

/// Run `git -C repo args…` and return trimmed stdout on success.
///
/// # Errors
/// Spawn failed or the command exited non-zero.
pub fn run_ok(repo: &Path, args: &[&str]) -> Result<String, GitCommandError> {
    let output = run(repo, args)?;
    if !output.status.success() {
        return Err(failed(args, &output));
    }
    Ok(trim_bytes(&output.stdout))
}

/// Run `git -C repo args…` with extra environment (commit identity).
///
/// # Errors
/// The process could not be spawned.
pub fn run_env(repo: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<Output, GitCommandError> {
    spawn(Some(repo), args, envs, None)
}

/// Run `git -C repo args…` with `stdin` piped in.
///
/// # Errors
/// Spawn, stdin write, or wait failed.
pub fn run_stdin(repo: &Path, args: &[&str], stdin: &str) -> Result<Output, GitCommandError> {
    spawn(Some(repo), args, &[], Some(stdin))
}

fn spawn(
    repo: Option<&Path>,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: Option<&str>,
) -> Result<Output, GitCommandError> {
    let mut command = Command::new("git");
    if let Some(repo) = repo {
        command.arg("-C").arg(repo);
    }
    command.envs(envs.iter().copied()).args(args);
    if let Some(stdin) = stdin {
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|source| spawn_err(repo, args, source))?;
        if let Some(mut pipe) = child.stdin.take() {
            pipe.write_all(stdin.as_bytes()).map_err(|source| spawn_err(repo, args, source))?;
        }
        return child.wait_with_output().map_err(|source| spawn_err(repo, args, source));
    }
    command.output().map_err(|source| spawn_err(repo, args, source))
}

fn spawn_err(repo: Option<&Path>, args: &[&str], source: io::Error) -> GitCommandError {
    GitCommandError::Spawn { args: format!("{args:?}"), repo: repo.map(|path| path.display().to_string()), source }
}

fn failed(args: &[&str], output: &Output) -> GitCommandError {
    GitCommandError::Failed { args: format!("{args:?}"), stderr: trim_bytes(&output.stderr) }
}

/// Mint `message` over `tree` with `parents` as a commit object in `repo`,
/// returning its sha. The one implementation the local backend and the
/// object-repo fake both commit through, so the two cannot mint different shas
/// for one input.
///
/// The author date is inherited from the parents rather than read from the
/// clock, because the sha has to stay a pure function of
/// `(message, tree, parents)`: `GitSource::integrate` recovers from a fault
/// between its commit and its ref update only because the retry re-creates a
/// byte-identical commit and git hands back the same sha.
///
/// # Errors
/// Spawn failed, or `commit-tree` refused the tree or one of the parents.
pub fn commit_tree(repo: &Path, message: &str, tree: &str, parents: &[String]) -> Result<String, GitDataError> {
    let mut args = vec!["commit-tree".to_owned(), tree.to_owned()];
    for parent in parents {
        args.push("-p".to_owned());
        args.push(parent.clone());
    }
    args.extend(["-m".to_owned(), message.to_owned()]);
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();

    let authored = format!("@{} +0000", inherited_author_secs(repo, parents));
    let identity = [
        ("GIT_AUTHOR_NAME", BLOOMERY_AUTHOR.0),
        ("GIT_AUTHOR_EMAIL", BLOOMERY_AUTHOR.1),
        ("GIT_AUTHOR_DATE", authored.as_str()),
        BLOOMERY_COMMITTER[0],
        BLOOMERY_COMMITTER[1],
        BLOOMERY_COMMITTER[2],
    ];
    let output = run_env(repo, &borrowed, &identity)?;
    if !output.status.success() {
        return Err(GitDataError::Command(format!(
            "git commit-tree {tree}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(trim_bytes(&output.stdout))
}

/// The moment a commit over `parents` is authored at: the newest one already
/// recorded on a parent, in whole seconds since the epoch.
///
/// A real moment that is nonetheless a pure function of the inputs — the two
/// properties otherwise in tension here, since a clock read would mint a second
/// commit on every retry. A fold's merge inherits the lane capture it merges
/// in, which is when the work was actually produced; a tree-replace integrate
/// inherits the branch it extends. Taking the newest rather than a fixed side
/// is what keeps the dates along a branch from going backwards.
///
/// A parentless commit has nothing to inherit and stays at the epoch. The only
/// ones are the claim registry's holds and their tombstones, which live on
/// `bloomery/claims/*` and never reach landed history.
fn inherited_author_secs(repo: &Path, parents: &[String]) -> i64 {
    parents.iter().filter_map(|parent| newest_stamp(repo, parent)).max().unwrap_or(0)
}

/// The later of `sha`'s author and committer timestamps, or `None` when `sha`
/// names no commit. Both sides are read because either can be the real one: a
/// bloomery-minted parent's committer date is the pinned epoch, and a rebased
/// parent's author date predates its committer date.
fn newest_stamp(repo: &Path, sha: &str) -> Option<i64> {
    run_ok(repo, &["show", "--no-patch", "--format=%at %ct", sha, "--"])
        .ok()?
        .split_whitespace()
        .filter_map(|stamp| stamp.parse::<i64>().ok())
        .max()
}

/// Read commit object `sha` (`cat-file`). A missing or non-commit object is
/// [`GitDataError::MissingObject`].
///
/// # Errors
/// Spawn failed, or `sha` does not name a commit in `repo`.
pub fn read_commit(repo: &Path, sha: &str) -> Result<GitCommit, GitDataError> {
    let kind = run(repo, &["cat-file", "-t", sha])?;
    if !kind.status.success() || String::from_utf8_lossy(&kind.stdout).trim() != "commit" {
        return Err(GitDataError::MissingObject(format!("no commit {sha}")));
    }
    let body = run_ok(repo, &["cat-file", "commit", sha])
        .map_err(|_| GitDataError::MissingObject(format!("no commit {sha}")))?;
    parse_commit(sha, &body)
}

/// Whether `ancestor` is reachable from `commit`. Equal shas are ancestors of
/// themselves without asking git — matching the trait, including when the sha
/// names no object.
///
/// # Errors
/// [`GitDataError::MissingObject`] when git exits 128 with a not-a-valid-commit
/// wording; any other execution failure is [`GitDataError::Command`].
pub fn is_ancestor(repo: &Path, ancestor: &str, commit: &str) -> Result<bool, GitDataError> {
    if ancestor == commit {
        return Ok(true);
    }
    let output = run(repo, &["merge-base", "--is-ancestor", ancestor, commit])?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        Some(128) if not_a_valid_commit(&stderr) => {
            Err(GitDataError::MissingObject(format!("missing ancestor {ancestor} or commit {commit}")))
        }
        _ => Err(GitDataError::Command(format!("git merge-base --is-ancestor {ancestor} {commit}: {}", stderr.trim()))),
    }
}

/// Paths `git merge-tree --name-only` names as colliding.
///
/// `--no-messages` suppresses the informational section git would otherwise
/// emit on conflict (`Auto-merging`, `CONFLICT (content)`), so the listing is
/// only the colliding paths plus the leading tree id the oid filter drops.
/// `-z` makes the listing a NUL split rather than a parse — git quotes
/// unusual bytes in line mode. Spawn failure or a missing object yields an
/// empty list — the caller already classified the merge itself.
#[must_use]
pub fn conflicted_paths(repo: &Path, base: &str, head: &str) -> Vec<String> {
    let Ok(output) = run(repo, &["merge-tree", "--write-tree", "--name-only", "--no-messages", "-z", base, head])
    else {
        return Vec::new();
    };
    split_nul(&String::from_utf8_lossy(&output.stdout)).into_iter().filter(|path| !is_git_oid(path)).collect()
}

fn is_git_oid(line: &str) -> bool {
    (line.len() == 40 || line.len() == 64) && line.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn not_a_valid_commit(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("not a valid commit")
}

fn parse_commit(sha: &str, body: &str) -> Result<GitCommit, GitDataError> {
    let mut tree = None;
    let mut message_start = None;
    for (index, line) in body.lines().enumerate() {
        if let Some(value) = line.strip_prefix("tree ") {
            tree = Some(value.trim().to_owned());
        } else if line.is_empty() {
            message_start = Some(index + 1);
            break;
        }
    }
    let tree = tree.ok_or_else(|| GitDataError::Command(format!("commit {sha} has no tree header")))?;
    let message =
        message_start.map_or_else(String::new, |start| body.lines().skip(start).collect::<Vec<_>>().join("\n"));
    Ok(GitCommit { sha: sha.to_owned(), tree, message })
}

/// Classify a failed `update-ref` (single or `--stdin`) onto the git-data
/// vocabulary. Git's wording is the only signal — there is no status code.
#[must_use]
pub fn classify_update(output: &Output, name: &str) -> GitDataError {
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    let lower = detail.to_ascii_lowercase();
    if lower.contains("already exists") || lower.contains("but expected") || lower.contains("is at") {
        GitDataError::RefConflict(format!("{name}: {detail}"))
    } else if lower.contains("unable to resolve")
        || lower.contains("not a valid object")
        || lower.contains("bad object")
        || lower.contains("nonexistent object")
        || lower.contains("missing")
    {
        GitDataError::MissingObject(format!("{name}: {detail}"))
    } else if lower.contains("multiple updates") {
        GitDataError::Command(format!("multiple updates for {name}: {detail}"))
    } else if lower.contains("bad name") {
        GitDataError::Command(format!("bad name {name}: {detail}"))
    } else {
        GitDataError::Command(format!("git update-ref {name}: {detail}"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        MIN_GIT, NAME_ONLY_DIFF_FLAGS, PORCELAIN_STATUS, name_only_diff_argv, parse_version, porcelain_path_tokens,
        require_min, shares_object_database, split_nul,
    };

    #[test]
    fn parse_version_reads_plain_and_suffixed_git_version_lines() {
        assert_eq!(parse_version("git version 2.43.0\n").expect("plain"), (2, 43, 0));
        assert_eq!(parse_version("git version 2.39.5 (Apple Git-154)").expect("suffixed"), (2, 39, 5));
        assert_eq!(parse_version("git version 2.40.0").expect("floor"), MIN_GIT);
    }

    #[test]
    fn require_min_names_a_too_old_version() {
        let error = require_min((2, 39, 5)).expect_err("too old");
        let text = error.to_string();
        assert!(text.contains("2.40"), "{text}");
        assert!(text.contains("2.39.5"), "{text}");
    }

    #[test]
    fn a_name_only_diff_always_carries_no_renames_and_nul_termination() {
        // Tripwire: only one of three closure feeds used to pass `--no-renames`,
        // and a rename then attributed a moved file to the destination crate
        // only. The layer composes the argv; call sites cannot drop the flags.
        let argv = name_only_diff_argv("abc", "HEAD");
        assert!(argv.contains(&"--no-renames"), "name-only diff argv must carry --no-renames: {argv:?}");
        assert!(argv.contains(&"-z"), "name-only diff argv must carry -z: {argv:?}");
        assert_eq!(argv[0], "diff");
        for flag in NAME_ONLY_DIFF_FLAGS {
            assert!(argv.contains(flag), "{flag} missing from {argv:?}");
        }
    }

    #[test]
    fn a_porcelain_entry_yields_the_path_without_its_status_field() {
        // The plausible bug: the two-letter status field rides into the lease
        // table, so `M  crates/a/src/lib.rs` and `A  crates/a/src/lib.rs` are
        // two different "paths" and neither is the file anyone else contends
        // for. The rename's bare second token has no status field to strip and
        // must survive as written.
        let entries = vec![
            " M crates/a/src/lib.rs".to_string(),
            "?? crates/a/src/new.rs".to_string(),
            "R  crates/a/src/to.rs".to_string(),
            "crates/a/src/from.rs".to_string(),
        ];

        assert_eq!(
            porcelain_path_tokens(entries),
            vec![
                "crates/a/src/lib.rs".to_string(),
                "crates/a/src/new.rs".to_string(),
                "crates/a/src/to.rs".to_string(),
                "crates/a/src/from.rs".to_string(),
            ],
        );
    }

    #[test]
    fn a_porcelain_status_parse_always_carries_nul_termination() {
        // Tripwire: the capture path's "did this lane produce work" check used
        // newline porcelain, which quotes unusual bytes and can hide a dirty
        // tree behind a path the emptiness test never sees.
        assert!(PORCELAIN_STATUS.contains(&"-z"), "porcelain status argv must carry -z: {PORCELAIN_STATUS:?}");
        assert_eq!(PORCELAIN_STATUS[0], "status");
        assert!(PORCELAIN_STATUS.contains(&"--porcelain"), "{PORCELAIN_STATUS:?}");
    }

    #[test]
    fn a_nul_separated_list_splits_on_the_separator_rather_than_on_whitespace() {
        // Tripwire: `-z` is what makes a path with a space in it survive, and a
        // split on lines or whitespace would silently truncate one into a path
        // no package owns.
        assert_eq!(
            split_nul("crates/aether-render/src/lib.rs\0docs/some note.md\0"),
            ["crates/aether-render/src/lib.rs", "docs/some note.md"]
        );
        assert!(split_nul("").is_empty(), "an empty listing names no paths");
    }

    #[test]
    fn a_relative_remote_never_shares_an_object_database() {
        // Tripwire: the absolute-path guard used to live at one of two call
        // sites. A named remote (`origin`) must not be asked of git, and must
        // not read as "same database" when the caller forgets the guard.
        assert!(!shares_object_database(Path::new("/tmp/repo.git"), Path::new("origin")));
        assert!(!shares_object_database(Path::new("/tmp/repo.git"), Path::new("relative/path")));
    }
}
