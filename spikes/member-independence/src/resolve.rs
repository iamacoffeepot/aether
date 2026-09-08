//! Whether a grep hit can name one specific item, decided by the reading file's `use` graph.
//!
//! Round 1 kept a hit whenever the bare identifier appeared in the file, so
//! `xtask::bloom::Endpoint::resolve` collected every `resolve` in the workspace —
//! a diagnostic string in a derive crate, a method on an unrelated audio type —
//! and one pair reached 7,187 sentences. The identifier is not the reference;
//! the reference is a name that *resolves* to the changed item.
//!
//! This module is the resolution step, at the fidelity the file itself affords:
//! the imports it declares, the items it defines, and the crate it belongs to.
//! It is deliberately not a name resolver — no module graph, no glob expansion
//! across crates, no macro expansion. It answers the one question the
//! independence intersection asks: *could this line be reading that item?* and
//! it answers `true` whenever it cannot tell, so the conservative direction is
//! toward more conflicts rather than a silently missed break.

use std::collections::{HashMap, HashSet};

use syn::{Item, UseTree};

use crate::extract::{ExtractedItem, ItemKind};

const MAX_MODULE_DEPTH: usize = 64;

/// One file's import surface: what bare names it binds, and to what.
#[derive(Clone, Debug, Default)]
pub struct FileImports {
    /// The crate this file belongs to, underscored (`aether_bloomery`).
    pub crate_name: String,
    /// Local binding name to the path it was imported from, as written.
    pub named: HashMap<String, String>,
    /// Prefixes of `use a::b::*;`, as written.
    pub globs: HashSet<String>,
}

impl FileImports {
    /// Collect every `use` in the file, at any module depth and any visibility.
    ///
    /// Visibility does not matter here: a private `use` still binds the name
    /// the reading code writes, which is exactly what a reference resolves
    /// through. `#[cfg(test)]` modules are walked for the same reason — a test
    /// that pins a changed item reads it through its own imports.
    pub fn collect(file_path: &str, file: &syn::File) -> Self {
        let (crate_name, _) = crate::extract::crate_and_module(file_path);
        let mut imports = FileImports {
            crate_name,
            ..FileImports::default()
        };
        imports.walk_items(&file.items, 0);
        imports
    }

    fn walk_items(&mut self, items: &[Item], depth: usize) {
        if depth > MAX_MODULE_DEPTH {
            return;
        }
        for item in items {
            match item {
                Item::Use(item) => self.walk_use(&item.tree, &[]),
                Item::Mod(item) => {
                    if let Some((_, inner)) = &item.content {
                        self.walk_items(inner, depth + 1);
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_use(&mut self, tree: &UseTree, prefix: &[String]) {
        match tree {
            UseTree::Path(path) => {
                let mut next = prefix.to_vec();
                next.push(path.ident.to_string());
                self.walk_use(&path.tree, &next);
            }
            UseTree::Name(name) => {
                let mut full = prefix.to_vec();
                full.push(name.ident.to_string());
                self.named.insert(name.ident.to_string(), full.join("::"));
            }
            UseTree::Rename(rename) => {
                let mut full = prefix.to_vec();
                full.push(rename.ident.to_string());
                self.named.insert(rename.rename.to_string(), full.join("::"));
            }
            UseTree::Glob(_) => {
                self.globs.insert(prefix.join("::"));
            }
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.walk_use(tree, prefix);
                }
            }
        }
    }
}

/// Whether a hit on `ident` in this file can be reading the item at `want_path`.
///
/// The order matters. An explicit import is the strongest evidence available
/// and settles the question in both directions: a file that imports `resolve`
/// from another crate is reading that other crate's `resolve`, not ours. Only
/// when no import binds the name do the weaker signals apply.
pub fn resolves_to(imports: &FileImports, items: &[ExtractedItem], src: &str, want_path: &str, ident: &str) -> bool {
    let Some(target_crate) = want_path.split("::").next().filter(|segment| !segment.is_empty()) else {
        return true;
    };

    if let Some(imported) = imports.named.get(ident) {
        return import_reaches(imported, &imports.crate_name, target_crate);
    }
    if imports.crate_name == target_crate {
        return !shadowed_by_local_definition(items, want_path, ident);
    }
    if imports.globs.iter().any(|prefix| first_segment(prefix) == target_crate) {
        return true;
    }

    // No import binds the name and the file is in another crate. The only way
    // left to reach the item is writing its crate out at the call site.
    src.contains(&format!("{target_crate}::"))
}

/// Whether an import path lands in `target_crate` from a file in `file_crate`.
fn import_reaches(imported: &str, file_crate: &str, target_crate: &str) -> bool {
    match first_segment(imported) {
        "crate" | "self" | "super" => file_crate == target_crate,
        head => head == target_crate,
    }
}

/// Whether the file defines its own item under this name and not the one that changed.
///
/// Inside the target's own crate an item is reachable without a `use` — through
/// `crate::`, `super::`, or plain module scope — so same-crate hits are kept by
/// default. The exception is a local definition of the same name: those hits
/// read the local one.
fn shadowed_by_local_definition(items: &[ExtractedItem], want_path: &str, ident: &str) -> bool {
    let defines_target = items.iter().any(|item| item.path == want_path);
    let defines_other = items
        .iter()
        .any(|item| item.ident == ident && item.path != want_path && item.kind != ItemKind::Use);
    defines_other && !defines_target
}

fn first_segment(path: &str) -> &str {
    path.split("::").next().unwrap_or(path)
}

/// Whether every whole-word occurrence of `ident` on this line sits inside a string literal.
///
/// The dominant surviving noise class after import resolution is prose: a
/// diagnostic message, a doc string, a `format!` template that happens to name
/// the item. None of them are references, and none of them break.
pub fn only_inside_string_literal(text: &str, ident: &str) -> bool {
    let bytes = text.as_bytes();
    let needle = ident.as_bytes();
    let mut in_string = false;
    let mut escaped = false;
    let mut seen = false;
    let mut seen_outside = false;

    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            _ => {}
        }
        if word_match_at(bytes, index, needle) {
            seen = true;
            seen_outside |= !in_string;
        }
    }

    seen && !seen_outside
}

fn word_match_at(haystack: &[u8], index: usize, needle: &[u8]) -> bool {
    let end = index + needle.len();
    if needle.is_empty() || end > haystack.len() || &haystack[index..end] != needle {
        return false;
    }
    let before_is_word = index > 0 && is_word_byte(haystack[index - 1]);
    let after_is_word = end < haystack.len() && is_word_byte(haystack[end]);
    !before_is_word && !after_is_word
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::{FileImports, only_inside_string_literal, resolves_to};
    use crate::extract::extract_source;

    fn imports(path: &str, src: &str) -> FileImports {
        FileImports::collect(path, &syn::parse_file(src).expect("parse"))
    }

    // Tripwire: the 7,187-sentence failure. A file in another crate that binds
    // `resolve` from its own crate must not count as a reader of xtask's
    // `resolve`, or the independence verdict is grep noise again.
    #[test]
    fn an_import_from_another_crate_does_not_read_our_item() {
        let path = "crates/aether-actor-derive/src/handler_parse.rs";
        let src = "use aether_actor::resolve;\nfn go() { resolve(); }\n";
        let items = extract_source(path, src).expect("extract");
        assert!(!resolves_to(
            &imports(path, src),
            &items,
            src,
            "xtask::bloom::Endpoint::resolve",
            "resolve"
        ));
    }

    #[test]
    fn a_file_with_no_import_and_no_qualified_path_does_not_read_our_item() {
        let path = "crates/aether-audio/src/runtime/reverb.rs";
        let src = "struct Room;\nimpl Room { fn resolve(&self) -> u8 { 1 } }\n";
        let items = extract_source(path, src).expect("extract");
        assert!(!resolves_to(
            &imports(path, src),
            &items,
            src,
            "xtask::bloom::Endpoint::resolve",
            "resolve"
        ));
    }

    #[test]
    fn an_import_of_the_target_crate_reads_it() {
        let path = "crates/aether-bloomery-console/src/screen/board.rs";
        let src = "use aether_bloomery::port::projection::MemberView;\nfn show(view: &MemberView) {}\n";
        let items = extract_source(path, src).expect("extract");
        assert!(resolves_to(
            &imports(path, src),
            &items,
            src,
            "aether_bloomery::port::projection::MemberView",
            "MemberView"
        ));
    }

    #[test]
    fn a_glob_import_of_the_target_crate_stays_conservative() {
        let path = "crates/aether-bloomery-console/src/dto.rs";
        let src = "use aether_bloomery::values::*;\nfn take(request: SurfaceRequest) {}\n";
        let items = extract_source(path, src).expect("extract");
        assert!(resolves_to(
            &imports(path, src),
            &items,
            src,
            "aether_bloomery::values::surface::SurfaceRequest",
            "SurfaceRequest"
        ));
    }

    // Tripwire: `done.resolve` inside a diagnostic string is prose, not a call.
    #[test]
    fn an_identifier_only_inside_a_string_literal_is_prose() {
        assert!(only_inside_string_literal(
            r#"            return Err(syn::Error::new(span, "expected done.resolve here"));"#,
            "resolve"
        ));
        assert!(!only_inside_string_literal(
            r#"            let done = endpoint.resolve("resolve");"#,
            "resolve"
        ));
        assert!(!only_inside_string_literal("    endpoint.resolve();", "resolve"));
    }

    #[test]
    fn an_escaped_quote_does_not_flip_the_string_state() {
        assert!(only_inside_string_literal(
            r#"    let text = "a \" resolve b";"#,
            "resolve"
        ));
    }
}
