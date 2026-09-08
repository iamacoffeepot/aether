//! `refs <rev> <symbol-name>` — defining vs referencing hits, with TEST marks.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use crate::extract::{self, ExtractedItem, extract_source};
use crate::git::{self, GrepHit};
use crate::resolve::{self, FileImports};

/// One reading file at one revision, parsed once and reused across its hits.
struct ReadingFile {
    src: String,
    file: syn::File,
    items: Vec<ExtractedItem>,
    imports: FileImports,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Defining,
    Referencing,
}

#[derive(Clone, Debug)]
pub struct RefHit {
    pub role: Role,
    pub ident: String,
    pub file: String,
    pub line: usize,
    pub is_test: bool,
    pub test_fn: Option<String>,
    pub defining_path: Option<String>,
    #[allow(dead_code)]
    pub text: String,
    pub plausible: bool,
}

impl RefHit {
    pub fn render(&self) -> String {
        let role = match self.role {
            Role::Defining => "defining",
            Role::Referencing => "referencing",
        };
        let test = if self.is_test { "TEST" } else { "live" };
        let path = self.defining_path.as_deref().unwrap_or(&self.ident);
        match &self.test_fn {
            Some(fn_name) if self.is_test => {
                format!("{role}\t{test}\t{path}\t{}:{}\t{fn_name}", self.file, self.line)
            }
            _ => format!("{role}\t{test}\t{path}\t{}:{}", self.file, self.line),
        }
    }
}

pub fn find_refs(repo: &Path, rev: &str, name: &str) -> Result<Vec<RefHit>> {
    find_refs_filtered(repo, rev, name, None)
}

/// If `want_path` is set, a defining hit must match that stable path; other same-ident definitions are ignored.
pub fn find_refs_filtered(repo: &Path, rev: &str, name: &str, want_path: Option<&str>) -> Result<Vec<RefHit>> {
    let hits = git::grep_ident(repo, rev, name)?;
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_file: HashMap<String, Vec<GrepHit>> = HashMap::new();
    for hit in hits {
        by_file.entry(hit.path.clone()).or_default().push(hit);
    }
    let mut parsed: HashMap<String, Option<ReadingFile>> = HashMap::new();
    let mut out = Vec::new();
    for (path, file_hits) in by_file {
        let entry = match parsed.get(&path) {
            Some(v) => v,
            None => {
                let loaded = match git::show_file(repo, rev, &path)? {
                    Some(src) => match syn::parse_file(&src) {
                        Ok(file) => {
                            let items = extract_source(&path, &src).unwrap_or_default();
                            let imports = FileImports::collect(&path, &file);
                            Some(ReadingFile {
                                src,
                                file,
                                items,
                                imports,
                            })
                        }
                        Err(err) => {
                            eprintln!("parse {rev}:{path}: {err}");
                            None
                        }
                    },
                    None => None,
                };
                parsed.insert(path.clone(), loaded);
                parsed.get(&path).unwrap()
            }
        };
        match entry {
            Some(reading) => {
                for hit in file_hits {
                    out.push(classify_hit(&path, reading, name, want_path, &hit));
                }
            }
            None => {
                let is_test = extract::is_tests_dir_file(&path);
                for hit in file_hits {
                    out.push(RefHit {
                        role: Role::Referencing,
                        ident: name.to_string(),
                        file: path.clone(),
                        line: hit.line,
                        is_test,
                        test_fn: None,
                        defining_path: None,
                        text: hit.text,
                        plausible: want_path.is_none(),
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    Ok(out)
}

fn classify_hit(path: &str, reading: &ReadingFile, name: &str, want_path: Option<&str>, hit: &GrepHit) -> RefHit {
    let named: Vec<&ExtractedItem> = reading.items.iter().filter(|i| i.ident == name).collect();
    let defining = named
        .iter()
        .copied()
        .find(|i| i.line == hit.line && want_path.is_none_or(|p| i.path == p));

    let (is_test, test_fn) = extract::test_enclosing(path, &reading.file, hit.line);
    let is_test = is_test || extract::is_tests_dir_file(path) || defining.is_some_and(|i| i.in_test);
    let plausible = hit_plausible(reading, name, want_path, hit);

    if let Some(item) = defining {
        return RefHit {
            role: Role::Defining,
            ident: name.to_string(),
            file: path.to_string(),
            line: hit.line,
            is_test,
            test_fn,
            defining_path: Some(item.path.clone()),
            text: hit.text.clone(),
            plausible: true,
        };
    }

    RefHit {
        role: Role::Referencing,
        ident: name.to_string(),
        file: path.to_string(),
        line: hit.line,
        is_test,
        test_fn,
        defining_path: want_path.map(str::to_string),
        text: hit.text.clone(),
        plausible,
    }
}

/// Whether this hit is a reference to the item `want_path` names.
///
/// Round 1 asked only whether the file was in the same cargo package or
/// mentioned the crate anywhere, which kept every same-crate occurrence of a
/// common word. The question is now put to the file's import surface
/// ([`resolve::resolves_to`]), and prose is dropped outright: a comment, or a
/// line whose every occurrence sits inside a string literal, is not a reader.
fn hit_plausible(reading: &ReadingFile, name: &str, want_path: Option<&str>, hit: &GrepHit) -> bool {
    let trimmed = hit.text.trim_start();
    if trimmed.starts_with("//") {
        return false;
    }
    if resolve::only_inside_string_literal(&hit.text, name) {
        return false;
    }
    let Some(want) = want_path else {
        return true;
    };
    resolve::resolves_to(&reading.imports, &reading.items, &reading.src, want, name)
}
