//! Shared git-repository builder for chassis scenarios that need a real repo.
//!
//! One place owns init, identity config, the seed commit, a bare clone, and
//! rev-parse. A lane-boundary scenario asks for a working clone with an origin
//! remote; a local-authority scenario asks for a bare clone of the same seed.
//! Chassis-local helpers fold in here; bloomery-git's own test helpers stay
//! that crate's business.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// How the seed repository is presented to the coordinator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Layout {
    /// Working clone plus a bare origin the dispatch can `git fetch` by name.
    Origin,
    /// Bare clone of the seed — the fleet-local authority a source port mounts.
    Bare,
}

/// A scratch git repository: identity, one seed commit, and either a working
/// clone with origin or a bare authority.
pub struct Repo {
    _root: TempDir,
    work: PathBuf,
    bare: PathBuf,
    head: String,
    layout: Layout,
}

/// Builder for [`Repo`]. Defaults match the lane-boundary scratch: a `main`
/// branch, a local identity, and a one-file seed commit.
pub struct RepoBuilder {
    user_name: String,
    user_email: String,
    seed_path: String,
    seed_contents: String,
    seed_files: Vec<(String, String)>,
    seed_message: String,
    branch: String,
    layout: Layout,
}

impl Default for RepoBuilder {
    fn default() -> Self {
        Self {
            user_name: "lane harness".to_owned(),
            user_email: "lane-harness@example.test".to_owned(),
            seed_path: "README.md".to_owned(),
            seed_contents: "the subject a lane-boundary scenario checks out.\n".to_owned(),
            seed_files: Vec::new(),
            seed_message: "subject".to_owned(),
            branch: "main".to_owned(),
            layout: Layout::Origin,
        }
    }
}

impl RepoBuilder {
    /// Committer identity written to the seed repository's local config.
    #[must_use]
    pub fn identity(mut self, name: impl Into<String>, email: impl Into<String>) -> Self {
        self.user_name = name.into();
        self.user_email = email.into();
        self
    }

    /// Path and contents of the single seed file.
    #[must_use]
    pub fn seed_file(mut self, path: impl Into<String>, contents: impl Into<String>) -> Self {
        self.seed_path = path.into();
        self.seed_contents = contents.into();
        self.seed_files.clear();
        self
    }

    /// Replace the seed with a small three-crate workspace so a declared
    /// surface glob (`crates/example-a/**`) is real and two members editing
    /// `crates/example-shared/src/lib.rs` collide textually.
    #[must_use]
    pub fn example_tree(mut self) -> Self {
        self.seed_files = example_project_files();
        self
    }

    /// Commit message for the seed commit.
    #[must_use]
    pub fn seed_message(mut self, message: impl Into<String>) -> Self {
        self.seed_message = message.into();
        self
    }

    /// Working clone with a bare origin remote — the lane-boundary shape.
    #[must_use]
    pub fn with_origin(mut self) -> Self {
        self.layout = Layout::Origin;
        self
    }

    /// Bare clone of the seed — the local-authority shape.
    #[must_use]
    pub fn bare_clone(mut self) -> Self {
        self.layout = Layout::Bare;
        self
    }

    /// Create the repository.
    ///
    /// # Panics
    /// Any git step failed.
    #[must_use]
    pub fn create(self) -> Repo {
        let root = tempfile::tempdir().expect("a temporary repository root");
        match self.layout {
            Layout::Origin => create_origin(root, &self),
            Layout::Bare => create_bare(root, &self),
        }
    }
}

impl Repo {
    /// Lane-boundary scratch: a working clone with a bare origin, seeded so a
    /// `git fetch origin <sha>` resolves locally.
    #[must_use]
    pub fn create() -> Self {
        Self::scratch()
    }

    /// [`create`](Self::create) — the named form a local-authority cell reaches for.
    #[must_use]
    pub fn scratch() -> Self {
        RepoBuilder::default().with_origin().create()
    }

    /// Bare authority: seed a commit, clone it `--bare`, return the bare path
    /// as [`path`](Self::path).
    #[must_use]
    pub fn bare_authority() -> Self {
        RepoBuilder::default()
            .identity("test", "test@example.test")
            .seed_file("README.md", "the sealed subject a local-authority bloom checks out.\n")
            .bare_clone()
            .create()
    }

    /// [`bare_authority`](Self::bare_authority) seeded with the three-crate
    /// example project [`BloomeryHarness::start`](crate::BloomeryHarness::start)
    /// checks out.
    #[must_use]
    pub fn with_example_project() -> Self {
        RepoBuilder::default().identity("test", "test@example.test").example_tree().bare_clone().create()
    }

    /// Start from a custom seed rather than either named shape.
    #[must_use]
    pub fn builder() -> RepoBuilder {
        RepoBuilder::default()
    }

    /// The working clone — the coordinator's working directory on the origin layout.
    #[must_use]
    pub fn work_dir(&self) -> PathBuf {
        self.work.clone()
    }

    /// The bare repository: origin.git on the origin layout, the authority on the
    /// bare layout. A local-authority cell points `authority_repo` here.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.bare
    }

    /// The seed commit as hex.
    #[must_use]
    pub fn head(&self) -> &str {
        &self.head
    }

    /// `git rev-parse` in the repository the coordinator talks to.
    ///
    /// # Panics
    /// The git step failed.
    #[must_use]
    pub fn rev_parse(&self, rev: &str) -> String {
        git_capture(self.git_dir(), &["rev-parse", rev])
    }

    /// Run `git` in the repository the coordinator talks to and return stdout.
    ///
    /// # Panics
    /// The git step failed.
    #[must_use]
    pub fn git(&self, args: &[&str]) -> String {
        git_capture(self.git_dir(), args)
    }

    /// A second commit on the working clone, pushed to origin. Lane-boundary
    /// scenarios that need a distinct subject use this.
    ///
    /// # Panics
    /// Any git step failed, or this is a bare-authority repo with no work clone
    /// the scenario owns.
    #[must_use]
    pub fn commit_another(&self, name: &str) -> String {
        assert!(self.layout == Layout::Origin, "commit_another needs a working clone");
        let work = self.work_dir();
        fs::write(work.join(name), format!("{name}\n")).expect("a second commit's file writes");
        git(&work, &["add", "--all"]);
        git(&work, &["commit", "--quiet", "--message", name]);
        git(&work, &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);
        git_capture(&work, &["rev-parse", "HEAD"])
    }

    /// Every scratch worktree git currently has registered — a scenario's leak
    /// check, since a dispatch that never released its worktree leaves an admin
    /// entry behind.
    ///
    /// # Panics
    /// The git step failed.
    #[must_use]
    pub fn registered_worktrees(&self) -> Vec<String> {
        git_capture(&self.work_dir(), &["worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(str::to_owned)
            .collect()
    }

    fn git_dir(&self) -> &Path {
        match self.layout {
            Layout::Origin => &self.work,
            Layout::Bare => &self.bare,
        }
    }
}

fn create_origin(root: TempDir, spec: &RepoBuilder) -> Repo {
    git(root.path(), &["init", "--quiet", "--bare", "origin.git"]);

    let work = root.path().join("work");
    git(root.path(), &["init", "--quiet", "work"]);
    configure_identity(&work, spec);
    write_seed(&work, spec);
    git(&work, &["add", "--all"]);
    git(&work, &["commit", "--quiet", "--message", &spec.seed_message]);

    let origin = root.path().join("origin.git");
    git(&work, &["remote", "add", "origin", &origin.to_string_lossy()]);
    git(&work, &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);

    let head = git_capture(&work, &["rev-parse", "HEAD"]);
    Repo { _root: root, work, bare: origin, head, layout: Layout::Origin }
}

fn create_bare(root: TempDir, spec: &RepoBuilder) -> Repo {
    let seed = root.path().join("seed");
    fs::create_dir(&seed).expect("the seed working tree creates");
    git(&seed, &["init", "--quiet", "-b", &spec.branch]);
    configure_identity(&seed, spec);
    write_seed(&seed, spec);
    git(&seed, &["add", "--all"]);
    git(&seed, &["commit", "--quiet", "--message", &spec.seed_message]);

    let head = git_capture(&seed, &["rev-parse", "HEAD"]);
    let bare = root.path().join("authority.git");
    let status = Command::new("git")
        .args(["clone", "--bare", "--quiet"])
        .arg(&seed)
        .arg(&bare)
        .status()
        .expect("git clone --bare starts");
    assert!(status.success(), "clone --bare into {}", bare.display());

    Repo { _root: root, work: seed, bare, head, layout: Layout::Bare }
}

fn configure_identity(dir: &Path, spec: &RepoBuilder) {
    git(dir, &["config", "--local", "user.name", &spec.user_name]);
    git(dir, &["config", "--local", "user.email", &spec.user_email]);
}

fn write_seed(dir: &Path, spec: &RepoBuilder) {
    let files = if spec.seed_files.is_empty() {
        vec![(spec.seed_path.clone(), spec.seed_contents.clone())]
    } else {
        spec.seed_files.clone()
    };
    for (relative, contents) in files {
        let path = dir.join(&relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("a seed file's parent creates");
        }
        fs::write(path, contents).expect("a seed file writes");
    }
}

fn example_project_files() -> Vec<(String, String)> {
    vec![
        (
            "Cargo.toml".into(),
            "[workspace]\nresolver = \"3\"\nmembers = [\"crates/example-a\", \"crates/example-b\", \"crates/example-shared\"]\n"
                .into(),
        ),
        (
            "crates/example-a/Cargo.toml".into(),
            "[package]\nname = \"example-a\"\nversion = \"0.0.0\"\nedition = \"2024\"\n".into(),
        ),
        ("crates/example-a/src/lib.rs".into(), "pub fn a() -> u8 { 1 }\n".into()),
        (
            "crates/example-b/Cargo.toml".into(),
            "[package]\nname = \"example-b\"\nversion = \"0.0.0\"\nedition = \"2024\"\n".into(),
        ),
        ("crates/example-b/src/lib.rs".into(), "pub fn b() -> u8 { 1 }\n".into()),
        (
            "crates/example-shared/Cargo.toml".into(),
            "[package]\nname = \"example-shared\"\nversion = \"0.0.0\"\nedition = \"2024\"\n".into(),
        ),
        ("crates/example-shared/src/lib.rs".into(), "pub fn shared() -> u8 { 1 }\n".into()),
    ]
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git").current_dir(dir).args(args).output().expect("git starts");
    assert!(output.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&output.stderr));
}

fn git_capture(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git").current_dir(dir).args(args).output().expect("git starts");
    assert!(output.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
