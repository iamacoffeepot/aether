//! Mechanical fixers the construct/refine lane runs after the model exits.
//!
//! The chassis stages the worktree only once this process returns, so this is
//! the last moment the lane owns its tree. `cargo fmt` and a
//! `MachineApplicable` `cargo clippy --fix` over the packages the run touched
//! apply the class of findings that otherwise burn a Refine lap on a patch the
//! toolchain already wrote. Best-effort: an error, a timeout, or a no-op
//! leaves the tree as the model left it and never fails the lane. What they
//! change is part of this candidate; the evidence envelope records whether
//! they ran and whether they moved anything, so a later reader can tell a
//! model-authored line from a fixer-authored one at the run level.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{fs, thread};

use crate::transform::sccache;

/// How long `cargo fmt` may run before it is treated as a failed fixer.
const FMT_BUDGET: Duration = Duration::from_mins(2);

/// How long the one scoped `clippy --fix` build may run. A check-profile
/// compile of a few packages, sccache-warm; past this the tree is restored
/// rather than waiting out a wedged rustc.
const CLIPPY_FIX_BUDGET: Duration = Duration::from_mins(15);

/// What the construct/refine evidence envelope records about the fixers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Report {
    /// At least one fixer was invoked.
    pub ran: bool,
    /// The worktree after the fixers differs from the tree the model left.
    pub changed: bool,
}

impl Report {
    /// Stamp `ran` / `changed` onto a construct evidence envelope. Always
    /// present: a reader that has to guess whether the lane even tried cannot
    /// tell a model-authored line from a fixer-authored one.
    pub(super) fn stamp(self, evidence: &mut serde_json::Value) {
        if let Some(object) = evidence.as_object_mut() {
            object.insert(
                "fixers".to_owned(),
                serde_json::json!({
                    "ran": self.ran,
                    "changed": self.changed,
                }),
            );
        }
    }
}

/// Run the mechanical fixers over `worktree`. Infallible: a fixer that
/// errors or times out is rolled back to the snapshot taken just before
/// *that* command, and the lane still returns a report.
pub(super) fn apply(worktree: &Path, out_dir: &Path) -> Report {
    let dirty = dirty_paths(worktree, Some(out_dir));
    let rust_files: Vec<String> = dirty
        .iter()
        .filter(|path| Path::new(path).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("rs")))
        .filter(|path| worktree.join(path).is_file())
        .cloned()
        .collect();
    if rust_files.is_empty() {
        return Report::default();
    }

    let packages =
        workspace_members(worktree).map(|members| owning_packages(&members, &rust_files)).unwrap_or_default();
    let before = worktree_state(worktree, out_dir);

    run_fixer(worktree, "cargo", &fmt_argv(worktree, &rust_files), FMT_BUDGET, |_| {});
    if !packages.is_empty() {
        run_fixer(worktree, "cargo", &clippy_fix_argv(&packages), CLIPPY_FIX_BUDGET, |command| {
            command.env("CARGO_INCREMENTAL", "0");
            sccache::export(sccache::detect().as_ref(), command);
        });
    }

    Report { ran: true, changed: before != worktree_state(worktree, out_dir) }
}

/// Longest-prefix workspace-root match: each path belongs to the package that
/// owns it, not that package's dependents. A docs path or anything else
/// outside a member root is dropped, so clippy is never pointed at the
/// workspace by accident.
fn owning_packages(members: &[(String, String)], paths: &[String]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for path in paths {
        if let Some((_, name)) = members
            .iter()
            .filter(|(root, _)| path == root || path.strip_prefix(root).is_some_and(|rest| rest.starts_with('/')))
            .max_by_key(|(root, _)| root.len())
        {
            names.insert(name.clone());
        }
    }
    names.into_iter().collect()
}

/// `cargo fmt` over the rust files the run actually left on disk — never the
/// whole workspace, so a rustfmt version drift cannot pull untouched files
/// into the candidate. A deleted path is omitted so rustfmt cannot fail on a
/// model deletion and trigger a restore that used to resurrect it.
fn fmt_argv(worktree: &Path, paths: &[String]) -> Vec<String> {
    let mut args = vec!["fmt".to_owned(), "--".to_owned()];
    args.extend(paths.iter().filter(|path| worktree.join(path).is_file()).cloned());
    args
}

/// One `cargo clippy --fix` over the packages that own a dirty rust file.
///
/// `--fix` defaults to `MachineApplicable` and implies `--no-deps` /
/// `--all-targets`; both are stated so a later cargo that drops the
/// implication still cannot widen applicability (`--broken-code`) or walk
/// the workspace. `--allow-dirty` / `--allow-staged` are required because
/// the model just dirtied the tree.
fn clippy_fix_argv(packages: &[String]) -> Vec<String> {
    let mut args = vec![
        "clippy".to_owned(),
        "--fix".to_owned(),
        "--allow-dirty".to_owned(),
        "--allow-staged".to_owned(),
        "--no-deps".to_owned(),
        "--all-targets".to_owned(),
    ];
    args.extend(packages.iter().flat_map(|package| ["-p".to_owned(), package.clone()]));
    args
}

/// Run one fixer under `budget`. On a non-zero exit, a spawn failure, or a
/// timeout the worktree is restored to the snapshot taken just before this
/// command, so a fixer that cannot finish never leaves a half-applied tree
/// and never fails the lane.
fn run_fixer(worktree: &Path, program: &str, args: &[String], budget: Duration, configure: impl FnOnce(&mut Command)) {
    let snapshot = Snapshot::capture(worktree);
    match spawn_and_wait(worktree, program, args, budget, configure) {
        Ok(status) if status.success() => {}
        other => {
            match other {
                Ok(status) => {
                    eprintln!(
                        "construct lane: {program} {} exited {status}; leaving the tree as the model left it",
                        args.join(" "),
                    );
                }
                Err(error) => {
                    eprintln!(
                        "construct lane: {program} {} {error}; leaving the tree as the model left it",
                        args.join(" "),
                    );
                }
            }
            if let Some(snapshot) = snapshot {
                snapshot.restore(worktree);
            }
        }
    }
}

fn spawn_and_wait(
    worktree: &Path,
    program: &str,
    args: &[String],
    budget: Duration,
    configure: impl FnOnce(&mut Command),
) -> Result<ExitStatus, String> {
    let mut command = Command::new(program);
    command.args(args).current_dir(worktree).stdin(Stdio::null());
    isolate(&mut command);
    configure(&mut command);
    let mut child = command.spawn().map_err(|error| format!("could not spawn: {error}"))?;
    wait_budget(&mut child, budget)
}

fn wait_budget(child: &mut Child, budget: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_tree(child);
                let _ = child.wait();
                return Err("timed out".to_owned());
            }
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(format!("could not wait: {error}")),
        }
    }
}

/// Put the child in its own process group so a timeout can reap rustc
/// grandchildren instead of leaving them attached to the slot's target dir.
fn isolate(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        let _ = Command::new("kill").args(["-s", "KILL", "--", &format!("-{pid}")]).status();
    }
    let _ = child.kill();
}

/// Dirty repository-relative paths. `out_dir` is the run's own evidence tree
/// and is ignored the same way the candidate signal ignores it; `None`
/// keeps every path, which the snapshot uses so a restore cannot miss a
/// fixer-dirtied file. A git that will not run yields an empty list — the
/// same absence `apply` already treats as nothing to fix.
fn dirty_paths(worktree: &Path, out_dir: Option<&Path>) -> Vec<String> {
    porcelain_dirty(worktree)
        .unwrap_or_default()
        .into_iter()
        .filter(|path| out_dir.is_none_or(|out| !in_out_dir(path, out)))
        .collect()
}

/// Porcelain paths, or `None` when git did not answer. Distinct from an
/// empty list so a snapshot cannot mistake a failed `status` for a clean
/// tree and then restore by deleting the model's work.
fn porcelain_dirty(worktree: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "-z"])
        .current_dir(worktree)
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    Some(porcelain_paths(&output.stdout))
}

/// The run's evidence tree, or an untracked parent porcelain reports as
/// `dir/` instead of listing the files inside. A crate path that merely
/// shares a prefix with `--out` (an `--out xtask/foo` over a dirty
/// `xtask/src/lib.rs`) is not the evidence tree.
fn in_out_dir(path: &str, out_dir: &Path) -> bool {
    if Path::new(path).starts_with(out_dir) {
        return true;
    }
    let Some(out) = out_dir.to_str() else {
        return false;
    };
    let prefix = path.trim_end_matches('/');
    !prefix.is_empty() && (out == prefix || out.starts_with(&format!("{prefix}/")))
}

/// Porcelain v1 `-z` paths. A rename/copy is two records (`orig\0new\0`); the
/// candidate is the new path, matching the space-separated parser the
/// candidate signal already uses.
fn porcelain_paths(stdout: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut records = stdout.split(|byte| *byte == 0).filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        let line = String::from_utf8_lossy(record);
        let Some(code) = line.as_bytes().get(..2) else {
            continue;
        };
        let path = line.get(3..).unwrap_or("").trim();
        let renamed = code[0] == b'R' || code[0] == b'C';
        let path = if renamed {
            records.next().map(String::from_utf8_lossy).map_or_else(|| path.to_owned(), Cow::into_owned)
        } else {
            path.to_owned()
        };
        if !path.is_empty() {
            paths.push(path);
        }
    }
    paths
}

fn worktree_state(worktree: &Path, out_dir: &Path) -> BTreeMap<String, Vec<u8>> {
    dirty_paths(worktree, Some(out_dir))
        .into_iter()
        .filter_map(|path| fs::read(worktree.join(&path)).ok().map(|bytes| (path, bytes)))
        .collect()
}

fn workspace_members(worktree: &Path) -> Option<Vec<(String, String)>> {
    let mut metadata = guppy::MetadataCommand::new();
    metadata.current_dir(worktree);
    metadata.no_deps();
    let graph = metadata.build_graph().ok()?;
    Some(
        graph
            .workspace()
            .iter()
            .filter_map(|package| {
                let root = package.source().workspace_path()?.as_str();
                (!root.is_empty()).then(|| (root.to_owned(), package.name().to_owned()))
            })
            .collect(),
    )
}

/// Contents of every dirty path, used to put the tree back when a fixer
/// cannot finish. `None` is a model deletion and is re-deleted on restore
/// rather than treated as fixer-introduced. Newly dirtied files (a clippy
/// edit to a file the model never touched, a rustfmt tempfile) are
/// reverted to HEAD or deleted.
struct Snapshot {
    files: BTreeMap<String, Option<Vec<u8>>>,
}

impl Snapshot {
    fn capture(worktree: &Path) -> Option<Self> {
        let files = porcelain_dirty(worktree)?
            .into_iter()
            .filter_map(|path| match fs::read(worktree.join(&path)) {
                Ok(bytes) => Some((path, Some(bytes))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some((path, None)),
                Err(_) => None,
            })
            .collect();
        Some(Self { files })
    }

    fn restore(&self, worktree: &Path) {
        let Some(now) = porcelain_dirty(worktree) else {
            return;
        };
        let now: BTreeSet<String> = now.into_iter().collect();
        for path in now.iter().filter(|path| !self.files.contains_key(*path)) {
            revert_introduced(worktree, path);
        }
        for (path, contents) in &self.files {
            match contents {
                Some(bytes) => {
                    let dest = worktree.join(path);
                    if let Some(parent) = dest.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::write(dest, bytes);
                }
                None => {
                    let _ = fs::remove_file(worktree.join(path));
                }
            }
        }
    }
}

/// A path the fixer dirtied that the model had left clean: restore the
/// HEAD blob when it is tracked, otherwise delete the file.
fn revert_introduced(worktree: &Path, path: &str) {
    let restored = Command::new("git")
        .args(["checkout", "--", path])
        .current_dir(worktree)
        .stderr(Stdio::null())
        .status()
        .ok()
        .is_some_and(|status| status.success());
    if !restored {
        let _ = fs::remove_file(worktree.join(path));
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::{env, fs, process};

    use super::{
        Report, Snapshot, apply, clippy_fix_argv, dirty_paths, fmt_argv, in_out_dir, isolate, owning_packages,
        porcelain_paths, revert_introduced, wait_budget,
    };

    fn members(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(root, name)| ((*root).to_owned(), (*name).to_owned())).collect()
    }

    fn paths(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    // A clippy --fix pointed at a reverse-dep closure (or the workspace) is
    // the bug this scoping exists to prevent: it would rewrite files the
    // model never touched, including on clippy version drift.
    #[test]
    fn dirty_rust_files_select_their_owning_package_not_dependents() {
        let workspace = members(&[
            ("crates/aether-math", "aether-math"),
            ("crates/aether-data", "aether-data"),
            ("xtask", "xtask"),
        ]);

        assert_eq!(
            owning_packages(&workspace, &paths(&["crates/aether-math/src/lib.rs"])),
            ["aether-math"],
            "a near-leaf edit must not pull dependents into --fix",
        );
        assert_eq!(
            owning_packages(&workspace, &paths(&["xtask/src/transform/construct.rs", "docs/guide/foo.md"])),
            ["xtask"],
            "a path outside every package root is dropped, not treated as the workspace",
        );
        assert_eq!(
            owning_packages(&workspace, &paths(&["crates/aether-math/src/lib.rs", "crates/aether-data/src/kind.rs"]),),
            ["aether-data", "aether-math"],
        );
        assert!(
            owning_packages(&workspace, &paths(&["docs/adr/0001-title.md", "README.md"])).is_empty(),
            "prose-only dirt must not invent a -p set that would walk the workspace",
        );
    }

    // Longest prefix is what keeps a nested crate from being charged to its
    // parent (or the parent to a similarly-prefixed sibling).
    #[test]
    fn a_nested_crate_wins_the_longest_workspace_root() {
        let workspace =
            members(&[("crates/foo", "foo"), ("crates/foo/bar", "foo-bar"), ("crates/foo-extra", "foo-extra")]);
        assert_eq!(owning_packages(&workspace, &paths(&["crates/foo/bar/src/lib.rs"])), ["foo-bar"]);
        assert_eq!(owning_packages(&workspace, &paths(&["crates/foo-extra/src/lib.rs"])), ["foo-extra"]);
        assert_eq!(owning_packages(&workspace, &paths(&["crates/foo/src/lib.rs"])), ["foo"]);
    }

    // Tripwire: `--broken-code` widens past MachineApplicable, and
    // `--workspace` ignores the package set. Either flag would apply fixes
    // the work order does not authorize.
    #[test]
    fn clippy_fix_stays_machine_applicable_and_package_scoped() {
        let argv = clippy_fix_argv(&paths(&["aether-math", "xtask"]));
        assert_eq!(argv[0], "clippy");
        assert!(argv.contains(&"--fix".to_owned()));
        assert!(argv.contains(&"--allow-dirty".to_owned()), "the model just dirtied the tree");
        assert!(argv.contains(&"--allow-staged".to_owned()));
        assert!(!argv.iter().any(|arg| arg == "--broken-code"), "default MachineApplicable must not be widened");
        assert!(
            !argv.iter().any(|arg| arg == "--workspace"),
            "a workspace clippy --fix would rewrite crates the run did not touch",
        );
        let dash_p = argv.iter().filter(|arg| *arg == "-p").count();
        assert_eq!(dash_p, 2, "each touched package is a -p, not a comma-list cargo would miss");
        assert!(argv.windows(2).any(|pair| pair == ["-p", "aether-math"]));
        assert!(argv.windows(2).any(|pair| pair == ["-p", "xtask"]));
    }

    #[test]
    fn fmt_is_restricted_to_the_dirty_rust_files() {
        let repo = git_scratch("fmt-scope");
        write(&repo, "crates/aether-math/src/lib.rs", "fn x() {}\n");
        write(&repo, "xtask/src/lib.rs", "fn y() {}\n");
        let argv = fmt_argv(&repo, &paths(&["crates/aether-math/src/lib.rs", "xtask/src/lib.rs"]));
        assert_eq!(argv[0], "fmt");
        assert_eq!(argv[1], "--");
        assert_eq!(&argv[2..], ["crates/aether-math/src/lib.rs", "xtask/src/lib.rs"]);
        assert!(
            !argv.iter().any(|arg| arg == "--all" || arg == "--workspace"),
            "workspace fmt on rustfmt drift would pull untouched files into the candidate",
        );
    }

    // rustfmt errors on a missing path and that non-zero used to restore the
    // deletion away. The argv must not include a path that is gone.
    #[test]
    fn fmt_argv_omits_a_deleted_path() {
        let repo = git_scratch("fmt-deleted");
        write(&repo, "kept.rs", "fn x() {}\n");
        write(&repo, "gone.rs", "fn y() {}\n");
        git(&repo, &["add", "kept.rs", "gone.rs"]);
        git(&repo, &["commit", "-m", "init"]);
        fs::remove_file(repo.join("gone.rs")).expect("delete");

        let argv = fmt_argv(&repo, &paths(&["kept.rs", "gone.rs"]));
        assert_eq!(&argv[2..], ["kept.rs"]);
    }

    #[test]
    fn porcelain_z_reads_the_new_path_of_a_rename() {
        // `R  orig\0new\0` — taking the first record's tail would feed clippy
        // the pre-rename path, which is gone.
        let stdout = b"R  old.rs\0new.rs\0 M crates/aether-math/src/lib.rs\0";
        assert_eq!(porcelain_paths(stdout), ["new.rs", "crates/aether-math/src/lib.rs"]);
    }

    #[test]
    fn a_fixer_report_always_stamps_both_bits() {
        let mut evidence = serde_json::json!({ "command": "construct.implement" });
        Report { ran: true, changed: false }.stamp(&mut evidence);
        assert_eq!(evidence["fixers"]["ran"], true);
        assert_eq!(evidence["fixers"]["changed"], false, "ran-but-unchanged must not look like 'never ran'");

        let mut idle = serde_json::json!({ "command": "construct.implement" });
        Report::default().stamp(&mut idle);
        assert_eq!(idle["fixers"]["ran"], false);
        assert_eq!(idle["fixers"]["changed"], false);
    }

    // Restore must put the model's dirty file back, drop a file the fixer
    // created, and revert a clean file the fixer edited. Losing any of those
    // is how a timed-out clippy --fix would become the candidate.
    #[test]
    fn a_failed_fixer_restores_the_tree_the_model_left() {
        let repo = git_scratch("restore");
        write(&repo, "kept.rs", "model\n");
        write(&repo, "clean.rs", "clean\n");
        git(&repo, &["add", "kept.rs", "clean.rs"]);
        git(&repo, &["commit", "-m", "init"]);
        write(&repo, "kept.rs", "model-edited\n");

        let snapshot = Snapshot::capture(&repo).expect("snapshot");
        write(&repo, "kept.rs", "fixer-half-applied\n");
        write(&repo, "clean.rs", "fixer-touched-clean\n");
        write(&repo, "new.rs", "fixer-created\n");
        snapshot.restore(&repo);

        assert_eq!(fs::read_to_string(repo.join("kept.rs")).expect("kept"), "model-edited\n");
        assert_eq!(fs::read_to_string(repo.join("clean.rs")).expect("clean"), "clean\n");
        assert!(!repo.join("new.rs").exists(), "a file the fixer created must not survive the restore");
    }

    // A model-deleted tracked file used to drop out of the snapshot (the
    // read failed) and restore classified it as fixer-introduced, so
    // `git checkout` resurrected it. Pre-fix this fails by execution.
    #[test]
    fn a_failed_fixer_keeps_a_model_deleted_file_deleted() {
        let repo = git_scratch("restore-deleted");
        write(&repo, "kept.rs", "model\n");
        write(&repo, "gone.rs", "delete-me\n");
        git(&repo, &["add", "kept.rs", "gone.rs"]);
        git(&repo, &["commit", "-m", "init"]);
        write(&repo, "kept.rs", "model-edited\n");
        fs::remove_file(repo.join("gone.rs")).expect("delete");

        let snapshot = Snapshot::capture(&repo).expect("snapshot");
        write(&repo, "kept.rs", "fixer-half-applied\n");
        snapshot.restore(&repo);

        assert_eq!(fs::read_to_string(repo.join("kept.rs")).expect("kept"), "model-edited\n");
        assert!(!repo.join("gone.rs").exists(), "a model deletion must survive the restore");
    }

    #[test]
    fn an_untracked_parent_of_the_evidence_dir_is_still_the_evidence_dir() {
        let out = Path::new(".bloomery/out");
        assert!(in_out_dir(".bloomery/out/evidence.json", out));
        assert!(in_out_dir(".bloomery/", out), "porcelain names the untracked parent, not every file inside");
        assert!(in_out_dir(".bloomery", out));
        assert!(!in_out_dir("xtask/src/lib.rs", out), "a crate path is not hidden just because --out is elsewhere");
        assert!(!in_out_dir("src/lib.rs", Path::new("out")));
    }

    #[test]
    fn dirty_paths_ignore_the_runs_own_evidence_tree() {
        let repo = git_scratch("out-dir");
        write(&repo, "src/lib.rs", "fn x() {}\n");
        write(&repo, ".bloomery/out/evidence.json", "{}\n");
        git(&repo, &["add", "src/lib.rs"]);
        git(&repo, &["commit", "-m", "init"]);
        write(&repo, "src/lib.rs", "fn x() {  }\n");

        let dirty = dirty_paths(&repo, Some(Path::new(".bloomery/out")));
        assert_eq!(dirty, ["src/lib.rs"], "the run's evidence output is not a fixer input");
    }

    // A failed `git status` must not look like a clean tree: restore would
    // then treat every model-dirtied path as fixer-introduced and delete it.
    #[test]
    fn a_snapshot_is_refused_when_git_cannot_list_the_tree() {
        let dir = scratch_dir("no-git-snap");
        write(&dir, "kept.rs", "model\n");
        assert!(
            Snapshot::capture(&dir).is_none(),
            "a failed status must not snapshot as empty and later delete the model's files",
        );
    }

    // apply is the lane's public seam and must not fail closed: a directory
    // that is not a git worktree (or a clean one) is "did not run", not an
    // error the construct lane would have to turn into a failed attempt.
    #[test]
    fn apply_is_idle_when_there_is_nothing_to_fix() {
        let missing = scratch_dir("no-git");
        assert_eq!(apply(&missing, Path::new("out")), Report::default());

        let repo = git_scratch("clean");
        write(&repo, "src/lib.rs", "fn x() {}\n");
        git(&repo, &["add", "src/lib.rs"]);
        git(&repo, &["commit", "-m", "init"]);
        assert_eq!(apply(&repo, Path::new("out")), Report::default(), "a clean tree is not a fixer input");

        write(&repo, "README.md", "prose\n");
        assert_eq!(
            apply(&repo, Path::new("out")),
            Report::default(),
            "a prose-only dirty tree must not invoke clippy --fix over the workspace",
        );
    }

    // Tripwire: a fixer that hangs must not become a failed lane, and the
    // child must actually die — an orphan rustc would keep the slot's
    // target dir locked.
    #[test]
    fn a_timeout_reaps_the_child() {
        use std::process::Stdio;
        use std::time::Duration;

        let mut command = Command::new("sleep");
        command.arg("8").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        isolate(&mut command);
        let mut child = command.spawn().expect("spawn sleep");
        let error = wait_budget(&mut child, Duration::from_millis(150)).expect_err("sleep must time out");
        assert!(error.contains("timed out"), "{error}");
        assert!(child.try_wait().expect("reap").is_some(), "the child must not be left running");
    }

    #[test]
    fn revert_introduced_deletes_an_untracked_file() {
        let repo = git_scratch("revert");
        write(&repo, "tracked.rs", "ok\n");
        git(&repo, &["add", "tracked.rs"]);
        git(&repo, &["commit", "-m", "init"]);
        write(&repo, "stray.rs", "nope\n");
        revert_introduced(&repo, "stray.rs");
        assert!(!repo.join("stray.rs").exists());
    }

    fn git_scratch(tag: &str) -> PathBuf {
        let dir = scratch_dir(tag);
        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "fixers@test"]);
        git(&dir, &["config", "user.name", "fixers"]);
        git(&dir, &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git").args(args).current_dir(dir).output().expect("spawn git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }

    fn write(dir: &Path, path: &str, contents: &str) {
        let dest = dir.join(path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(dest, contents).expect("write");
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-fixers-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }
}
