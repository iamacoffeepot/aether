//! The scratch git repository a lane-boundary scenario's coordinator runs in.
//!
//! The dispatch path shells git with no `-C`: `git worktree add --force
//! --detach` and the subject `git fetch` both resolve against the coordinator's
//! own working directory. Pointing that at the developer's checkout would make
//! every scenario materialize the whole workspace, run `cargo fmt` over it, and
//! reach the network for a subject fetch — slow, and it writes worktree admin
//! entries into a repository the test does not own.
//!
//! So each scenario gets a repository of its own: a bare `origin` and a working
//! clone with a single commit, in a temp directory that goes away with the
//! fixture. It is small enough that a checkout is instant, and real enough that
//! every git step the coordinator takes is the one it takes in production —
//! including the `origin` fetch, which git satisfies locally when the object is
//! already present and which therefore needs no network but does need a remote
//! to name.

#![allow(dead_code, reason = "each test binary compiles the whole module and uses only the fixtures it needs")]
#![allow(clippy::unwrap_used, reason = "a fixture that cannot set up its repository reports it by panicking")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// A scratch repository: a bare origin, a working clone, and the commit the
/// scenario seals against.
pub struct ScratchRepo {
    root: TempDir,
    head: String,
}

impl ScratchRepo {
    /// Create the repository and its origin, returning the fixture.
    ///
    /// # Panics
    /// Any git step failed.
    pub fn create() -> Self {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "--quiet", "--bare", "origin.git"]);

        let work = root.path().join("work");
        git(root.path(), &["init", "--quiet", "work"]);
        // Local config, so the fixture reads the same on a machine whose global
        // identity is unset — and so the capture commit the coordinator makes in
        // a worktree of this repository has an identity to resolve.
        git(&work, &["config", "--local", "user.name", "lane harness"]);
        git(&work, &["config", "--local", "user.email", "lane-harness@example.test"]);
        fs::write(work.join("README.md"), "the subject a lane-boundary scenario checks out.\n").unwrap();
        git(&work, &["add", "--all"]);
        git(&work, &["commit", "--quiet", "--message", "subject"]);

        // The remote exists so the dispatch's `git fetch --no-tags origin <sha>`
        // resolves. It never reaches it: git satisfies a want it already holds
        // locally without opening a connection, which is exactly the case here.
        let origin = root.path().join("origin.git");
        git(&work, &["remote", "add", "origin", &origin.to_string_lossy()]);
        git(&work, &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);

        let head = capture(&work, &["rev-parse", "HEAD"]);
        Self { root, head }
    }

    /// The working clone — the coordinator's working directory.
    pub fn work_dir(&self) -> PathBuf {
        self.root.path().join("work")
    }

    /// The commit a scenario seals against, as hex.
    pub fn head(&self) -> &str {
        &self.head
    }

    /// A second commit on the working clone, for a scenario that needs a
    /// distinct subject.
    ///
    /// # Panics
    /// Any git step failed.
    pub fn commit_another(&self, name: &str) -> String {
        let work = self.work_dir();
        fs::write(work.join(name), format!("{name}\n")).unwrap();
        git(&work, &["add", "--all"]);
        git(&work, &["commit", "--quiet", "--message", name]);
        git(&work, &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);
        capture(&work, &["rev-parse", "HEAD"])
    }

    /// Every scratch worktree git currently has registered — a scenario's
    /// leak check, since a dispatch that never released its worktree leaves an
    /// admin entry behind.
    ///
    /// # Panics
    /// The git step failed.
    pub fn registered_worktrees(&self) -> Vec<String> {
        capture(&self.work_dir(), &["worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(str::to_owned)
            .collect()
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&output.stderr));
}

fn capture(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
