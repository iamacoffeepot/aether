//! Which resolved packages a `Cargo.lock` change moved (issue #5951).
//!
//! A lockfile change is a workspace-level input only for the crates whose
//! resolved dependency set actually changed. This module diffs the base and
//! candidate lockfiles package by package — version, source, checksum, and
//! dependency list — so the scope lane can verify the reverse-dependency
//! closure of the moved externals instead of the whole workspace.
//!
//! Workspace members compare by identity (version and source) alone. A
//! member's dependency list is derived text: adding a dependency rewrites it,
//! and that is exactly the case the narrowing exists for — the manifest move
//! names the member through the path-based closure and the new external names
//! its dependents through the attribution below. Only a member's own version
//! or source moving, or a member appearing or disappearing, fails open to the
//! whole workspace.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Deserialize;

/// How a `Cargo.lock` change attributes.
pub struct LockDiff {
    /// External packages whose resolved version, source, checksum, or
    /// dependency list differs between the base and candidate lockfiles.
    pub changed: BTreeSet<String>,
    /// Workspace members whose own version or source moved, or which one side
    /// does not name at all. Never a member whose dependency text alone moved.
    pub members_changed: BTreeSet<String>,
}

/// Diff the base and candidate lockfile contents, classifying member entries
/// through `members`.
///
/// # Errors
/// Either side is not a parseable lockfile.
pub fn diff(base_text: &str, candidate_text: &str, members: &BTreeSet<String>) -> Result<LockDiff> {
    let base = grouped(base_text, "base")?;
    let candidate = grouped(candidate_text, "candidate")?;

    let mut changed = BTreeSet::new();
    let mut members_changed = BTreeSet::new();
    let names: BTreeSet<&String> = base.keys().chain(candidate.keys()).collect();
    for name in names {
        if members.contains(name) {
            if base.get(name).map(|fingerprints| identity_of(fingerprints))
                != candidate.get(name).map(|fingerprints| identity_of(fingerprints))
            {
                members_changed.insert(name.clone());
            }
        } else if base.get(name) != candidate.get(name) {
            changed.insert(name.clone());
        }
    }

    Ok(LockDiff { changed, members_changed })
}

/// A lockfile entry's resolved content: everything about the package the
/// workspace builds against except its name.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Fingerprint {
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    dependencies: Vec<String>,
}

/// A member entry's own identity: the version and source the member ships as,
/// without the dependency text its manifest derives.
fn identity_of(fingerprints: &[Fingerprint]) -> Vec<(&str, Option<&str>)> {
    fingerprints.iter().map(|fingerprint| (fingerprint.version.as_str(), fingerprint.source.as_deref())).collect()
}

/// The lockfile's entries grouped by package name, each group sorted so the
/// comparison is order-insensitive.
fn grouped(text: &str, side: &str) -> Result<BTreeMap<String, Vec<Fingerprint>>> {
    let lockfile: Lockfile = toml::from_str(text).with_context(|| format!("parse the {side} Cargo.lock"))?;

    let mut grouped: BTreeMap<String, Vec<Fingerprint>> = BTreeMap::new();
    for entry in lockfile.package {
        let mut dependencies = entry.dependencies;
        dependencies.sort();
        grouped.entry(entry.name).or_default().push(Fingerprint {
            version: entry.version,
            source: entry.source,
            checksum: entry.checksum,
            dependencies,
        });
    }
    for fingerprints in grouped.values_mut() {
        fingerprints.sort();
    }

    Ok(grouped)
}

/// The lockfile as TOML: a header plus one `[[package]]` table per resolved
/// package. Unknown sections (a newer `version` line, a `[metadata]` table)
/// are ignored — only the package entries shape a build.
#[derive(Debug, Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockEntry>,
}

/// One `[[package]]` entry. `dependencies` is absent for a leaf.
#[derive(Debug, Deserialize)]
struct LockEntry {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::diff;

    const EXTERNAL: &str = "notify";

    fn lockfile(packages: &str) -> String {
        format!("version = 4\n\n{packages}")
    }

    fn external(version: &str) -> String {
        format!(
            "[[package]]\nname = \"{EXTERNAL}\"\nversion = \"{version}\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{version}\"\n"
        )
    }

    fn members(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn an_identical_pair_moves_nothing_and_a_bump_marks_its_package() {
        // Tripwire for the false-green direction: a bump the diff misses reads
        // as "no resolved package moved", and a lockfile-only diff then
        // resolves an empty closure — a pass over a changed build.
        let base = lockfile(&external("8.2.0"));

        let same = diff(&base, &base, &BTreeSet::new()).expect("diff identical lockfiles");
        assert!(same.changed.is_empty(), "identical lockfiles move nothing: {:?}", same.changed);

        let bumped = diff(&base, &lockfile(&external("8.2.1")), &BTreeSet::new()).expect("diff a bumped lockfile");
        assert_eq!(bumped.changed, members(&[EXTERNAL]), "a version bump marks its package");
    }

    #[test]
    fn a_source_or_dependency_move_marks_the_package_changed() {
        // Tripwire for a version-only comparison: a source swap or a reshaped
        // dependency list rebuilds dependents exactly as a bump does, so it
        // must attribute the same way.
        let base = lockfile(&external("8.2.0"));

        let swapped = base.replace("registry+https://github.com/rust-lang/crates.io-index", "sparse+candidate");
        let moved = diff(&base, &swapped, &BTreeSet::new()).expect("diff a source swap");
        assert_eq!(moved.changed, members(&[EXTERNAL]), "a source move marks its package");

        let reshaped = lockfile(&(external("8.2.0") + "dependencies = [\n \"serde\",\n]\n"));
        let moved = diff(&base, &reshaped, &BTreeSet::new()).expect("diff a dependency move");
        assert_eq!(moved.changed, members(&[EXTERNAL]), "a dependency move marks its package");
    }

    #[test]
    fn a_members_dependency_text_is_not_a_member_move() {
        // Tripwire for the case the narrowing exists for: adding a dependency
        // rewrites the member's lock entry, and treating that text as a member
        // move widens the single-dep-add candidate back to the whole workspace.
        let member = "aether-mcp";
        let base = lockfile(&format!(
            "[[package]]\nname = \"{member}\"\nversion = \"0.3.0-alpha\"\ndependencies = [\n \"notify\",\n]\n"
        ));
        let added = lockfile(&format!(
            "[[package]]\nname = \"{member}\"\nversion = \"0.3.0-alpha\"\ndependencies = [\n \"notify\",\n \"serde\",\n]\n"
        ));

        let moved = diff(&base, &added, &members(&[member])).expect("diff a member dependency add");
        assert!(moved.members_changed.is_empty(), "dependency text alone moves no member: {:?}", moved.members_changed);
    }

    #[test]
    fn a_members_own_version_move_is_a_member_move() {
        // Tripwire for the other fail-open: a member shipping as a new version
        // is not attributable through any external, so it must widen.
        let member = "aether-math";
        let base = lockfile(&format!("[[package]]\nname = \"{member}\"\nversion = \"0.3.0-alpha\"\n"));
        let bumped = lockfile(&format!("[[package]]\nname = \"{member}\"\nversion = \"0.4.0-alpha\"\n"));

        let moved = diff(&base, &bumped, &members(&[member])).expect("diff a member version bump");
        assert_eq!(moved.members_changed, members(&[member]), "a member version move widens");
    }
}
