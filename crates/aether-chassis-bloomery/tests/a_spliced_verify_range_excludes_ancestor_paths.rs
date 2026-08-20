//! A spliced member's Verify range is its own construct base, not the bloom
//! sealed base (#5277). Containment diffs that range, so the ancestor's files
//! must not appear in it.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn a_member_with_one_spliced_ancestor_is_contained_against_its_own_delta() {
    // Pre-fix: the gate diffs sealed-base..checkout, so every path the ancestor
    // introduced is a containment finding against this member's surface.
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    write_tree(dir.path(), "pub fn owned() -> u8 { 1 }\n", "pub fn other() -> u8 { 1 }\n");
    let sealed_base = commit(dir.path(), "sealed base");
    write_tree(dir.path(), "pub fn owned() -> u8 { 1 }\n", "pub fn other() -> u8 { 2 }\n");
    let ancestor = commit(dir.path(), "ancestor member");
    write_tree(dir.path(), "pub fn owned() -> u8 { 2 }\n", "pub fn other() -> u8 { 2 }\n");
    commit(dir.path(), "this member");

    let against_sealed = name_only(dir.path(), &sealed_base);
    assert!(
        against_sealed.iter().any(|path| path == "crates/other/src/lib.rs"),
        "the bloom-base range spans the ancestor's commits: {against_sealed:?}",
    );

    let against_construct = name_only(dir.path(), &ancestor);
    assert_eq!(
        against_construct,
        ["crates/owned/src/lib.rs"],
        "the construct-base range names only this member's change",
    );
    assert!(
        !against_construct.iter().any(|path| path == "crates/other/src/lib.rs"),
        "containment must not name the ancestor's files",
    );
}

fn init_repo(root: &Path) {
    run(root, &["init", "--object-format=sha1", "--quiet"]);
    run(root, &["config", "user.name", "splice-range"]);
    run(root, &["config", "user.email", "splice-range@test"]);
    run(root, &["config", "commit.gpgsign", "false"]);
    run(root, &["config", "core.autocrlf", "false"]);
}

fn write_tree(root: &Path, owned: &str, other: &str) {
    let owned_path = root.join("crates/owned/src/lib.rs");
    let other_path = root.join("crates/other/src/lib.rs");
    fs::create_dir_all(owned_path.parent().unwrap()).unwrap();
    fs::create_dir_all(other_path.parent().unwrap()).unwrap();
    fs::write(owned_path, owned).unwrap();
    fs::write(other_path, other).unwrap();
}

fn commit(root: &Path, message: &str) -> String {
    run(root, &["add", "-A"]);
    run(root, &["commit", "--quiet", "--message", message]);
    let output = Command::new("git").current_dir(root).args(["rev-parse", "HEAD"]).output().unwrap();
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn name_only(root: &Path, base: &str) -> Vec<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["diff", "--name-only", "--no-renames", "--no-ext-diff", "-z", base, "HEAD"])
        .output()
        .unwrap();
    assert!(output.status.success(), "git diff failed: {}", String::from_utf8_lossy(&output.stderr));
    output
        .stdout
        .split(|&byte| byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).unwrap())
        .collect()
}

fn run(root: &Path, args: &[&str]) {
    let output = Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
}
