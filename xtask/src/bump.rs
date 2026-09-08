//! `cargo xtask bump <version>` — move `[workspace.package] version` and
//! regenerate every lockfile the move invalidates.
//!
//! The root `Cargo.lock` is the obvious one. The trap is the second: `fuzz` is
//! a `[workspace] exclude` member with its own standalone workspace and its own
//! `Cargo.lock`, which records the path-dependency versions of `aether-codec`
//! and `aether-data`. A root `cargo update` never opens it, so a hand-run bump
//! leaves it pinned to the previous version and the fuzz build resolves against
//! a version that no longer exists. Rather than name `fuzz` here, this command
//! reads the root manifest's `exclude` list and re-locks every excluded crate
//! that carries a lockfile of its own — the miss is structural, so the fix is.

use std::path::{Path, PathBuf};
use std::{fs, process::Command};

use anyhow::{Context, Result, bail};
use cargo_metadata::MetadataCommand;
use cargo_metadata::semver::Version;
use clap::Args;

use crate::cargo::{command, run_status};

#[derive(Args)]
pub struct BumpArgs {
    /// The new workspace version — a semver version such as `0.4.0-alpha`.
    /// Anything semver refuses is refused here.
    version: String,
    /// Print the manifest edit and the lockfile commands without writing a
    /// file or invoking cargo.
    #[arg(long)]
    dry_run: bool,
}

pub fn run(args: &BumpArgs) -> Result<()> {
    let version = Version::parse(&args.version)
        .with_context(|| format!("`{}` is not a semver version — bump takes e.g. `0.4.0-alpha`", args.version))?;

    let metadata = MetadataCommand::new().no_deps().exec().context("run cargo metadata")?;
    let workspace_root = metadata.workspace_root.as_std_path().to_path_buf();
    let manifest_path = workspace_root.join("Cargo.toml");

    let manifest = fs::read_to_string(&manifest_path).with_context(|| format!("read {}", manifest_path.display()))?;
    let (bumped, previous) = rewrite_workspace_version(&manifest, &version)?;
    if previous == version.to_string() {
        bail!("the workspace is already at {previous} — nothing to bump");
    }

    // The root manifest leads; each excluded crate carrying its own lockfile
    // follows, and every one of them re-locks the same way.
    let mut manifests = vec![manifest_path.clone()];
    manifests.extend(excluded_lockfile_manifests(&manifest, &workspace_root)?);

    println!("Cargo.toml: [workspace.package] version {previous} -> {version}");
    for lock_manifest in &manifests {
        println!(
            "{}: cargo update --workspace --manifest-path {}",
            lockfile_of(lock_manifest).display(),
            lock_manifest.display()
        );
    }

    if args.dry_run {
        println!("(dry run — nothing written)");
        return Ok(());
    }

    fs::write(&manifest_path, bumped).with_context(|| format!("write {}", manifest_path.display()))?;
    for lock_manifest in &manifests {
        relock(&workspace_root, lock_manifest)?;
    }
    Ok(())
}

/// Replace the `version` value inside the root manifest's `[workspace.package]`
/// table, returning the rewritten text and the version it replaced. The edit is
/// line-based on purpose: round-tripping the manifest through a TOML serializer
/// would reflow comments and table order across a file that is mostly prose
/// about why each dependency is pinned.
///
/// Only `[workspace.package]`'s own `version` moves. Every other `version =`
/// in the file — the `[workspace.dependencies]` requirements, an excluded
/// crate's literal — belongs to something else.
fn rewrite_workspace_version(manifest: &str, version: &Version) -> Result<(String, String)> {
    let mut lines: Vec<String> = manifest.lines().map(str::to_owned).collect();
    let mut in_workspace_package = false;

    for line in &mut lines {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_workspace_package = trimmed == "[workspace.package]";
            continue;
        }
        if !in_workspace_package {
            continue;
        }
        let Some(value) = trimmed.strip_prefix("version") else {
            continue;
        };
        let Some(value) = value.trim_start().strip_prefix('=') else {
            continue;
        };
        let previous = value.trim().trim_matches('"').to_owned();
        *line = format!("version = \"{version}\"");
        let mut bumped = lines.join("\n");
        if manifest.ends_with('\n') {
            bumped.push('\n');
        }
        return Ok((bumped, previous));
    }

    bail!("no `version` under `[workspace.package]` in the root manifest")
}

/// The manifests of `[workspace] exclude` entries that carry their own
/// `Cargo.lock`. An excluded crate without a lockfile has nothing a bump can
/// invalidate; one with a lockfile records the workspace crates it path-depends
/// on by version and goes stale the moment the workspace version moves.
fn excluded_lockfile_manifests(manifest: &str, workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let document: toml::Value = toml::from_str(manifest).context("parse the root manifest")?;
    let excluded = document.get("workspace").and_then(|w| w.get("exclude")).and_then(toml::Value::as_array);

    let mut manifests = Vec::new();
    for entry in excluded.into_iter().flatten().filter_map(toml::Value::as_str) {
        let candidate = workspace_root.join(entry).join("Cargo.toml");
        if candidate.is_file() && lockfile_of(&candidate).is_file() {
            manifests.push(candidate);
        }
    }
    Ok(manifests)
}

/// The lockfile beside a manifest.
fn lockfile_of(manifest: &Path) -> PathBuf {
    manifest.with_file_name("Cargo.lock")
}

/// Re-lock the workspace `manifest` roots: `cargo update --workspace`
/// re-resolves the member versions and holds the registry pins. Every
/// invocation runs from `workspace_root` so a relative `--manifest-path` and
/// cargo's own environment resolve the same way for the root and the excluded
/// crates alike.
fn relock(workspace_root: &Path, manifest: &Path) -> Result<()> {
    let mut cmd: Command = command();
    cmd.current_dir(workspace_root).args(["update", "--workspace"]).arg("--manifest-path").arg(manifest);
    run_status(cmd, &format!("re-lock {}", lockfile_of(manifest).display()))
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::process;

    use cargo_metadata::semver::Version;

    use super::{excluded_lockfile_manifests, rewrite_workspace_version};

    #[test]
    fn only_the_workspace_package_version_moves() {
        // Tripwire: the root manifest carries several `version =` lines that
        // are not the workspace version — dependency requirements in
        // `[workspace.dependencies]`, and (in an excluded crate's manifest read
        // by the same code path) a deliberate `0.0.0`. A rewrite that matched
        // on `version =` alone would silently retarget `clap` at the workspace
        // version, which still parses and still builds until the next resolve.
        let manifest = "\
[workspace]
members = [\"xtask\"]
exclude = [\"fuzz\"]

[workspace.package]
version = \"0.3.0-alpha\"
edition = \"2024\"

[workspace.dependencies]
clap = { version = \"4\", features = [\"derive\"] }
version = \"not-a-table-key\"
";
        let (bumped, previous) =
            rewrite_workspace_version(manifest, &Version::parse("0.4.0-alpha").expect("parse")).expect("rewrite");

        assert_eq!(previous, "0.3.0-alpha");
        assert!(bumped.contains("version = \"0.4.0-alpha\""), "the workspace version moved: {bumped}");
        assert!(bumped.contains("clap = { version = \"4\""), "the clap requirement is untouched: {bumped}");
        assert!(bumped.contains("version = \"not-a-table-key\""), "a later table's version is untouched: {bumped}");

        let no_table = "[workspace]\nmembers = []\n";
        rewrite_workspace_version(no_table, &Version::parse("0.4.0").expect("parse"))
            .expect_err("a manifest with no workspace version is an error, not a silent no-op");
    }

    #[test]
    fn excluded_crates_are_selected_by_carrying_a_lockfile() {
        // Tripwire: this is the whole point of the command (finding F9). An
        // excluded crate with its own lockfile must be re-locked; one without
        // must not be handed to cargo, and a stale `exclude` entry naming a
        // directory that no longer exists must not abort the bump.
        let dir = env::temp_dir().join(format!("aether-xtask-bump-{}", process::id()));
        fs::create_dir_all(dir.join("fuzz")).expect("create fuzz");
        fs::create_dir_all(dir.join("bench")).expect("create bench");
        fs::write(dir.join("fuzz/Cargo.toml"), "").expect("write fuzz manifest");
        fs::write(dir.join("fuzz/Cargo.lock"), "").expect("write fuzz lock");
        fs::write(dir.join("bench/Cargo.toml"), "").expect("write bench manifest");

        let manifest = "[workspace]\nexclude = [\"fuzz\", \"bench\", \"removed\"]\n";
        let selected = excluded_lockfile_manifests(manifest, &dir).expect("scan excludes");

        assert_eq!(selected, vec![dir.join("fuzz/Cargo.toml")], "only the excluded crate with a lockfile is re-locked");

        fs::remove_dir_all(&dir).ok();
    }
}
