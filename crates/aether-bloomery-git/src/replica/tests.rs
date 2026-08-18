//! Allowlist and push-path tests for the source replica.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use super::{GitSourceReplica, ReplicaError, SourceReplica, published_refspecs};
use crate::mainline::MainlineRef;

#[test]
fn published_refspecs_are_exactly_mainline_plus_tags() {
    // Tripwire: the replica publishes only the configured mainline (force) and
    // tags (fast-forward). A slip that added --mirror, or that treated a bloom /
    // claim / candidate / attempt / checkpoint ref as public, would leak the
    // coordination namespace onto GitHub.
    let mainline = MainlineRef::new("refs/heads/main");
    let refs = [
        "refs/heads/main",
        "refs/tags/v1.0",
        "refs/tags/nightly",
        "refs/heads/bloom/abcdef012345/candidate/wp",
        "refs/heads/bloom/abcdef012345/attempt/1",
        "refs/heads/bloom/abcdef012345/checkpoint/deadbeef",
        "refs/heads/bloom/abcdef012345/member-checkpoint/wp",
        "refs/heads/bloomery/claims/wp",
        "refs/heads/bloomery/admission/abcdef012345",
        "refs/heads/feature",
    ];
    let specs = published_refspecs(&mainline, refs);
    let args: Vec<String> = specs.iter().map(super::allowlist::PublishedRefspec::as_arg).collect();

    assert_eq!(
        args,
        ["+refs/heads/main:refs/heads/main", "refs/tags/v1.0:refs/tags/v1.0", "refs/tags/nightly:refs/tags/nightly"],
        "only mainline (forced) and tags (ff-only) are published",
    );
    assert!(args.iter().all(|arg| !arg.contains("--mirror")), "the allowlist never mints --mirror");
    assert!(
        args.iter().filter(|arg| arg.starts_with('+')).all(|arg| arg.contains("refs/heads/main")),
        "force is reserved for the configured mainline",
    );
}

#[test]
fn push_args_never_include_mirror_and_force_only_the_mainline() {
    let mainline = MainlineRef::new("refs/heads/main");
    let specs = published_refspecs(&mainline, ["refs/heads/main", "refs/tags/v1"]);
    let args = GitSourceReplica::push_args("https://github.com/octo/shadow.git", &specs);
    assert_eq!(
        args,
        [
            "push",
            "--porcelain",
            "https://github.com/octo/shadow.git",
            "+refs/heads/main:refs/heads/main",
            "refs/tags/v1:refs/tags/v1",
        ]
    );
    assert!(!args.iter().any(|arg| arg == "--mirror" || arg == "--force" || arg == "--force-with-lease"));
}

#[test]
fn a_real_push_copies_mainline_and_tags_and_leaves_coordination_refs_behind() {
    let (_root, authority, replica) = paired_repos();
    seed_ref(&authority, "refs/heads/bloom/abcdef012345/candidate/wp");
    seed_ref(&authority, "refs/heads/bloom/abcdef012345/attempt/1");
    seed_ref(&authority, "refs/heads/bloom/abcdef012345/checkpoint/deadbeef");
    seed_ref(&authority, "refs/heads/bloomery/claims/wp");
    run_git(&authority, &["tag", "v1.0"]);

    GitSourceReplica::new(&authority, replica.to_str().expect("utf-8"), MainlineRef::default(), "")
        .publish()
        .expect("allowlisted push succeeds");

    let published = GitSourceReplica::list_refs(&replica).expect("replica refs");
    assert!(published.iter().any(|name| name == "refs/heads/main"), "mainline must arrive: {published:?}");
    assert!(published.iter().any(|name| name == "refs/tags/v1.0"), "tags must arrive: {published:?}");
    assert!(
        published.iter().all(|name| !name.contains("/bloom/")
            && !name.contains("bloomery/claims")
            && !name.contains("/candidate/")
            && !name.contains("/attempt/")
            && !name.contains("/checkpoint/")),
        "coordination refs must stay off the replica: {published:?}"
    );
}

#[test]
fn a_protected_mainline_force_is_classified_as_rejected() {
    let (_root, authority, replica) = paired_repos();
    advance_mainline(&authority);
    install_rejecting_hook(&replica);
    let error = GitSourceReplica::new(&authority, replica.to_str().expect("utf-8"), MainlineRef::default(), "")
        .publish()
        .expect_err("a hook that refuses the force is a rejected force");
    assert!(
        matches!(error, ReplicaError::ForceRejected(_)),
        "protected-branch refusal must be ForceRejected, got {error}"
    );
}

fn paired_repos() -> (TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().expect("root");
    let seed = root.path().join("seed");
    fs::create_dir(&seed).expect("seed");
    run_git(&seed, &["init", "--quiet", "-b", "main"]);
    run_git(&seed, &["config", "--local", "user.name", "test"]);
    run_git(&seed, &["config", "--local", "user.email", "test@example.test"]);
    fs::write(seed.join("README"), "subject\n").expect("file");
    run_git(&seed, &["add", "--all"]);
    run_git(&seed, &["commit", "--quiet", "--message", "subject"]);

    let authority = root.path().join("authority.git");
    let replica = root.path().join("replica.git");
    assert!(
        Command::new("git")
            .args(["clone", "--bare", "--quiet"])
            .arg(&seed)
            .arg(&authority)
            .status()
            .expect("clone authority")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["clone", "--bare", "--quiet"])
            .arg(&seed)
            .arg(&replica)
            .status()
            .expect("clone replica")
            .success()
    );
    (root, authority, replica)
}

fn seed_ref(repo: &Path, name: &str) {
    let head = git_stdout(repo, &["rev-parse", "HEAD"]);
    run_git(repo, &["update-ref", name, &head]);
}

fn advance_mainline(repo: &Path) {
    let tree = git_stdout(repo, &["rev-parse", "HEAD^{tree}"]);
    let parent = git_stdout(repo, &["rev-parse", "HEAD"]);
    let output = Command::new("git")
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.test")
        .args(["commit-tree", &tree, "-p", &parent, "-m", "advance"])
        .output()
        .expect("commit-tree");
    assert!(output.status.success(), "commit-tree: {}", String::from_utf8_lossy(&output.stderr));
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    run_git(repo, &["update-ref", "refs/heads/main", &commit]);
}

fn install_rejecting_hook(replica: &Path) {
    let hook = replica.join("hooks").join("pre-receive");
    fs::create_dir_all(hook.parent().expect("hooks")).expect("hooks dir");
    fs::write(
        &hook,
        "#!/bin/sh\necho 'remote: error: GH006: Protected branch update failed' >&2\necho '! [remote rejected] main -> main (protected branch hook declined)' >&2\nexit 1\n",
    )
    .expect("hook");
    let mut perms = fs::metadata(&hook).expect("hook meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook, perms).expect("chmod hook");
}

fn run_git(repo: &Path, args: &[&str]) {
    let status = Command::new("git").current_dir(repo).args(args).status().expect("git");
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git").current_dir(repo).args(args).output().expect("git");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
