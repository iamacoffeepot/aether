//! Workspace crate discovery and per-crate `.rs` file walk.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use syn::parse_file;

use crate::symbols::extract::{self, FileModChild};
use crate::symbols::table::{Symbol, Table};

pub struct CrateSpec {
    pub name: String,
    pub root: PathBuf,
}

/// Build the inventory over every workspace member, including test modules
/// and `tests/` trees.
pub fn build_workspace_table() -> Result<Table> {
    let metadata = MetadataCommand::new().no_deps().exec().context("run cargo metadata")?;
    let workspace_root = metadata.workspace_root.as_std_path();
    let mut symbols = Vec::new();
    for spec in crates_from_metadata(&metadata) {
        symbols.extend(collect_crate(&spec.name, &spec.root, workspace_root)?);
    }
    Ok(Table::new(symbols))
}

pub fn crates_from_metadata(metadata: &cargo_metadata::Metadata) -> Vec<CrateSpec> {
    let mut crates: Vec<CrateSpec> = metadata
        .workspace_packages()
        .into_iter()
        .filter_map(|package| {
            let root = package.manifest_path.parent()?.as_std_path().to_path_buf();
            Some(CrateSpec { name: package.name.replace('-', "_"), root })
        })
        .collect();
    crates.sort_by(|left, right| left.name.cmp(&right.name).then_with(|| left.root.cmp(&right.root)));
    crates
}

pub fn collect_crate(crate_name: &str, crate_root: &Path, workspace_root: &Path) -> Result<Vec<Symbol>> {
    let files = rust_files(crate_root);
    let mut parsed = Vec::new();
    for abs in files {
        let source = match fs::read_to_string(&abs) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", abs.display()));
            }
        };
        let file = match parse_file(&source) {
            Ok(file) => file,
            Err(error) => {
                eprintln!("symbols: skip {} ({error})", abs.display());
                continue;
            }
        };
        parsed.push(ParsedFile {
            rel_crate: rel_path(crate_root, &abs),
            rel_workspace: rel_path(workspace_root, &abs),
            abs,
            file,
        });
    }

    let test_files = test_file_set(crate_root, &parsed);
    let mut symbols = Vec::new();
    for file in &parsed {
        let file_is_test = test_files.contains(&file.abs);
        for mut symbol in extract::extract_parsed(crate_name, &file.rel_crate, &file.file, file_is_test) {
            symbol.path.clone_from(&file.rel_workspace);
            symbols.push(symbol);
        }
    }
    Ok(symbols)
}

struct ParsedFile {
    abs: PathBuf,
    rel_crate: String,
    rel_workspace: String,
    file: syn::File,
}

pub fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if !matches!(entry.file_name().to_str(), Some("target" | ".git")) {
                    stack.push(path);
                }
            } else if file_type.is_file() && path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn test_file_set(crate_root: &Path, parsed: &[ParsedFile]) -> BTreeSet<PathBuf> {
    let mut test_files = BTreeSet::new();
    for file in parsed {
        if is_tests_tree(crate_root, &file.abs) {
            test_files.insert(file.abs.clone());
        }
    }

    let mut decls: BTreeMap<PathBuf, Vec<FileModChild>> = BTreeMap::new();
    for file in parsed {
        decls.insert(file.abs.clone(), extract::file_mod_children(&file.abs, &file.file.items, false));
    }

    let mut changed = true;
    while changed {
        changed = false;
        for (parent, children) in &decls {
            let parent_is_test = test_files.contains(parent);
            for child in children {
                if (parent_is_test || child.test) && test_files.insert(child.path.clone()) {
                    changed = true;
                }
            }
        }
    }
    test_files
}

fn is_tests_tree(crate_root: &Path, file: &Path) -> bool {
    file.strip_prefix(crate_root)
        .ok()
        .is_some_and(|rel| rel.components().next().is_some_and(|component| component.as_os_str() == "tests"))
}

pub fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{collect_crate, crates_from_metadata, rust_files};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn scratch_crate() -> PathBuf {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!("aether-xtask-symbols-{}-{seq}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).expect("src");
        fs::create_dir_all(root.join("tests")).expect("tests");
        root
    }

    #[test]
    fn walk_covers_cfg_test_file_mod_and_tests_tree() {
        // A file-backed `#[cfg(test)] mod helpers` and a `tests/` integration
        // file are the two places private helpers hide from rustdoc. A walk
        // that only read `src/lib.rs` would miss both.
        let root = scratch_crate();
        fs::write(root.join("src/lib.rs"), "pub fn live() {}\n#[cfg(test)]\nmod helpers;\n").expect("lib");
        fs::write(root.join("src/helpers.rs"), "fn scratch_dir() {}\n").expect("helpers");
        fs::write(root.join("tests/integration.rs"), "fn digest() {}\n").expect("integration");

        let files = rust_files(&root);
        assert!(
            files.iter().any(|path| path.ends_with("src/helpers.rs")),
            "file-backed test module is walked: {files:?}"
        );
        assert!(files.iter().any(|path| path.ends_with("tests/integration.rs")), "tests/ tree is walked: {files:?}");

        let symbols = collect_crate("demo", &root, &root).expect("collect");
        let helper = symbols.iter().find(|symbol| symbol.name == "scratch_dir").expect("cfg(test) helper");
        assert!(helper.test, "file reached only through #[cfg(test)] mod is marked test");
        assert_eq!(helper.visibility, "private");
        let digest = symbols.iter().find(|symbol| symbol.name == "digest").expect("tests/ helper");
        assert!(digest.test, "tests/ tree symbols are marked test");
        let live = symbols.iter().find(|symbol| symbol.name == "live").expect("production fn");
        assert!(!live.test);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_members_are_the_inventory_crates() {
        // A walk over `packages` (every crate cargo metadata knows, including
        // crates.io deps) would flood the table; the census is workspace-local.
        let metadata = cargo_metadata::MetadataCommand::new().no_deps().exec().expect("cargo metadata");
        let names: Vec<String> = crates_from_metadata(&metadata).into_iter().map(|spec| spec.name).collect();
        assert!(names.iter().any(|name| name == "xtask"), "{names:?}");
        assert!(names.iter().any(|name| name == "aether_data"), "{names:?}");
        assert!(
            !names.iter().any(|name| name == "syn" || name == "serde"),
            "registry crates are not workspace members: {names:?}"
        );
    }

    #[test]
    fn rust_files_skips_target_and_sorts() {
        let root = scratch_crate();
        fs::write(root.join("src/lib.rs"), "fn a() {}\n").expect("lib");
        fs::create_dir_all(root.join("target")).expect("target");
        fs::write(root.join("target/out.rs"), "fn skipped() {}\n").expect("target file");
        let files = rust_files(&root);
        assert!(files.iter().all(|path| !path.components().any(|c| c.as_os_str() == "target")));
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted, "walk order is sorted so two runs agree");
        let _ = fs::remove_dir_all(&root);
    }
}
