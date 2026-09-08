//! Conflict set between two candidate diffs.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use crate::diff::{Change, ChangeKind, diff_revs};
use crate::extract::extract_source;
use crate::git;
use crate::refs::{self, RefHit, Role};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Independent,
    SuspectTestPin,
    ConflictBreaks,
    ConflictDuplicate,
    ConflictBoth,
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Independent => "Independent",
            Verdict::SuspectTestPin => "Suspect(TestPin)",
            Verdict::ConflictBreaks => "Conflict(Breaks)",
            Verdict::ConflictDuplicate => "Conflict(Duplicate)",
            Verdict::ConflictBoth => "Conflict(Breaks, Duplicate)",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Sentence {
    #[allow(dead_code)]
    pub kind: &'static str,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub verdict: Verdict,
    pub sentences: Vec<Sentence>,
    pub elapsed_millis: u128,
    pub changes_a: usize,
    pub changes_b: usize,
}

pub fn independent(repo: &Path, base: &str, head_a: &str, head_b: &str) -> Result<Report> {
    let start = Instant::now();
    let a = diff_revs(repo, base, head_a, &[])?;
    let b = diff_revs(repo, base, head_b, &[])?;
    let mut report = conflict_set(repo, head_a, head_b, &a, &b)?;
    report.elapsed_millis = start.elapsed().as_millis();
    Ok(report)
}

/// Each candidate against its own parent, then intersect the two diffs.
pub fn independent_parents(repo: &Path, head_a: &str, head_b: &str) -> Result<Report> {
    let start = Instant::now();
    let base_a = crate::git::parent_of(repo, head_a)?;
    let base_b = crate::git::parent_of(repo, head_b)?;
    let a = diff_revs(repo, &base_a, head_a, &[])?;
    let b = diff_revs(repo, &base_b, head_b, &[])?;
    let mut report = conflict_set(repo, head_a, head_b, &a, &b)?;
    report.elapsed_millis = start.elapsed().as_millis();
    Ok(report)
}

pub fn conflict_set(repo: &Path, head_a: &str, head_b: &str, a: &[Change], b: &[Change]) -> Result<Report> {
    let mut sentences = Vec::new();
    let mut breaks = false;
    let mut duplicate = false;
    let mut test_pin = false;

    let added_a: HashSet<&str> = a
        .iter()
        .filter(|c| matches!(c.kind, ChangeKind::Added))
        .map(|c| c.path.as_str())
        .collect();
    let added_b: HashSet<&str> = b
        .iter()
        .filter(|c| matches!(c.kind, ChangeKind::Added))
        .map(|c| c.path.as_str())
        .collect();
    let mut dups: Vec<_> = added_a.intersection(&added_b).copied().collect();
    dups.sort();
    for path in dups {
        duplicate = true;
        sentences.push(Sentence {
            kind: "Duplicate",
            text: format!("A and B both added {path}"),
        });
    }

    note_reexports("A", a, &mut sentences);
    note_reexports("B", b, &mut sentences);

    breaks |= scan_breaks(repo, "A", "B", head_b, a, &mut sentences)?;
    breaks |= scan_breaks(repo, "B", "A", head_a, b, &mut sentences)?;
    test_pin |= scan_test_pin(repo, "A", "B", head_b, a, &mut sentences)?;
    test_pin |= scan_test_pin(repo, "B", "A", head_a, b, &mut sentences)?;

    let verdict = if breaks && duplicate {
        Verdict::ConflictBoth
    } else if breaks {
        Verdict::ConflictBreaks
    } else if duplicate {
        Verdict::ConflictDuplicate
    } else if test_pin {
        Verdict::SuspectTestPin
    } else {
        Verdict::Independent
    };

    Ok(Report {
        verdict,
        sentences,
        elapsed_millis: 0,
        changes_a: a.len(),
        changes_b: b.len(),
    })
}

/// Record every re-export edge without letting it drive the verdict.
///
/// A `Reexport` moves a definition and leaves the name where readers write it,
/// so it is not a break. It is still worth a line in the report: the residual
/// risk is a re-export that points at an item whose signature differs, which
/// this spike does not follow across crates, and a silent exclusion would hide
/// that the exclusion happened.
fn note_reexports(changer: &str, changes: &[Change], sentences: &mut Vec<Sentence>) {
    for change in changes.iter().filter(|c| c.kind == ChangeKind::Reexport) {
        sentences.push(Sentence {
            kind: "Reexport",
            text: format!(
                "{changer} re-exported {} at {}:{}; the name still resolves, so this is not a break",
                change.path, change.file, change.line
            ),
        });
    }
}

fn scan_breaks(
    repo: &Path,
    changer: &str,
    reader: &str,
    reader_rev: &str,
    changes: &[Change],
    sentences: &mut Vec<Sentence>,
) -> Result<bool> {
    let mut found = false;
    let mut cache: HashMap<String, Vec<RefHit>> = HashMap::new();
    for change in changes {
        let (old_path, verb) = match &change.kind {
            ChangeKind::Removed => (change.path.as_str(), "removed"),
            ChangeKind::SignatureChanged => (change.path.as_str(), "changed signature of"),
            ChangeKind::Renamed { from, .. } => (from.as_str(), "renamed"),
            _ => continue,
        };
        let ident = ident_of(old_path);
        if ident.is_empty() || !is_searchable_ident(&ident) {
            continue;
        }
        if reader_already_has(repo, reader_rev, change) {
            continue;
        }
        let hits = cache.entry(ident.clone()).or_insert_with(|| {
            refs::find_refs_filtered(repo, reader_rev, &ident, Some(old_path)).unwrap_or_else(|err| {
                eprintln!("refs {reader_rev} {ident}: {err}");
                Vec::new()
            })
        });
        let reads: Vec<&RefHit> = hits
            .iter()
            .filter(|h| h.role == Role::Referencing && h.plausible)
            .filter(|h| !is_own_definition(h, old_path))
            .collect();
        if reads.is_empty() {
            continue;
        }
        found = true;
        for hit in reads {
            let text = match &change.kind {
                ChangeKind::Renamed { from, to } => format!(
                    "{changer} renamed {from} to {to}; {reader} reads it at {}:{}",
                    hit.file, hit.line
                ),
                _ => format!(
                    "{changer} {verb} {old_path}; {reader} reads it at {}:{}",
                    hit.file, hit.line
                ),
            };
            sentences.push(Sentence { kind: "Breaks", text });
        }
    }
    Ok(found)
}

fn scan_test_pin(
    repo: &Path,
    changer: &str,
    reader: &str,
    reader_rev: &str,
    changes: &[Change],
    sentences: &mut Vec<Sentence>,
) -> Result<bool> {
    let mut found = false;
    let mut cache: HashMap<String, Vec<RefHit>> = HashMap::new();
    for change in changes {
        if !matches!(change.kind, ChangeKind::BodyChanged) {
            continue;
        }
        let ident = ident_of(&change.path);
        if ident.is_empty() || !is_searchable_ident(&ident) {
            continue;
        }
        if reader_already_has(repo, reader_rev, change) {
            continue;
        }
        let hits = cache.entry(ident.clone()).or_insert_with(|| {
            refs::find_refs_filtered(repo, reader_rev, &ident, Some(&change.path)).unwrap_or_else(|err| {
                eprintln!("refs {reader_rev} {ident}: {err}");
                Vec::new()
            })
        });
        let pins: Vec<&RefHit> = hits
            .iter()
            .filter(|h| h.role == Role::Referencing && h.is_test && h.plausible)
            .collect();
        if pins.is_empty() {
            continue;
        }
        found = true;
        for hit in pins {
            let test = hit.test_fn.as_deref().unwrap_or("<test>");
            sentences.push(Sentence {
                kind: "TestPin",
                text: format!(
                    "{changer} body-changed {}; {reader} test `{test}` reads it at {}:{}",
                    change.path, hit.file, hit.line
                ),
            });
        }
    }
    Ok(found)
}

fn reader_already_has(repo: &Path, reader_rev: &str, change: &Change) -> bool {
    match item_at(repo, reader_rev, &change.file, &change.path) {
        Some((sig, body)) => match change.kind {
            ChangeKind::SignatureChanged => sig == change.signature_hash,
            ChangeKind::BodyChanged => body == change.body_hash,
            ChangeKind::Renamed { .. } => true,
            ChangeKind::Removed => false,
            ChangeKind::Added => true,
            ChangeKind::Reexport => sig == change.signature_hash,
        },
        None => matches!(change.kind, ChangeKind::Removed),
    }
}

fn item_at(repo: &Path, rev: &str, file: &str, path: &str) -> Option<(String, String)> {
    let src = git::show_file(repo, rev, file).ok()??;
    let items = extract_source(file, &src).ok()?;
    let item = items.into_iter().find(|i| i.path == path)?;
    Some((item.signature_hash, item.body_hash))
}

fn ident_of(path: &str) -> String {
    let tail = path.rsplit("::").next().unwrap_or(path);
    let tail = tail.split('#').next().unwrap_or(tail);
    if tail.starts_with("impl ") {
        return tail
            .split_whitespace()
            .last()
            .unwrap_or("")
            .replace(['<', '>', ',', '(', ')'], "");
    }
    tail.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

const SKIP_IDENTS: &[&str] = &[
    "self",
    "Self",
    "super",
    "crate",
    "from",
    "into",
    "new",
    "ok",
    "Ok",
    "err",
    "Err",
    "fmt",
    "clone",
    "default",
    "eq",
    "ne",
    "hash",
    "drop",
    "as_str",
    "to_string",
    "len",
    "is_empty",
    "iter",
    "next",
    "item",
    "get",
    "set",
    "push",
    "pop",
    "id",
    "ty",
    "run",
    "mod",
    "use",
    "name",
    "path",
    "file",
    "args",
    "n",
    "i",
    "x",
    "y",
    "s",
    "v",
    "t",
];

fn is_searchable_ident(ident: &str) -> bool {
    ident.len() >= 4
        && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && ident
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && !SKIP_IDENTS.contains(&ident)
}

fn is_own_definition(hit: &RefHit, path: &str) -> bool {
    hit.role == Role::Defining && hit.defining_path.as_deref() == Some(path)
}

pub fn render(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(report.verdict.label());
    out.push('\n');
    for s in &report.sentences {
        out.push_str(&s.text);
        out.push('\n');
    }
    out
}
