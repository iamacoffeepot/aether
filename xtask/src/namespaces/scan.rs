//! A comment-, string-, and test-aware pass over one Rust source file.
//!
//! The question this answers — "is this string literal the same text as some
//! actor's declared `NAMESPACE`?" — is a lexical one, so the pass is lexical:
//! a single left-to-right walk that knows where a string literal begins and
//! ends, that an attribute body is not code, and that a `#[cfg(test)]` region
//! is not production. `syn` would answer the first three, but the fourth
//! question the report needs — *which line* — costs `proc-macro2`'s
//! `span-locations` feature across the whole workspace build, so the walk
//! carries its own line counter instead.

use std::path::{Path, PathBuf};

/// One `const NAMESPACE` (or `const …_NAMESPACE`) bound to a string literal.
#[derive(Debug, Clone)]
pub struct Declaration {
    pub namespace: String,
    pub crate_name: String,
    pub path: PathBuf,
    pub line: usize,
}

/// One string literal in production code, with the position that reports it.
#[derive(Debug, Clone)]
pub struct Literal {
    pub value: String,
    pub crate_name: String,
    pub path: PathBuf,
    pub line: usize,
}

/// A namespace is a dotted name (ADR-0099 §1). The rule doubles as the filter
/// that keeps a `const …_NAMESPACE` holding a non-address — a wasm export
/// name, an ellipsis in a derive's error text — out of the declared set, where
/// it would match every unrelated occurrence of that word.
pub fn is_namespace(value: &str) -> bool {
    value.contains('.') && value.split('.').all(|segment| !segment.is_empty())
}

/// What one file contributes, before the tree decides whether the file itself
/// is production code.
pub struct FileScan {
    pub declarations: Vec<Declaration>,
    pub literals: Vec<Literal>,
    /// `mod name;` declarations, with whether the declaration is `#[cfg(test)]`
    /// -gated. A test module's file is entirely test code, which a per-file
    /// walk cannot see on its own: `tools/tests/mail.rs` reads as ordinary
    /// source until you look at the `mod tests;` that pulled it in.
    pub modules: Vec<ModuleDeclaration>,
}

/// One `mod name;` — the edge that carries test-ness from a declaring file to
/// a declared one.
pub struct ModuleDeclaration {
    pub name: String,
    pub test: bool,
}

/// Scan one file's production-code declarations, literals, and module edges.
///
/// Inline test regions contribute nothing: a fixture actor's `NAMESPACE` is
/// not an address anything ships, and a test that names a mailbox by string is
/// a different finding with a different fix.
pub fn scan(source: &str, crate_name: &str, path: &Path) -> FileScan {
    let mut found = FileScan { declarations: Vec::new(), literals: Vec::new(), modules: Vec::new() };
    let mut walk = Walk::new(source);
    while let Some(token) = walk.next_token() {
        match token {
            Token::Declaration { value, line } if is_namespace(&value) => found.declarations.push(Declaration {
                namespace: value,
                crate_name: crate_name.to_owned(),
                path: path.to_path_buf(),
                line,
            }),
            Token::Literal { value, line } => found.literals.push(Literal {
                value,
                crate_name: crate_name.to_owned(),
                path: path.to_path_buf(),
                line,
            }),
            Token::Module { name, test } => found.modules.push(ModuleDeclaration { name, test }),
            Token::Declaration { .. } => {}
        }
    }
    found
}

/// Whether an attribute body marks what follows as test code: the `#[test]`
/// family (`#[tokio::test]` included), or a `cfg` predicate with `test` among
/// its terms — `cfg(any(test, feature = "…"))` gates a test module as surely
/// as bare `cfg(test)`, and `feature = "testing"` must not be mistaken for one.
fn marks_test(attribute: &str) -> bool {
    if attribute == "test" || attribute.ends_with("::test") {
        return true;
    }
    let Some(predicate) = attribute.strip_prefix("cfg") else {
        return false;
    };
    predicate.split(|c: char| !c.is_alphanumeric() && c != '_').any(|term| term == "test")
}

enum Token {
    /// A string literal that is the right-hand side of a `NAMESPACE` const.
    Declaration { value: String, line: usize },
    /// Any other string literal in production code.
    Literal { value: String, line: usize },
    /// A `mod name;` pointing at another file.
    Module { name: String, test: bool },
}

/// The walk's cursor: position, line, and the three pieces of context a string
/// literal's meaning depends on — whether a `NAMESPACE` const is open, whether
/// an attribute body is open, and whether a test region is open.
struct Walk {
    chars: Vec<char>,
    at: usize,
    line: usize,
    /// Brace depth at which the enclosing test region opened, if any.
    test_depth: Option<usize>,
    depth: usize,
    /// A `#[cfg(test)]` / `#[test]` attribute has been read and the item it
    /// marks has not opened its block yet.
    pending_test: bool,
    /// `const NAMESPACE` has been read and its initializer has not been.
    pending_declaration: bool,
    previous_identifier: String,
    /// A `mod name;` read by [`read_identifier`](Self::read_identifier),
    /// waiting to be handed back by the token loop.
    pending_module: Option<Token>,
}

impl Walk {
    fn new(source: &str) -> Self {
        Self {
            chars: source.chars().collect(),
            at: 0,
            line: 1,
            test_depth: None,
            depth: 0,
            pending_test: false,
            pending_declaration: false,
            previous_identifier: String::new(),
            pending_module: None,
        }
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let found = self.peek(0)?;
        self.at += 1;
        if found == '\n' {
            self.line += 1;
        }
        Some(found)
    }

    fn in_test(&self) -> bool {
        self.test_depth.is_some()
    }

    /// Advance to the next string literal in code position, classifying it.
    /// Everything else — comments, attributes, char literals, lifetimes — is
    /// consumed for its side effects on the cursor's context and skipped.
    fn next_token(&mut self) -> Option<Token> {
        while let Some(found) = self.peek(0) {
            if let Some(module) = self.pending_module.take() {
                return Some(module);
            }
            match found {
                '/' if self.peek(1) == Some('/') => self.skip_line_comment(),
                '/' if self.peek(1) == Some('*') => self.skip_block_comment(),
                '#' => self.read_attribute(),
                '\'' => self.skip_quote(),
                '"' => {
                    let line = self.line;
                    let value = self.read_string(0);
                    let declaration = self.pending_declaration;
                    self.pending_declaration = false;
                    if self.in_test() {
                        continue;
                    }
                    return Some(if declaration {
                        Token::Declaration { value, line }
                    } else {
                        Token::Literal { value, line }
                    });
                }
                'r' if self.raw_string_hashes().is_some() => {
                    let line = self.line;
                    let hashes = self.raw_string_hashes().unwrap_or(0);
                    self.at += 1;
                    let value = self.read_string(hashes);
                    self.pending_declaration = false;
                    if self.in_test() {
                        continue;
                    }
                    return Some(Token::Literal { value, line });
                }
                '{' => {
                    self.bump();
                    if self.pending_test && self.test_depth.is_none() {
                        self.test_depth = Some(self.depth);
                    }
                    self.pending_test = false;
                    self.pending_declaration = false;
                    self.depth += 1;
                }
                '}' => {
                    self.bump();
                    self.depth = self.depth.saturating_sub(1);
                    if self.test_depth == Some(self.depth) {
                        self.test_depth = None;
                    }
                    self.pending_declaration = false;
                }
                ';' => {
                    self.bump();
                    self.pending_test = false;
                    self.pending_declaration = false;
                }
                _ if found.is_alphanumeric() || found == '_' => {
                    self.read_identifier();
                    if self.pending_module.is_some() && !self.in_test() {
                        continue;
                    }
                    self.pending_module = None;
                }
                _ => {
                    self.bump();
                }
            }
        }
        None
    }

    fn read_identifier(&mut self) {
        let mut identifier = String::new();
        while let Some(found) = self.peek(0) {
            if found.is_alphanumeric() || found == '_' {
                identifier.push(found);
                self.bump();
            } else {
                break;
            }
        }
        let names_a_namespace = identifier == "NAMESPACE" || identifier.ends_with("_NAMESPACE");
        if names_a_namespace && self.previous_identifier == "const" {
            self.pending_declaration = true;
        }
        if self.previous_identifier == "mod" && self.declares_an_external_module() {
            self.pending_module = Some(Token::Module { name: identifier.clone(), test: self.pending_test });
        }
        self.previous_identifier = identifier;
    }

    /// Whether the identifier just read closes a `mod name;` rather than
    /// opening a `mod name { … }` — the difference between an edge to another
    /// file and an inline region this walk already covers.
    fn declares_an_external_module(&self) -> bool {
        let mut ahead = 0;
        while self.peek(ahead).is_some_and(char::is_whitespace) {
            ahead += 1;
        }
        self.peek(ahead) == Some(';')
    }

    fn skip_line_comment(&mut self) {
        while let Some(found) = self.bump() {
            if found == '\n' {
                break;
            }
        }
    }

    fn skip_block_comment(&mut self) {
        let mut nesting = 0usize;
        while self.peek(0).is_some() {
            if self.peek(0) == Some('/') && self.peek(1) == Some('*') {
                nesting += 1;
                self.bump();
                self.bump();
            } else if self.peek(0) == Some('*') && self.peek(1) == Some('/') {
                nesting -= 1;
                self.bump();
                self.bump();
                if nesting == 0 {
                    return;
                }
            } else {
                self.bump();
            }
        }
    }

    /// Consume `#[…]` / `#![…]` whole. Its string literals are documentation
    /// and configuration, never a send site — and its text is where a test
    /// region announces itself.
    fn read_attribute(&mut self) {
        self.bump();
        if self.peek(0) == Some('!') {
            self.bump();
        }
        if self.peek(0) != Some('[') {
            return;
        }
        let mut body = String::new();
        let mut nesting = 0usize;
        while let Some(found) = self.peek(0) {
            match found {
                '[' => nesting += 1,
                ']' => nesting -= 1,
                '"' => {
                    self.read_string(0);
                    continue;
                }
                _ => {}
            }
            body.push(found);
            self.bump();
            if nesting == 0 {
                break;
            }
        }
        let trimmed = body.trim_matches(|c| c == '[' || c == ']').trim();
        if marks_test(trimmed) {
            self.pending_test = true;
        }
    }

    /// A `'` opens a char literal only when it closes again within two
    /// characters; otherwise it is a lifetime, and the identifier after it is
    /// ordinary code.
    fn skip_quote(&mut self) {
        let escaped = self.peek(1) == Some('\\');
        let closes_immediately = self.peek(2) == Some('\'');
        if !escaped && !closes_immediately {
            self.bump();
            return;
        }
        self.bump();
        while let Some(found) = self.bump() {
            if found == '\\' {
                self.bump();
            } else if found == '\'' {
                return;
            }
        }
    }

    /// The number of `#`s in a raw-string opener at the cursor, or `None` when
    /// the `r` starts an ordinary identifier.
    fn raw_string_hashes(&self) -> Option<usize> {
        if self.peek(0) != Some('r') {
            return None;
        }
        if self.at > 0 {
            let before = self.chars[self.at - 1];
            if before.is_alphanumeric() || before == '_' {
                return None;
            }
        }
        let mut hashes = 0;
        while self.peek(1 + hashes) == Some('#') {
            hashes += 1;
        }
        (self.peek(1 + hashes) == Some('"')).then_some(hashes)
    }

    /// Read a string literal body, cursor on the opening `"`. `hashes` is zero
    /// for an ordinary literal (backslash escapes) and the raw-string hash
    /// count otherwise (no escapes; closed by `"` plus that many `#`).
    fn read_string(&mut self, hashes: usize) -> String {
        self.bump();
        let mut value = String::new();
        while let Some(found) = self.bump() {
            if hashes == 0 && found == '\\' {
                self.bump();
                value.push('\\');
                continue;
            }
            if found == '"' {
                if (0..hashes).all(|ahead| self.peek(ahead) == Some('#')) {
                    for _ in 0..hashes {
                        self.bump();
                    }
                    break;
                }
                value.push(found);
                continue;
            }
            value.push(found);
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_source(source: &str) -> FileScan {
        scan(source, "fixture", Path::new("src/lib.rs"))
    }

    /// Tripwire: the three positions a namespace-looking string can sit in
    /// that are *not* a hand-named address — a doc comment, an attribute body,
    /// and a `#[cfg(test)]` region. Each was a false positive the first pass
    /// reported, and each would push a real finding off the end of a report an
    /// author then stops reading.
    #[test]
    fn comments_attributes_and_test_regions_are_not_code() {
        let found = scan_source(
            r#"
/// Send to `"aether.render"` every frame.
// const NAMESPACE: &'static str = "aether.commented";
#[doc = "aether.attribute"]
const NAMESPACE: &'static str = "aether.real";
fn live() { let _ = "aether.render"; }
#[cfg(test)]
mod tests {
    const NAMESPACE: &'static str = "test.fixture";
    fn hidden() { let _ = "aether.render"; }
}
"#,
        );

        assert_eq!(found.declarations.iter().map(|d| d.namespace.as_str()).collect::<Vec<_>>(), ["aether.real"]);
        assert_eq!(found.literals.iter().map(|l| l.value.as_str()).collect::<Vec<_>>(), ["aether.render"]);
        assert_eq!(found.literals[0].line, 6);
    }

    /// Tripwire: the `mod name;` edge that carries test-ness into another file.
    /// Without it a test-only module's own source reads as production — which
    /// is how a fixture actor's `NAMESPACE` gets counted as a declaration and
    /// silently licenses every duplicate of that name in its crate.
    #[test]
    fn a_cfg_test_module_declaration_is_reported_as_an_edge() {
        let found =
            scan_source("#[cfg(test)]\nmod fixtures;\n#[cfg(any(test, feature = \"probe\"))]\nmod gated;\nmod live;\n");

        let edges: Vec<(&str, bool)> = found.modules.iter().map(|m| (m.name.as_str(), m.test)).collect();
        assert_eq!(edges, [("fixtures", true), ("gated", true), ("live", false)]);
    }

    /// Tripwire: the lexical shapes that desynchronize a naive scanner and
    /// make everything after them garbage — a lifetime's unpaired quote, a
    /// raw string holding a quote, and an escaped quote inside an ordinary
    /// one. A desync is silent: the pass keeps running and reports nonsense.
    #[test]
    fn lifetimes_raw_strings_and_escapes_keep_the_walk_in_step() {
        let found = scan_source(
            r##"
fn quoted<'a>(_: &'a str) {
    let _ = r#"a "quoted" raw"#;
    let _ = "escaped \" quote";
    let _ = "aether.render";
}
"##,
        );

        assert_eq!(found.literals.last().map(|l| l.value.as_str()), Some("aether.render"));
        assert_eq!(found.literals.last().map(|l| l.line), Some(5));
    }
}
