//! The dist-consumer invariant: code that reaches a `cargo xtask dist`
//! artifact through the filesystem instead of through a cargo dependency
//! edge must still be visible to `wasm_needed`.
//!
//! Two checks sit on either side of the dependency edge
//! [`is_dist_consumer`](crate::affected::select::is_dist_consumer) keys
//! on, and together they cover every `.rs` byte in a package:
//!
//! - [`test_code_resolving_dist_artifacts_is_classified`] over test code.
//!   A package whose own tests reach an artifact by path must be
//!   classified, or the pre-build is skipped and the test finds nothing.
//!   This is the `aether-chassis-headless` bug (#4197) stated as a rule.
//! - [`dist_resolving_harness_list_is_exactly_the_crates_that_resolve`]
//!   over library code. A crate that offers such resolution to its
//!   dependents must be in [`DIST_RESOLVING_HARNESSES`], because that
//!   membership is the only thing that makes those dependents classify.
//!   The check derives the set and compares, so the constant is
//!   recomputed rather than trusted.
//!
//! Neither check depends on [`RESOLVER_HELPERS`] being complete. A new
//! locator under a new name still lives in a library, where the second
//! check catches it on its path-construction signature and forces it into
//! the harness list; every dependent then classifies through the
//! dependency edge without this module having to learn the name.

use std::collections::{BTreeMap, BTreeSet};

use cargo_metadata::Package;

use crate::affected::invariants::source::{self, RustSource, string_literals};
use crate::affected::invariants::workspace::{TEST_DIR, Workspace, has_library, package_root};
use crate::affected::select::DIST_RESOLVING_HARNESSES;

/// Helpers that hand back a path into the `cargo xtask dist` output. A
/// call is unambiguous evidence in a way a path literal is not — test
/// data and diagnostics quote paths, they do not call locators.
const RESOLVER_HELPERS: &[&str] = &[
    "locate_component_wasm",
    "require_wasm",
    "require_runtime",
    "headless_bin_path",
    "chassis_bin_path",
    "component_wasm_path",
    "read_component_wasm",
    "dist_component_available",
];

/// Filesystem reads, paired with a path literal below. A `dist`-shaped
/// string on its own is as likely to be a fixture or a diagnostic as a
/// real resolution, and a check that cannot tell those apart is a check
/// somebody eventually silences.
const FILESYSTEM_READS: &[&str] =
    &["fs::read", "fs::read_to_string", "fs::metadata", "File::open", "read_dir", ".exists()", ".is_file()"];

/// How a region betrays that it resolves a dist artifact by path.
struct Resolution {
    /// Human-readable evidence, quoted into the failure message.
    evidence: String,
    /// Byte offset of the evidence, for the reported line.
    offset: usize,
}

/// Scan one region for the two signatures, returning the first hit —
/// which is all a failure message needs, since the fix is per-package
/// rather than per-line.
fn dist_resolution(region: &str) -> Option<Resolution> {
    if let Some((offset, helper)) = find_identifier(region, RESOLVER_HELPERS) {
        return Some(Resolution { evidence: format!("calls the dist-artifact locator `{helper}`"), offset });
    }
    let read = FILESYSTEM_READS.iter().find(|read| region.contains(**read))?;
    let (offset, literal) = dist_path_literals(region).next()?;
    Some(Resolution { evidence: format!("builds the path {literal:?} and reads it with `{read}`"), offset })
}

/// String literals in `region` that name a `cargo xtask dist` output.
///
/// A path literal holds no whitespace, and that alone separates
/// `"wasm32-unknown-unknown"` and `"../../dist"` from the prose that
/// mentions them — which is most of what a plain substring search finds:
/// build hints, skip diagnostics, and inline JSON manifest fixtures.
fn dist_path_literals(region: &str) -> impl Iterator<Item = (usize, &str)> {
    string_literals(region).into_iter().filter(|(_, literal)| {
        !literal.chars().any(char::is_whitespace)
            && (literal.contains("wasm32-unknown-unknown") || literal.split('/').any(|segment| segment == "dist"))
    })
}

/// The earliest whole-identifier occurrence of any `names` entry.
fn find_identifier<'a>(region: &str, names: &[&'a str]) -> Option<(usize, &'a str)> {
    let bytes = region.as_bytes();
    let boundary = |index: usize| !bytes.get(index).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
    names
        .iter()
        .filter_map(|name| {
            let offset = region
                .match_indices(name)
                .map(|(offset, _)| offset)
                .find(|offset| offset.checked_sub(1).is_none_or(boundary) && boundary(offset + name.len()))?;
            Some((offset, *name))
        })
        .min_by_key(|(offset, _)| *offset)
}

/// Every `.rs` file in the package, pre-processed and split into the two
/// regions the checks scan.
struct PackageSource {
    file: RustSource,
    /// Code that exists only in a test build. A file under the package's
    /// own `tests/` directory is that in its entirety — it carries no
    /// `#[cfg(test)]` because the whole target is one.
    test_region: String,
    /// Code a dependent compiles against. Empty for a `tests/` file.
    library_region: String,
}

fn package_sources(package: &Package) -> Vec<PackageSource> {
    let tests_dir = package_root(package).join(TEST_DIR);
    source::read_all(&source::walk(package_root(package)).rust_files)
        .into_iter()
        .map(|file| {
            if file.path.starts_with(&tests_dir) {
                let test_region = file.code.clone();
                PackageSource { file, test_region, library_region: String::new() }
            } else {
                let (test_region, library_region) = (file.test_code.clone(), file.non_test_code.clone());
                PackageSource { file, test_region, library_region }
            }
        })
        .collect()
}

#[test]
fn test_code_resolving_dist_artifacts_is_classified() {
    // Tripwire: `cargo xtask affected` derives `wasm_needed` from the
    // dist-consumer set, so a package outside that set runs its tests
    // with no `cargo xtask dist` pre-build and any artifact those tests
    // resolve by path is simply absent — a hard failure under
    // AETHER_REQUIRE_RUNTIME that only surfaces after the PR lands
    // (issues #3766 / #4197). Before #4212's narrowing, an unclassified
    // package like `aether-chassis-headless` got its pre-build by
    // accident, from an unrelated dependent selected alongside it; the
    // narrowing removed the accident and left the property unguarded.
    //
    // A package whose every change already forces `run_all` is skipped —
    // `xtask` itself, whose sources hold the marker table this scan reads.
    // That exemption is derived from `global_screen`, not named, so it
    // lapses if the screen ever stops covering the package.
    let workspace = Workspace::load();
    let mut violations = Vec::new();
    for package in workspace.packages() {
        if workspace.is_classified_dist_consumer(package) || workspace.changes_force_run_all(package) {
            continue;
        }
        for source in package_sources(package) {
            let file = &source.file;
            let Some(hit) = dist_resolution(&source.test_region) else {
                continue;
            };
            violations.push(format!(
                "  {name}: test code resolves a `cargo xtask dist` artifact by filesystem path, but the \
                 package is not classified as a dist consumer.\n    {path}:{line}\n      {evidence}\n    \
                 Fix: give {name} a dependency on one of the dist-resolving harnesses ({harnesses}) or on a \
                 wasm-source crate, or stop resolving the artifact by path.",
                name = package.name,
                path = workspace.relative(&file.path),
                line = file.line_of(hit.offset),
                evidence = hit.evidence,
                harnesses = DIST_RESOLVING_HARNESSES.join(", "),
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "test code reaches a dist artifact from a package `cargo xtask affected` will not pre-build for:\n\n{}\n",
        violations.join("\n\n")
    );
}

#[test]
fn dist_resolving_harness_list_is_exactly_the_crates_that_resolve() {
    // Tripwire: DIST_RESOLVING_HARNESSES is what makes a *dependent's*
    // tests classify — `aether-chassis-headless` needs the pre-build only
    // because it deps `aether-harness-substrate-capture`, which is on the
    // list. A library that resolves dist artifacts without being listed
    // silently declassifies every dependent reaching an artifact through
    // it, which is the #4197 bug one hop removed. Deriving the set here
    // and comparing turns the constant from a 2026-07-31 snapshot into
    // something CI recomputes on every push.
    //
    // Only library packages are scanned. A bin-only package is nobody's
    // dependency, so nothing outside its own test code (covered by the
    // check above) can propagate to another package's test run — which is
    // why `xtask` itself, the dist *producer*, is not a finding here.
    let workspace = Workspace::load();
    let mut derived = BTreeMap::new();
    let scanned =
        workspace.packages().filter(|package| has_library(package) && !workspace.changes_force_run_all(package));
    for package in scanned {
        for source in package_sources(package) {
            let file = &source.file;
            let Some(hit) = dist_resolution(&source.library_region) else {
                continue;
            };
            derived.entry(package.name.to_string()).or_insert_with(|| {
                format!(
                    "    {name}: {path}:{line}\n      {evidence}",
                    name = package.name,
                    path = workspace.relative(&file.path),
                    line = file.line_of(hit.offset),
                    evidence = hit.evidence,
                )
            });
        }
    }

    let resolvers: BTreeSet<&String> = derived.keys().collect();
    let listed: BTreeSet<String> = DIST_RESOLVING_HARNESSES.iter().map(|name| (*name).to_string()).collect();
    let unlisted: Vec<&String> = resolvers.iter().copied().filter(|name| !listed.contains(*name)).collect();
    assert!(
        unlisted.is_empty(),
        "library code resolves a `cargo xtask dist` artifact by filesystem path in crates missing from \
         DIST_RESOLVING_HARNESSES (xtask/src/affected/select.rs):\n\n{evidence}\n\n    \
         A package whose tests reach the artifact through one of these classifies as a non-consumer, so \
         `cargo xtask affected` skips the `cargo xtask dist` pre-build and those tests hard-fail with \
         nothing to load.\n    Fix: add {unlisted:?} to DIST_RESOLVING_HARNESSES.\n",
        evidence = unlisted.iter().map(|name| derived[*name].as_str()).collect::<Vec<_>>().join("\n"),
    );

    let undetected: Vec<&String> = listed.iter().filter(|name| !resolvers.contains(name)).collect();
    assert!(
        undetected.is_empty(),
        "DIST_RESOLVING_HARNESSES lists {undetected:?}, but no dist-artifact path resolution was found in \
         their library sources.\n\n    \
         Either the crate stopped resolving dist artifacts by path, or this scan's signatures no longer \
         match how it does — in which case the scan is stale and the completeness check above is \
         vacuous.\n    Read the crate before touching the list: deleting the entry to make this pass \
         disarms the guard for every dependent that reaches an artifact through it.\n"
    );
}

#[test]
fn dist_resolution_reads_calls_and_paths_but_not_prose() {
    // Tripwire: the whole guard rests on this discrimination. Widen it and
    // the check fires on every diagnostic string naming a build command —
    // a red nobody trusts is a red somebody deletes. Narrow it and the
    // hand-rolled locator, the shape that caused #4197, walks through.
    assert!(dist_resolution(r#"let wasm = require_wasm("aether_kit_commons");"#).is_some(), "a locator call");
    assert!(
        dist_resolution(r#"let base = root.join("wasm32-unknown-unknown").join(profile); base.exists()"#).is_some(),
        "a hand-rolled path build followed by a filesystem read"
    );
    assert!(
        dist_resolution(r#"fs::read("../../target/wasm32-unknown-unknown/release/probe.wasm")"#).is_some(),
        "a literal artifact path handed to a read"
    );
    assert!(
        dist_resolution(r#"let path = dist_root.join("dist/bin/aether-headless"); path.is_file()"#).is_some(),
        "a dist-tree path built and probed"
    );

    assert!(
        dist_resolution(r#"panic!("run `cargo build --target wasm32-unknown-unknown -p foo`"); fs::read(other)"#)
            .is_none(),
        "a build hint quoting the target triple is prose, not a resolution"
    );
    assert!(
        dist_resolution(r##"let manifest = r#"{ "chassis": { "x": "dist/bin/x" } }"#; fs::read(other)"##).is_none(),
        "an inline manifest fixture is test data, not a resolution"
    );
    assert!(
        dist_resolution(r#"let hint = "target/wasm32-unknown-unknown/release/x.wasm";"#).is_none(),
        "a path with no filesystem read behind it resolves nothing"
    );
    assert!(dist_resolution("require_runtime_enabled()").is_none(), "a longer identifier is not a locator call");
}
