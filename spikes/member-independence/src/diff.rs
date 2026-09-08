//! Classify extracted items between two revisions.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::Result;

use crate::extract::{ExtractedItem, ItemKind, extract_source};
use crate::git;

const RENAME_MIN_BODY_TOKENS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    SignatureChanged,
    BodyChanged,
    Renamed {
        from: String,
        to: String,
    },
    /// The name still resolves at the same path, through a `pub use` on one
    /// side or the other: a local definition replaced by a re-export of the
    /// same name, or a re-export retargeted. Round 1 reported both as
    /// `SignatureChanged` and called the pair a conflict, but a reader writing
    /// the same path still compiles — the definition moved, the name did not.
    Reexport,
}

impl ChangeKind {
    pub fn label(&self) -> String {
        match self {
            ChangeKind::Added => "Added".into(),
            ChangeKind::Removed => "Removed".into(),
            ChangeKind::SignatureChanged => "SignatureChanged".into(),
            ChangeKind::BodyChanged => "BodyChanged".into(),
            ChangeKind::Renamed { .. } => "Renamed".into(),
            ChangeKind::Reexport => "Reexport".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Change {
    pub kind: ChangeKind,
    pub path: String,
    pub file: String,
    pub line: usize,
    pub ident: String,
    #[allow(dead_code)]
    pub in_test: bool,
    pub signature_hash: String,
    pub body_hash: String,
}

impl Change {
    pub fn display_path(&self) -> String {
        match &self.kind {
            ChangeKind::Renamed { from, to } => format!("{from} -> {to}"),
            _ => self.path.clone(),
        }
    }

    pub fn render(&self) -> String {
        format!(
            "{}\t{}\t{}:{}",
            self.kind.label(),
            self.display_path(),
            self.file,
            self.line
        )
    }
}

pub fn diff_revs(repo: &Path, base: &str, head: &str, prefixes: &[String]) -> Result<Vec<Change>> {
    let files = git::changed_rs_files(repo, base, head, prefixes)?;
    let mut base_items = BTreeMap::new();
    let mut head_items = BTreeMap::new();
    for file in &files {
        if let Some(src) = git::show_file(repo, base, file)? {
            match extract_source(file, &src) {
                Ok(items) => merge_items(&mut base_items, items),
                Err(err) => eprintln!("parse {base}:{file}: {err}"),
            }
        }
        if let Some(src) = git::show_file(repo, head, file)? {
            match extract_source(file, &src) {
                Ok(items) => merge_items(&mut head_items, items),
                Err(err) => eprintln!("parse {head}:{file}: {err}"),
            }
        }
    }
    Ok(classify(base_items, head_items))
}

fn merge_items(into: &mut BTreeMap<String, ExtractedItem>, items: Vec<ExtractedItem>) {
    for item in items {
        into.entry(item.path.clone()).or_insert(item);
    }
}

pub fn classify(
    base_items: BTreeMap<String, ExtractedItem>,
    head_items: BTreeMap<String, ExtractedItem>,
) -> Vec<Change> {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();

    for (path, item) in &head_items {
        match base_items.get(path) {
            None => added.push(item.clone()),
            Some(prev) => {
                if prev.signature_hash == item.signature_hash && prev.body_hash == item.body_hash {
                    continue;
                }
                let kind = if prev.kind == ItemKind::Use || item.kind == ItemKind::Use {
                    ChangeKind::Reexport
                } else if prev.signature_hash != item.signature_hash {
                    ChangeKind::SignatureChanged
                } else {
                    ChangeKind::BodyChanged
                };
                changed.push(Change {
                    kind,
                    path: path.clone(),
                    file: item.file.clone(),
                    line: item.line,
                    ident: item.ident.clone(),
                    in_test: item.in_test,
                    signature_hash: item.signature_hash.clone(),
                    body_hash: item.body_hash.clone(),
                });
            }
        }
    }
    for (path, item) in &base_items {
        if !head_items.contains_key(path) {
            removed.push(item.clone());
        }
    }

    let (added, removed, mut renames) = pair_renames(added, removed);
    let mut out = Vec::new();
    out.extend(added.into_iter().map(|item| Change {
        kind: ChangeKind::Added,
        path: item.path,
        file: item.file,
        line: item.line,
        ident: item.ident,
        in_test: item.in_test,
        signature_hash: item.signature_hash,
        body_hash: item.body_hash,
    }));
    out.extend(removed.into_iter().map(|item| Change {
        kind: ChangeKind::Removed,
        path: item.path,
        file: item.file,
        line: item.line,
        ident: item.ident,
        in_test: item.in_test,
        signature_hash: item.signature_hash,
        body_hash: item.body_hash,
    }));
    out.extend(renames.drain(..));
    out.extend(changed);
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.file.cmp(&b.file)).then(a.line.cmp(&b.line)));
    out
}

fn pair_renames(
    added: Vec<ExtractedItem>,
    removed: Vec<ExtractedItem>,
) -> (Vec<ExtractedItem>, Vec<ExtractedItem>, Vec<Change>) {
    let mut add_by_hash: HashMap<String, Vec<ExtractedItem>> = HashMap::new();
    for item in added {
        if item.body_token_count >= RENAME_MIN_BODY_TOKENS {
            add_by_hash.entry(item.body_hash.clone()).or_default().push(item);
        } else {
            add_by_hash
                .entry(format!("uniq-add-{}", add_by_hash.len()))
                .or_default()
                .push(item);
        }
    }
    let mut rem_by_hash: HashMap<String, Vec<ExtractedItem>> = HashMap::new();
    for item in removed {
        if item.body_token_count >= RENAME_MIN_BODY_TOKENS {
            rem_by_hash.entry(item.body_hash.clone()).or_default().push(item);
        } else {
            rem_by_hash
                .entry(format!("uniq-rem-{}", rem_by_hash.len()))
                .or_default()
                .push(item);
        }
    }

    let mut leftover_add = Vec::new();
    let mut leftover_rem = Vec::new();
    let mut renames = Vec::new();

    let hashes: Vec<String> = add_by_hash.keys().cloned().collect();
    for hash in hashes {
        let adds = add_by_hash.remove(&hash).unwrap_or_default();
        let rems = rem_by_hash.remove(&hash).unwrap_or_default();
        if adds.len() == 1 && rems.len() == 1 && !hash.starts_with("uniq-") {
            let to = adds.into_iter().next().unwrap();
            let from = rems.into_iter().next().unwrap();
            if from.path != to.path {
                renames.push(Change {
                    kind: ChangeKind::Renamed {
                        from: from.path.clone(),
                        to: to.path.clone(),
                    },
                    path: to.path.clone(),
                    file: to.file.clone(),
                    line: to.line,
                    ident: to.ident.clone(),
                    in_test: to.in_test,
                    signature_hash: to.signature_hash.clone(),
                    body_hash: to.body_hash.clone(),
                });
                continue;
            }
            leftover_add.push(to);
            leftover_rem.push(from);
        } else {
            leftover_add.extend(adds);
            leftover_rem.extend(rems);
        }
    }
    for (_, rems) in rem_by_hash {
        leftover_rem.extend(rems);
    }
    (leftover_add, leftover_rem, renames)
}

pub fn packages_of(changes: &[Change]) -> Vec<String> {
    let mut pkgs: Vec<String> = changes
        .iter()
        .filter_map(|c| git::cargo_package_from_path(&c.file))
        .collect();
    pkgs.sort();
    pkgs.dedup();
    pkgs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::extract_source;

    fn items(path: &str, src: &str) -> BTreeMap<String, ExtractedItem> {
        let mut map = BTreeMap::new();
        merge_items(&mut map, extract_source(path, src).unwrap());
        map
    }

    #[test]
    fn classifies_add_remove_sig_body() {
        let base = items(
            "crates/demo/src/lib.rs",
            "pub fn keep(x: i32) -> i32 { x + 1 }\npub fn gone() { 1 }\n",
        );
        let head = items(
            "crates/demo/src/lib.rs",
            "pub fn keep(x: i32) -> i32 { x + 2 }\npub fn keep2(x: i32, y: i32) -> i32 { x + y }\n",
        );
        let changes = classify(base, head);
        let kinds: Vec<_> = changes.iter().map(|c| (c.kind.label(), c.path.clone())).collect();
        assert!(kinds.iter().any(|(k, p)| k == "Removed" && p.ends_with("::gone")));
        assert!(kinds.iter().any(|(k, p)| k == "Added" && p.ends_with("::keep2")));
        assert!(kinds.iter().any(|(k, p)| k == "BodyChanged" && p.ends_with("::keep")));
    }

    // Tripwire: the `path_in_surface` miss. A local definition replaced by a
    // `pub use` of the same name kept every call site compiling, and round 1
    // called the pair Conflict(Breaks) for it.
    #[test]
    fn a_local_definition_replaced_by_a_reexport_is_not_a_signature_change() {
        let path = "crates/aether-chassis-bloomery/src/bloomery/verify/containment.rs";
        let base = items(
            path,
            "pub fn path_in_surface(surface: &[String], path: &str) -> bool { surface.iter().any(|g| g == path) }\n",
        );
        let head = items(path, "pub use aether_bloomery::path_in_surface;\n");
        let changes = classify(base, head);
        let reexport = changes
            .iter()
            .find(|change| change.path.ends_with("::path_in_surface"))
            .expect("the name is still there");
        assert_eq!(reexport.kind, ChangeKind::Reexport, "{changes:?}");
    }

    #[test]
    fn detects_rename_by_normalized_body_hash() {
        let src_a = r#"
            pub fn old_name(x: i32) -> i32 {
                let a = x + 1;
                let b = a * 2;
                b - 3
            }
        "#;
        let src_b = r#"
            pub fn new_name(y: i32) -> i32 {
                let a = y + 1;
                let b = a * 2;
                b - 3
            }
        "#;
        let changes = classify(
            items("crates/demo/src/lib.rs", src_a),
            items("crates/demo/src/lib.rs", src_b),
        );
        assert!(
            changes.iter().any(|c| matches!(&c.kind, ChangeKind::Renamed { from, to } if from.ends_with("::old_name") && to.ends_with("::new_name"))),
            "{changes:?}"
        );
    }
}
