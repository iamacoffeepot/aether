//! `cargo xtask namespaces` — the hand-*naming* check.
//!
//! `clippy.toml` already disallows hand-*hashing* an address
//! (`mailbox_id_from_name` / `_pair`), but the step before it is unguarded: an
//! actor's `NAMESPACE` written out again as a `&str` somewhere else. The
//! duplicate compiles, resolves, and warn-drops if the two ever diverge —
//! there is no compile error to catch it, because the parameter that receives
//! it is a `&str` (iamacoffeepot/aether#5720).
//!
//! So this check reads the declarations out of the tree and looks for the
//! literals that repeat one. A crate writing its own namespace is out of
//! scope; a crate writing *another* crate's is the finding, unless [`allow`]
//! records why the dependency graph leaves it no const to read.
//!
//! Local-only. The `verify.*` members each shell out to one external program
//! and are mirrored by a CI job of their own; adding a tenth required job for
//! a check whose baseline is still being paid down is a wider change than the
//! finding asks for, so this runs from a laptop and from a lane by name.

mod allow;
mod scan;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::{fs, io, process};

use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use clap::Args;

use crate::namespaces::allow::AllowList;
use crate::namespaces::scan::{Declaration, FileScan, Literal};
use crate::symbols::walk::rust_files;

/// The allow file, relative to the workspace root.
const ALLOW_FILE: &str = "xtask/namespace-literals-allow.toml";

#[derive(Args, Debug)]
pub struct NamespacesArgs {
    /// Report every finding without failing. The check still prints the same
    /// table; only the exit code changes.
    #[arg(long)]
    allow_failure: bool,
}

pub fn run(args: &NamespacesArgs) -> Result<()> {
    let metadata = MetadataCommand::new().no_deps().exec().context("run cargo metadata")?;
    let workspace_root = metadata.workspace_root.as_std_path().to_path_buf();
    let (declarations, literals) = read_tree(&metadata, &workspace_root)?;

    let owners = owners_by_namespace(&declarations);
    let mut allowed = AllowList::load(&workspace_root.join(ALLOW_FILE))?;
    let mut findings = Vec::new();
    for literal in &literals {
        let Some(declaring) = owners.get(literal.value.as_str()) else {
            continue;
        };
        let owning_crates = crate_names(declaring);
        if owning_crates.contains(&literal.crate_name) || allowed.allows(&literal.crate_name, &literal.value) {
            continue;
        }
        findings.push(Finding { literal, owning_crates, declared_at: declaring[0] });
    }

    let stale = allowed.stale();
    report(&declarations, &literals, &findings, &stale, &workspace_root);
    if args.allow_failure || (findings.is_empty() && stale.is_empty()) {
        return Ok(());
    }
    process::exit(1);
}

/// Every `crates/*/src/**.rs` file's production declarations and literals.
///
/// `src/` only: an integration test under `tests/` addresses its recipients by
/// string because the harness op takes one, which is a separate finding with a
/// separate fix, and folding it in here would bury this one.
fn read_tree(metadata: &cargo_metadata::Metadata, workspace_root: &Path) -> Result<(Vec<Declaration>, Vec<Literal>)> {
    let mut scanned: Vec<(PathBuf, FileScan)> = Vec::new();
    for package in metadata.workspace_packages() {
        let Some(crate_root) = package.manifest_path.parent() else {
            continue;
        };
        let source_root = crate_root.as_std_path().join("src");
        if !source_root.is_dir() {
            continue;
        }
        for path in rust_files(&source_root) {
            let source = match fs::read_to_string(&path) {
                Ok(source) => source,
                Err(error) if error.kind() == io::ErrorKind::InvalidData => continue,
                Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
            };
            let relative = path.strip_prefix(workspace_root).unwrap_or(&path).to_path_buf();
            scanned.push((path, scan::scan(&source, package.name.as_str(), &relative)));
        }
    }

    let test_files = test_file_set(&scanned);
    let mut declarations = Vec::new();
    let mut literals = Vec::new();
    for (path, file) in scanned {
        if test_files.contains(&path) {
            continue;
        }
        declarations.extend(file.declarations);
        literals.extend(file.literals);
    }
    Ok((declarations, literals))
}

/// The files reachable only through a `#[cfg(test)]` module edge.
///
/// Nothing in such a file is production code, however ordinary it reads on its
/// own — `tools/tests/mail.rs` is only test source because `tools/mod.rs` says
/// `#[cfg(test)] mod tests;`. Test-ness is transitive, so the closure runs to a
/// fixed point rather than one level down.
fn test_file_set(scanned: &[(PathBuf, FileScan)]) -> BTreeSet<PathBuf> {
    let known: BTreeSet<&PathBuf> = scanned.iter().map(|(path, _)| path).collect();
    let mut test_files = BTreeSet::new();
    let mut settled = false;
    while !settled {
        settled = true;
        for (path, file) in scanned {
            let declarer_is_test = test_files.contains(path);
            for module in &file.modules {
                if !module.test && !declarer_is_test {
                    continue;
                }
                for candidate in module_files(path, &module.name) {
                    if known.contains(&candidate) && test_files.insert(candidate) {
                        settled = false;
                    }
                }
            }
        }
    }
    test_files
}

/// The two files a `mod name;` in `declarer` can resolve to. A `mod.rs` /
/// `lib.rs` / `main.rs` declares siblings; any other file declares children of
/// a directory named after itself.
fn module_files(declarer: &Path, name: &str) -> Vec<PathBuf> {
    let Some(parent) = declarer.parent() else {
        return Vec::new();
    };
    let stem = declarer.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
    let directory = if matches!(stem, "mod" | "lib" | "main") {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };
    vec![directory.join(format!("{name}.rs")), directory.join(name).join("mod.rs")]
}

/// One literal that repeats a namespace its crate does not declare.
struct Finding<'a> {
    literal: &'a Literal,
    owning_crates: Vec<String>,
    declared_at: &'a Declaration,
}

/// Namespace to the declarations of it, in tree order. A namespace can have
/// more than one declaring crate — a guest-face type and its runtime, or a
/// fixture beside the real actor — and any of them writing the literal is
/// writing its own.
fn owners_by_namespace(declarations: &[Declaration]) -> BTreeMap<&str, Vec<&Declaration>> {
    let mut owners: BTreeMap<&str, Vec<&Declaration>> = BTreeMap::new();
    for declaration in declarations {
        owners.entry(declaration.namespace.as_str()).or_default().push(declaration);
    }
    owners
}

fn crate_names(declarations: &[&Declaration]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for declaration in declarations {
        if !names.contains(&declaration.crate_name) {
            names.push(declaration.crate_name.clone());
        }
    }
    names
}

fn report(
    declarations: &[Declaration],
    literals: &[Literal],
    findings: &[Finding<'_>],
    stale: &[&allow::Entry],
    workspace_root: &Path,
) {
    println!("namespaces: {} declared across {} string literals in crates/*/src", declarations.len(), literals.len());

    for finding in findings {
        println!(
            "{}:{}: `{}` is {}'s declared NAMESPACE ({}:{}) — address the type, or record the dependency-direction reason in {}",
            finding.literal.path.display(),
            finding.literal.line,
            finding.literal.value,
            finding.owning_crates.join(" / "),
            finding.declared_at.path.display(),
            finding.declared_at.line,
            ALLOW_FILE,
        );
    }

    for entry in stale {
        let claim = entry.reason.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
        println!(
            "{}: `{}` in {} is allowed but no longer written — drop the entry ({claim})",
            ALLOW_FILE, entry.namespace, entry.crate_name,
        );
    }

    if findings.is_empty() && stale.is_empty() {
        println!(
            "namespaces: no hand-written peer namespaces (allow file: {})",
            workspace_root.join(ALLOW_FILE).display()
        );
    } else {
        println!("namespaces: {} finding(s), {} stale allowance(s)", findings.len(), stale.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: the ownership rule. A crate writing a namespace it declares
    /// itself is not a finding, and a namespace with two declaring crates is
    /// owned by both — the guest-face / runtime split makes that the normal
    /// case, so getting it wrong would report the whole workspace.
    #[test]
    fn a_namespace_is_owned_by_every_crate_that_declares_it() {
        let declaration = |crate_name: &str, namespace: &str| Declaration {
            namespace: namespace.to_owned(),
            crate_name: crate_name.to_owned(),
            path: PathBuf::from("src/lib.rs"),
            line: 1,
        };
        let declared = [
            declaration("aether-fleet", "aether.fleet"),
            declaration("aether-mcp", "aether.fleet"),
            declaration("aether-fleet", "aether.fleet"),
        ];
        let owners = owners_by_namespace(&declared);

        assert_eq!(crate_names(&owners["aether.fleet"]), ["aether-fleet", "aether-mcp"]);
    }
}
