//! The lane over scratch Git repositories and a scratch journal.
//!
//! Every repository is built with the `git` binary through the index
//! (`update-index --cacheinfo`), so modes and symlinks do not depend on the
//! host filesystem.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path as HostPath, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{env, process, str};

use aether_bloomery_journal::{AppendError, Batch, Journal};
use aether_bloomery_kinds::{
    Digest, EncodedArtifact, Head, Name, Node, OpaqueBytes, Path, Publish, PublishResult, Ref, Seq, Tree,
};
use anyhow::{Context, Result, bail};

use super::batch::split;
use super::publish::{Stage, publish};
use super::read::read_commit;
use super::tree::{Refusal, Rule, build};
use crate::git;

/// A directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Result<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("xtask-import-commit-{label}-{}-{unique}", process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &HostPath {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Run git in `repo` and require success.
fn git_ok(repo: &HostPath, args: &[&str]) -> Result<String> {
    Ok(git::run_ok(repo, args)?)
}

/// A scratch repository whose one commit holds `entries`, each
/// `(mode, path, content)`; a `160000` entry's content is the gitlink's sha.
fn commit(label: &str, entries: &[(&str, &str, &[u8])]) -> Result<(Scratch, String)> {
    let repo = Scratch::new(label)?;
    git_ok(repo.path(), &["init", "-q"])?;

    for (index, (mode, path, content)) in entries.iter().enumerate() {
        let oid = if *mode == "160000" {
            str::from_utf8(content)?.to_owned()
        } else {
            let staged = repo.path().join(format!(".content-{index}"));
            fs::write(&staged, content)?;
            git_ok(repo.path(), &["hash-object", "-w", &staged.to_string_lossy()])?
        };
        git_ok(repo.path(), &["update-index", "--add", "--cacheinfo", &format!("{mode},{oid},{path}")])?;
    }

    let tree = git_ok(repo.path(), &["write-tree"])?;
    let sha = git_ok(
        repo.path(),
        &[
            "-c",
            "user.name=import",
            "-c",
            "user.email=import@example.invalid",
            "commit-tree",
            "--no-gpg-sign",
            &tree,
            "-m",
            "import",
        ],
    )?;
    Ok((repo, sha))
}

/// The refusal a commit's build stopped on.
fn refusal(label: &str, entries: &[(&str, &str, &[u8])]) -> Result<Refusal> {
    let (repo, sha) = commit(label, entries)?;
    match build(&read_commit(repo.path(), &sha)?) {
        Ok(_) => bail!("the build accepted the commit"),
        Err(error) => error.downcast::<Refusal>().context("the build failed for another reason"),
    }
}

/// Stages a publish exactly as the journal actor's `on_publish` does.
struct JournalStage(Journal);

impl Stage for JournalStage {
    fn stage(&mut self, request: &Publish) -> Result<PublishResult> {
        let mut batch = Batch::new();
        let artifacts = request.artifacts().iter().map(|artifact| batch.stage_artifact(artifact.clone())).collect();
        for moved in request.moves() {
            batch.push_event(moved, None)?;
        }

        Ok(match self.0.append(Seq(request.expected_seq()), &batch) {
            Ok(range) => PublishResult::Committed { head: range.end.0.saturating_sub(1), artifacts },
            Err(AppendError::HeadMoved { actual }) => PublishResult::Conflict { actual: actual.0 },
            Err(error) => PublishResult::Err { message: error.to_string() },
        })
    }
}

/// A two-level commit with every blob mode.
const FIXTURE: &[(&str, &str, &[u8])] = &[
    ("100644", "plain.txt", b"plain\n"),
    ("100755", "run.sh", b"#!/bin/sh\n"),
    ("120000", "link", b"nested/deep.txt"),
    ("100644", "nested/deep.txt", b"deep\n"),
    ("100644", "nested/inner/leaf.txt", b"leaf\n"),
];

#[test]
fn a_commit_builds_the_tree_written_by_hand() -> Result<()> {
    let (repo, sha) = commit("modes", FIXTURE)?;
    let listing = read_commit(repo.path(), &sha)?;
    assert_eq!(listing.commit, sha);

    let inner =
        Tree::new(BTreeMap::from([(Name::new("leaf.txt")?, Node::File(Ref::<OpaqueBytes>::of_bytes(b"leaf\n")))]));
    let nested = Tree::new(BTreeMap::from([
        (Name::new("deep.txt")?, Node::File(Ref::<OpaqueBytes>::of_bytes(b"deep\n"))),
        (Name::new("inner")?, Node::Directory(Ref::of_encoded(&inner)?)),
    ]));
    let root = Tree::new(BTreeMap::from([
        (Name::new("plain.txt")?, Node::File(Ref::<OpaqueBytes>::of_bytes(b"plain\n"))),
        (Name::new("run.sh")?, Node::Executable(Ref::<OpaqueBytes>::of_bytes(b"#!/bin/sh\n"))),
        (Name::new("link")?, Node::Symlink(Path::new("nested/deep.txt")?)),
        (Name::new("nested")?, Node::Directory(Ref::of_encoded(&nested)?)),
    ]));
    assert_eq!(build(&listing)?.root, Ref::of_encoded(&root)?.digest());
    Ok(())
}

#[test]
fn a_gitlink_or_an_absolute_symlink_refuses_and_names_its_path() -> Result<()> {
    let gitlink = refusal(
        "gitlink",
        &[("100644", "kept.txt", b"kept\n"), ("160000", "sub", b"0123456789abcdef0123456789abcdef01234567")],
    )?;
    assert_eq!(gitlink.path, "sub");
    assert!(matches!(gitlink.rule, Rule::Mode(ref mode) if mode == "160000"), "{gitlink}");

    let absolute = refusal("absolute", &[("120000", "etc/passwd-link", b"/etc/passwd")])?;
    assert_eq!(absolute.path, "etc/passwd-link");
    assert!(matches!(absolute.rule, Rule::Target(_)), "{absolute}");
    Ok(())
}

/// The fixture's artifacts split under a budget of its largest artifact, which
/// forces several batches; returns the root digest beside them.
fn small_batches(label: &str) -> Result<(Digest, Vec<Vec<EncodedArtifact>>)> {
    let (repo, sha) = commit(label, FIXTURE)?;
    let built = build(&read_commit(repo.path(), &sha)?)?;

    let budget_bytes =
        built.artifacts.iter().map(|staged| staged.artifact.bytes().len()).max().context("no artifacts")?;
    let batches = split(built.artifacts, budget_bytes)?;
    assert!(batches.len() > 1, "the budget forced {} batch", batches.len());
    for batch in &batches {
        assert!(batch.iter().map(|artifact| artifact.bytes().len()).sum::<usize>() <= budget_bytes);
    }
    Ok((built.root, batches))
}

/// A journal opened under a scratch root, removed with it.
fn scratch_journal() -> Result<(Scratch, JournalStage)> {
    let root = Scratch::new("journal")?;
    let journal = Journal::open(&root.path().join("journal"))?;
    Ok((root, JournalStage(journal)))
}

#[test]
fn batches_under_a_small_budget_verify_in_order_and_move_no_head() -> Result<()> {
    let (root, batches) = small_batches("batches")?;
    let (_scratch, mut stage) = scratch_journal()?;

    let head = stage.0.head()?;
    publish(&mut stage, head.0, batches)?;

    assert!(stage.0.get::<Tree>(&root)?.is_some());
    assert_eq!(stage.0.head()?, head);
    Ok(())
}

#[test]
fn a_stale_fence_is_resent_until_every_batch_commits() -> Result<()> {
    let (root, batches) = small_batches("conflict")?;
    let (_scratch, mut stage) = scratch_journal()?;

    let fence = stage.0.head()?;
    let seeded = stage.stage(&Publish::head(&Head::<Tree>::new("seed"), &Tree::empty(), fence.0)?)?;
    assert!(matches!(seeded, PublishResult::Committed { .. }), "{seeded:?}");

    publish(&mut stage, fence.0, batches)?;

    assert!(stage.0.get::<Tree>(&root)?.is_some());
    assert_eq!(stage.0.head()?, Seq(fence.0 + 1));
    Ok(())
}
