//! Whether the artifacts `dist` would build are already the ones it would
//! produce (#5111 follow-on).
//!
//! `cargo xtask dist` cross-builds every component package in its own cargo
//! invocation — twenty-odd of them, plus the behaviour variants and the chassis
//! bins — and it is the single largest step in a verify lane even when every
//! one of those invocations turns out to have nothing to do. What cargo cannot
//! skip is the per-invocation work of deciding that: resolve the workspace,
//! stat the tree, walk the unit graph, once per package.
//!
//! So the whole build is keyed on what it reads. The key is a digest over every
//! workspace member's sources, the workspace manifests, and the toolchain and
//! selectors the build runs under; it is stamped beside the wasm artifacts, and
//! a run whose key matches the stamp — and whose artifacts are all still on
//! disk — skips straight to assembling `dist/`.
//!
//! Every uncertainty resolves toward building. A file that cannot be read, a
//! directory that cannot be walked, a stamp that cannot be parsed, a tree
//! deeper than the walk's cap, an artifact that has gone missing: each yields
//! "not fresh", never a skip. The failure this refuses to have is a gate
//! judging a candidate against wasm built from something else.

use std::fs;
use std::path::{Path, PathBuf};

use cargo_metadata::Metadata;
use sha2::{Digest, Sha256};

use crate::cargo::{Profile, command};

/// The file the key is stamped into, under the wasm profile directory — so the
/// stamp lives and dies with the artifacts it describes. A target directory
/// evicted for budget takes its stamp with it, and the next run rebuilds.
const STAMP: &str = ".aether-dist-key";

/// How deep the source walk goes under one workspace member before it gives up
/// and reports "cannot key this tree" (and so builds). Deeper than any crate
/// here; a bound rather than a limit, because a walk over a tree it does not
/// own must not be able to run forever.
const MAX_DEPTH: usize = 32;

/// Directory names the walk never descends into: build output and version
/// control, neither of which is an input to what `dist` builds.
const SKIPPED: [&str; 3] = ["target", ".git", "dist"];

/// The digest of everything one `dist` build reads, rendered hex.
#[derive(PartialEq, Eq)]
pub(super) struct BuildKey(String);

/// The key for this invocation, or `None` when the tree cannot be read
/// completely enough to key it.
///
/// The inputs are the workspace members' own source trees (which is what
/// `metadata` enumerates), the workspace manifests beside them, and the
/// selectors that decide what gets built from them: the profile, the flag that
/// drops the chassis bins, and the compiler's own version string.
pub(super) fn key(metadata: &Metadata, profile: Profile, no_bins: bool) -> Option<BuildKey> {
    let mut digest = Sha256::new();
    digest.update(profile.as_str().as_bytes());
    digest.update([u8::from(no_bins)]);
    digest.update(rustc_version()?.as_bytes());

    let workspace_root = metadata.workspace_root.as_std_path();
    let mut roots: Vec<PathBuf> = metadata
        .workspace_packages()
        .iter()
        .filter_map(|package| package.manifest_path.as_std_path().parent().map(Path::to_path_buf))
        .collect();
    roots.sort();
    roots.dedup();

    absorb_sources(&mut digest, workspace_root, &roots)?;
    Some(BuildKey(format!("{:x}", digest.finalize())))
}

/// Fold the workspace manifests and every member's source tree into `digest`.
///
/// Content, never metadata: a checkout materialized into a directory git has
/// not written before restamps every mtime, and keying on those would rebuild
/// exactly when nothing changed — which is the cost this exists to remove.
fn absorb_sources(digest: &mut Sha256, workspace_root: &Path, roots: &[PathBuf]) -> Option<()> {
    for file in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
        let path = workspace_root.join(file);
        if path.exists() {
            absorb(digest, workspace_root, &path)?;
        }
    }
    for root in roots {
        absorb_tree(digest, workspace_root, root)?;
    }
    Some(())
}

/// Whether `key` is what the artifacts on disk were built from — the stamp
/// matches it, and every artifact the caller named is still there.
///
/// Both halves, because they fail independently: the sources decide whether the
/// artifacts would differ, and the artifacts decide whether there are any.
pub(super) fn is_current(key: &BuildKey, wasm_profile_dir: &Path, artifacts: &[PathBuf]) -> bool {
    fs::read_to_string(wasm_profile_dir.join(STAMP)).is_ok_and(|stamped| stamped.trim() == key.0)
        && artifacts.iter().all(|artifact| artifact.exists())
}

/// Stamp `key` beside the artifacts it describes, after they are built.
///
/// Best effort: a stamp that cannot be written costs the next run a rebuild it
/// would otherwise have skipped, which is the direction this errs in anyway.
pub(super) fn record(key: &BuildKey, wasm_profile_dir: &Path) {
    let path = wasm_profile_dir.join(STAMP);
    if let Err(error) = fs::create_dir_all(wasm_profile_dir).and_then(|()| fs::write(&path, &key.0)) {
        eprintln!("dist: could not stamp the build key at {} ({error}); the next run will rebuild", path.display());
    }
}

/// Remove the stamp, so the next run rebuilds whatever this one is about to
/// leave half-built.
pub(super) fn invalidate(wasm_profile_dir: &Path) {
    let _ = fs::remove_file(wasm_profile_dir.join(STAMP));
}

/// Fold one file's path (relative to the workspace, so the tree keys the same
/// from any checkout) and its bytes into `digest`.
fn absorb(digest: &mut Sha256, workspace_root: &Path, path: &Path) -> Option<()> {
    digest.update(path.strip_prefix(workspace_root).unwrap_or(path).to_str()?.as_bytes());
    digest.update(fs::read(path).ok()?);
    Some(())
}

/// Fold a whole directory tree into `digest`, in a stable order.
///
/// Iterative over an explicit stack rather than recursive: the depth is a
/// property of a tree this does not own, and a stack frame per level is the one
/// resource a walk cannot be given back.
fn absorb_tree(digest: &mut Sha256, workspace_root: &Path, root: &Path) -> Option<()> {
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > MAX_DEPTH {
            return None;
        }
        let mut entries: Vec<PathBuf> =
            fs::read_dir(&directory).ok()?.filter_map(|entry| Some(entry.ok()?.path())).collect();
        entries.sort();
        for entry in entries {
            let name = entry.file_name().and_then(|name| name.to_str())?;
            if SKIPPED.contains(&name) {
                continue;
            }
            if entry.is_dir() {
                pending.push((entry, depth + 1));
            } else {
                absorb(digest, workspace_root, &entry)?;
            }
        }
    }
    Some(())
}

/// The compiler's own version line, so a toolchain bump rebuilds.
fn rustc_version() -> Option<String> {
    let probed = command().args(["--version", "--verbose"]).output().ok()?;
    probed.status.success().then(|| String::from_utf8_lossy(&probed.stdout).into_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::env::temp_dir;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::id;
    use std::slice::from_ref;

    use sha2::{Digest, Sha256};

    use super::{BuildKey, absorb_sources, is_current};

    /// The digest of one tree, as `key` folds it — without the toolchain probe,
    /// which is the host's answer rather than the tree's.
    fn digest_of(root: &Path) -> String {
        let mut digest = Sha256::new();
        absorb_sources(&mut digest, root, &[root.join("crates").join("member")]).expect("the fixture tree reads");
        format!("{:x}", digest.finalize())
    }

    /// A workspace-shaped fixture: a root manifest and one member with a source
    /// file, plus the build output a real one accumulates beside them.
    fn fixture(root: &Path) {
        fs::create_dir_all(root.join("crates/member/src")).unwrap();
        fs::create_dir_all(root.join("crates/member/target/debug")).unwrap();
        fs::write(root.join("Cargo.toml"), b"[workspace]\n").unwrap();
        fs::write(root.join("crates/member/src/lib.rs"), b"pub fn one() -> u8 { 1 }\n").unwrap();
        fs::write(root.join("crates/member/target/debug/libmember.rlib"), b"artifact").unwrap();
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = temp_dir().join(format!("aether-dist-freshness-{tag}-{}", id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn the_key_follows_content_and_ignores_when_the_file_was_written() {
        // Tripwire: the whole point of keying the build is to skip it when a
        // fresh checkout wrote the same bytes at a new mtime — which is what
        // cargo cannot do. A key that moved with the timestamps would rebuild
        // every time and a key that ignored content would skip a real change.
        let root = temp_root("content");
        fixture(&root);
        let first = digest_of(&root);

        let source = root.join("crates/member/src/lib.rs");
        let bytes = fs::read(&source).unwrap();
        fs::remove_file(&source).unwrap();
        fs::write(&source, &bytes).unwrap();
        assert_eq!(digest_of(&root), first, "rewriting identical bytes is not a change");

        fs::write(&source, b"pub fn one() -> u8 { 2 }\n").unwrap();
        assert_ne!(digest_of(&root), first, "a changed byte must reach the key");

        fs::write(&source, &bytes).unwrap();
        fs::write(root.join("crates/member/src/extra.rs"), b"pub fn two() {}\n").unwrap();
        assert_ne!(digest_of(&root), first, "a new file must reach the key");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_output_under_a_member_is_not_an_input_to_the_build() {
        // Tripwire: a member's own `target/` is written by the build this key
        // gates. Folding it in would make the key differ from itself the moment
        // the build it describes finished, and the skip would never fire.
        let root = temp_root("output");
        fixture(&root);
        let first = digest_of(&root);

        fs::write(root.join("crates/member/target/debug/libmember.rlib"), b"rebuilt, differently").unwrap();
        assert_eq!(digest_of(&root), first, "the artifacts the build writes are not what it reads");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_matching_stamp_over_a_missing_artifact_is_not_fresh() {
        // Tripwire: the sources decide whether the artifacts would differ, and
        // the artifacts decide whether there are any. A target directory the
        // janitor evicted for budget leaves the stamp's own tree gone with it,
        // but an artifact removed on its own would otherwise be skipped past —
        // and the gate would assemble a dist/ around a file that is not there.
        let root = temp_root("stamp");
        fs::create_dir_all(&root).unwrap();
        let key = BuildKey(String::from("abc123"));
        fs::write(root.join(super::STAMP), &key.0).unwrap();

        let present = root.join("component.wasm");
        fs::write(&present, b"wasm").unwrap();
        assert!(is_current(&key, &root, from_ref(&present)), "stamp and artifact agree");
        assert!(!is_current(&key, &root, &[present, root.join("gone.wasm")]), "a missing artifact rebuilds");

        let _ = fs::remove_dir_all(&root);
    }
}
