//! The kind-name grammar, enforced over the whole declared vocabulary.
//!
//! A kind name is a wire contract: `Kind::ID` hashes the name together with the
//! schema, so renaming a kind mints a different id and unroutes every peer that
//! already matched the old one. The grammar is written out in
//! `docs/guide/systems/mail-and-kinds.md`; this test is the half that holds.
//!
//! **What it catches.** A kind declared with a name that breaks the grammar —
//! a missing family segment (`aether.draw_triangle`), a second reply suffix
//! (`_ack`, `_report`, `_reply`), a `<x>_config` leaf where the grammar says
//! `.config`, or a segment that is not lowercase `snake_case`. That is a real
//! authoring mistake with a real cost, because the correction after the name
//! ships is a breaking id change rather than an edit.
//!
//! **Where the population comes from.** `aether_kinds::descriptors::all()`
//! reports only the kinds linked into the calling binary, and no single crate
//! in this workspace links the whole vocabulary — this crate's test binary sees
//! its own kinds and nothing from `aether-render`, `aether-store`, or any other
//! family. So the population is the *declaration sites* instead: every
//! `#[kind(name = "…")]` or `#[aether_data::kind(name = "…", …)]` attribute
//! under `crates/`, read straight off the source. That covers all 57 crates at
//! no build cost and is the same text an author types.
//!
//! Two kinds of name are outside the scan by construction: names minted inside
//! a macro from a non-literal (`#[kind(name = $name)]` in the perf probe
//! registry, `#[kind(name = #kind_name)]` in `aether-http-derive`'s route
//! kinds) are computed rather than authored, and names under a non-`aether`
//! root (`test.*`, `persist.*`) are test fixtures, not shipped contracts.
//!
//! **The allow-list.** `kind_name_allow_list.txt` lists the names that predate
//! the grammar. A violator missing from it fails; an entry that is no longer a
//! violator also fails, so the list only ever shrinks.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The root segment the grammar governs. Everything else declared in the
/// workspace is a test fixture rather than a shipped wire contract.
const GOVERNED_ROOT: &str = "aether";

const ALLOW_LIST: &str = include_str!("kind_name_allow_list.txt");

/// Reply-suffix spellings the grammar rejects — `_result` is the only one.
/// Stated as suffixes and as whole leaf segments so `…_ack` and a bare
/// `.response` are both caught.
const REJECTED_REPLY_SUFFIXES: &[&str] = &["_reply", "_ack", "_report", "_complete", "_response"];
const REJECTED_REPLY_LEAVES: &[&str] = &["reply", "ack", "report", "response"];

/// One rule of the grammar, as the failure message names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rule {
    /// Fewer than `<root>.<family>.<leaf>` segments.
    FamilySegment,
    /// A segment that is not lowercase `snake_case` (dashes allowed only in a
    /// non-leaf segment, where they name a dash-sibling namespace).
    SegmentCasing,
    /// A reply suffix other than `_result`.
    ReplySuffix,
    /// A `<x>_config` leaf where the grammar says `.config`.
    ConfigSuffix,
}

impl Rule {
    fn explain(self) -> &'static str {
        match self {
            Self::FamilySegment => "needs a family segment: aether.<family>[.<sub>].<leaf>",
            Self::SegmentCasing => "segments are lowercase snake_case (a dash only names a dash-sibling namespace)",
            Self::ReplySuffix => "_result is the only reply suffix",
            Self::ConfigSuffix => "a capability's boot config is aether.<family>.config, not a <x>_config leaf",
        }
    }
}

fn is_snake_segment(segment: &str, dashes_allowed: bool) -> bool {
    let mut chars = segment.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut previous_separator = false;
    for c in chars {
        let separator = c == '_' || (dashes_allowed && c == '-');
        if separator && previous_separator {
            return false;
        }
        if !separator && !c.is_ascii_lowercase() && !c.is_ascii_digit() {
            return false;
        }
        previous_separator = separator;
    }
    !previous_separator
}

/// Every rule `name` breaks, in rule order. Empty for a conforming name.
fn violations(name: &str) -> Vec<Rule> {
    let segments: Vec<&str> = name.split('.').collect();
    let leaf = *segments.last().unwrap_or(&"");
    let mut broken = Vec::new();

    if segments.len() < 3 {
        broken.push(Rule::FamilySegment);
    }
    let casing_holds =
        segments[..segments.len() - 1].iter().all(|s| is_snake_segment(s, true)) && is_snake_segment(leaf, false);
    if !casing_holds {
        broken.push(Rule::SegmentCasing);
    }
    if REJECTED_REPLY_SUFFIXES.iter().any(|s| leaf.ends_with(s)) || REJECTED_REPLY_LEAVES.contains(&leaf) {
        broken.push(Rule::ReplySuffix);
    }
    if leaf.ends_with("_config") {
        broken.push(Rule::ConfigSuffix);
    }

    broken
}

/// The two spellings a kind name is declared in: the derives' inert helper
/// attribute, and the `#[aether_data::kind]` attribute macro that emits that
/// helper along with the derive stack. Both carry the same authored literal,
/// so both are population.
const DECLARATION_OPENERS: [&str; 2] = ["#[kind(", "#[aether_data::kind("];

/// The declared kind-name literals on one source line, or nothing when the
/// line is a comment or the attribute's `name` is a macro binding rather than a
/// literal.
fn kind_names_in_line(line: &str) -> Vec<&str> {
    if line.trim_start().starts_with("//") {
        return Vec::new();
    }

    let mut found = Vec::new();
    let mut rest = line;
    while let Some((open, opener)) = DECLARATION_OPENERS
        .iter()
        .filter_map(|opener| rest.find(opener).map(|at| (at, *opener)))
        .min_by_key(|(at, _)| *at)
    {
        rest = &rest[open + opener.len()..];
        let after_name = rest.trim_start();
        let Some(after_name) = after_name.strip_prefix("name") else {
            continue;
        };
        let Some(after_eq) = after_name.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(value) = after_eq.trim_start().strip_prefix('"') else {
            continue;
        };
        let Some(close) = value.find('"') else {
            continue;
        };
        found.push(&value[..close]);
        rest = &value[close + 1..];
    }
    found
}

/// Every `.rs` file under `dir`, walked with an explicit stack (CLAUDE.md bans
/// recursion over data whose depth isn't structurally bounded).
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut pending = vec![dir.to_path_buf()];
    let mut files = Vec::new();
    while let Some(current) = pending.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if name != "target" && name != ".git" {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files
}

fn declared_kind_names() -> BTreeSet<String> {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join("crates");
    let mut names = BTreeSet::new();
    for file in rust_sources(&crates_dir) {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            names.extend(kind_names_in_line(line).into_iter().map(str::to_owned));
        }
    }

    assert!(
        names.len() > 400,
        "the declaration scan found only {} kind names — the walk is not reaching crates/",
        names.len()
    );
    names
}

fn allow_list() -> Vec<String> {
    ALLOW_LIST
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn allow_list_is_sorted_and_unique() {
    let entries = allow_list();
    let mut expected = entries.clone();
    expected.sort();
    expected.dedup();
    assert_eq!(entries, expected, "kind_name_allow_list.txt must be sorted and free of duplicates");
}

#[test]
fn every_declared_kind_name_follows_the_grammar() {
    let allowed: BTreeSet<String> = allow_list().into_iter().collect();
    let mut unlisted = Vec::new();
    let mut still_violating = BTreeSet::new();

    for name in declared_kind_names() {
        if name.split('.').next() != Some(GOVERNED_ROOT) {
            continue;
        }
        let broken = violations(&name);
        if broken.is_empty() {
            continue;
        }
        if allowed.contains(&name) {
            still_violating.insert(name);
            continue;
        }
        let reasons = broken.iter().map(|r| r.explain()).collect::<Vec<_>>().join("; ");
        unlisted.push(format!("  {name} — {reasons}"));
    }

    assert!(
        unlisted.is_empty(),
        "kind names break the grammar in docs/guide/systems/mail-and-kinds.md:\n{}\n\nRename the kind. \
         The allow-list carries names that predate the grammar and does not accept new entries.",
        unlisted.join("\n")
    );

    let stale: Vec<&String> = allowed.difference(&still_violating).collect();
    assert!(
        stale.is_empty(),
        "these kind_name_allow_list.txt entries are no longer violations (renamed, or the kind is gone) \
         — delete the lines so the list keeps shrinking:\n{stale:#?}"
    );
}

#[test]
fn grammar_rules_fire_on_the_shapes_they_name() {
    // Tripwire: each rule is checked against one name that breaks exactly it
    // and one from the two model families that must stay clean. A rule that
    // silently stops matching would otherwise let the allow-list drain itself.
    assert_eq!(violations("aether.fs.read_result"), []);
    assert_eq!(violations("aether.store.append_event_result"), []);
    assert_eq!(violations("aether.kit.camera-controller.config"), []);

    assert_eq!(violations("aether.draw_triangle"), [Rule::FamilySegment]);
    assert_eq!(violations("aether.render.DrawTriangle"), [Rule::SegmentCasing]);
    assert_eq!(violations("aether.render.draw-triangle"), [Rule::SegmentCasing]);
    assert_eq!(violations("aether.trace.dispatch_traced_ack"), [Rule::ReplySuffix]);
    assert_eq!(violations("aether.http.server.response"), [Rule::ReplySuffix]);
    assert_eq!(violations("aether.store.record_config"), [Rule::ConfigSuffix]);
}

#[test]
fn the_scan_reads_declarations_and_skips_prose() {
    // Tripwire: the extractor is the population. If it stops matching a real
    // attribute the grammar check passes over an empty set; if it starts
    // matching doc comments the allow-list fills with ellipses.
    assert_eq!(kind_names_in_line(r#"#[kind(name = "aether.fs.read")]"#), ["aether.fs.read"]);
    assert_eq!(kind_names_in_line(r#"#[kind(name="aether.fs.read")]"#), ["aether.fs.read"]);
    assert_eq!(kind_names_in_line(r#"#[aether_data::kind(name = "aether.fs.read")]"#), ["aether.fs.read"]);
    assert_eq!(kind_names_in_line(r#"#[aether_data::kind(name = "aether.fs.read", pod, eq)]"#), ["aether.fs.read"]);
    assert!(kind_names_in_line(r#"//! the `#[kind(name = "…")]` literal is the identity"#).is_empty());
    assert!(kind_names_in_line("#[kind(name = $name)]").is_empty());
}
