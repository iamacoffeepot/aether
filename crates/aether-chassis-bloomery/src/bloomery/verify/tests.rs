//! Closure-key coverage the module owns: the encoding tripwire, and the
//! "moves when and only when a hashed input moves" contract.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use super::{ClosureKey, ClosureKeyError, closure_key};

/// Tripwire: the encoding (domain tag, field order, which inputs join) cannot
/// drift without this hex moving. Computed over the seeded fixture, not a
/// restatement of a `const`.
const ALPHA_FIXTURE_KEY: &str = "d655c5e75b31987cdaed40ebeed9a4e29190a2e2b2bf7c668ce2a5afe369593f";

#[test]
fn the_fixture_key_is_byte_stable() {
    let repo = fixture();

    let first = closure_key(repo.path(), "alpha").expect("alpha's key");
    let second = closure_key(repo.path(), "alpha").expect("alpha's key again");

    assert_eq!(hex_of(&first), ALPHA_FIXTURE_KEY, "the seeded fixture must hash to the pinned encoding");
    assert_eq!(first, second, "same tree, same package must produce the same key");
}

#[test]
fn disjoint_closures_differ_and_move_only_with_their_inputs() {
    // alpha and gamma share the workspace-wide inputs and nothing else, so a
    // key that hashed the whole checkout would make them collide. beta depends
    // on alpha, so alpha's subtree is in beta's closure.
    let repo = fixture();
    let alpha = closure_key(repo.path(), "alpha").expect("alpha's key");
    let beta = closure_key(repo.path(), "beta").expect("beta's key");
    let gamma = closure_key(repo.path(), "gamma").expect("gamma's key");

    assert_ne!(alpha, gamma, "disjoint package trees must not share a key");
    assert_ne!(beta, gamma, "beta's closure includes alpha, not gamma");
    assert_ne!(alpha, beta, "beta's own tree distinguishes it from alpha");

    rewrite(repo.path(), "crates/alpha/src/lib.rs", "pub fn alpha() -> u8 { 9 }\n");
    commit(repo.path(), "alpha source");
    let alpha_after_self = closure_key(repo.path(), "alpha").expect("alpha's key after its own source moves");
    let beta_after_alpha = closure_key(repo.path(), "beta").expect("beta's key after alpha moves");
    let gamma_after_alpha = closure_key(repo.path(), "gamma").expect("gamma's key after alpha moves");
    assert_ne!(alpha_after_self, alpha, "a closure member's subtree hash must move the key");
    assert_ne!(beta_after_alpha, beta, "a dependency's subtree hash must move the dependent's key");
    assert_eq!(gamma_after_alpha, gamma, "an unrelated package's source must not move gamma");

    rewrite(repo.path(), "README.md", "unrelated\n");
    commit(repo.path(), "readme");
    assert_eq!(
        closure_key(repo.path(), "alpha").expect("alpha's key after an unrelated path moves"),
        alpha_after_self,
        "a path outside the package graph must not move the key",
    );

    rewrite(repo.path(), "Cargo.lock", &format!("{}\n# pin-shift\n", lockfile()));
    commit(repo.path(), "lockfile");
    assert_ne!(
        closure_key(repo.path(), "alpha").expect("alpha's key after the lockfile moves"),
        alpha_after_self,
        "the lockfile joins every closure",
    );
}

#[test]
fn an_unknown_package_is_refused() {
    // A name that is not a workspace member must not hash as an empty closure
    // of just the workspace-wide inputs — that would collide every unknown
    // name onto one key.
    let repo = fixture();

    match closure_key(repo.path(), "delta") {
        Err(ClosureKeyError::UnknownPackage(name)) => assert_eq!(name, "delta"),
        other => panic!("expected UnknownPackage, got {other:?}"),
    }
}

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().expect("a temp dir for the fixture creates");
    write_tree(dir.path());
    git(dir.path(), &["init", "--object-format=sha1", "--quiet"]);
    git(dir.path(), &["config", "user.name", "closure-key"]);
    git(dir.path(), &["config", "user.email", "closure-key@test"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    git(dir.path(), &["config", "core.autocrlf", "false"]);
    commit(dir.path(), "seed");
    dir
}

fn write_tree(root: &Path) {
    fs::create_dir_all(root.join("crates/alpha/src")).expect("alpha's crate dir creates");
    fs::create_dir_all(root.join("crates/beta/src")).expect("beta's crate dir creates");
    fs::create_dir_all(root.join("crates/gamma/src")).expect("gamma's crate dir creates");
    rewrite(
        root,
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/alpha\", \"crates/beta\", \"crates/gamma\"]\n",
    );
    rewrite(root, "Cargo.lock", &lockfile());
    rewrite(root, "crates/alpha/Cargo.toml", &manifest("alpha", None));
    rewrite(root, "crates/alpha/src/lib.rs", "pub fn alpha() -> u8 { 1 }\n");
    rewrite(root, "crates/beta/Cargo.toml", &manifest("beta", Some("alpha")));
    rewrite(root, "crates/beta/src/lib.rs", "pub fn beta() -> u8 { alpha::alpha() }\n");
    rewrite(root, "crates/gamma/Cargo.toml", &manifest("gamma", None));
    rewrite(root, "crates/gamma/src/lib.rs", "pub fn gamma() -> u8 { 2 }\n");
}

fn manifest(name: &str, dep: Option<&str>) -> String {
    match dep {
        Some(dep) => format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{dep} = {{ path = \"../{dep}\" }}\n"
        ),
        None => format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
    }
}

fn lockfile() -> String {
    "\
# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = \"alpha\"
version = \"0.1.0\"

[[package]]
name = \"beta\"
version = \"0.1.0\"
dependencies = [
 \"alpha\",
]

[[package]]
name = \"gamma\"
version = \"0.1.0\"
"
    .to_owned()
}

fn rewrite(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture parent dir creates");
    }
    fs::write(&path, contents).expect("the fixture file writes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("the fixture file is mode 644");
    }
}

fn commit(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "--quiet", "--message", message]);
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git").current_dir(root).args(args).output().expect("git starts");
    assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
}

fn hex_of(key: &ClosureKey) -> String {
    key.as_bytes().iter().fold(String::with_capacity(64), |mut hex, byte| {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}
