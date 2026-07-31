//! Reading Rust sources the way these checks need them: comment bytes
//! blanked out, and test-only code separated from the rest.
//!
//! Every transform blanks bytes in place rather than removing them, so an
//! offset into any of the three views is an offset into the original file
//! and [`RustSource::line_of`] can turn a match back into the line number
//! a failure message quotes. Blanking with spaces keeps the buffer valid
//! UTF-8 whatever the comment held.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::{fs, iter};

/// One `.rs` file, pre-processed for scanning.
pub(super) struct RustSource {
    /// Absolute path of the file this was read from.
    pub(super) path: PathBuf,
    /// The file with comment bytes replaced by spaces.
    pub(super) code: String,
    /// [`Self::code`] with every byte outside a test-only item blanked —
    /// the code that exists only in a `cargo test` build.
    pub(super) test_code: String,
    /// [`Self::code`] with every byte inside a test-only item blanked —
    /// the code a dependent compiles against.
    pub(super) non_test_code: String,
}

impl RustSource {
    /// The 1-based line holding `offset`. Offsets come from the blanked
    /// views, so the lookup floors to a character boundary rather than
    /// risking a slice panic on a multi-byte character a blanked span
    /// happened to cover.
    pub(super) fn line_of(&self, offset: usize) -> usize {
        let mut end = offset.min(self.code.len());
        while !self.code.is_char_boundary(end) {
            end -= 1;
        }
        self.code[..end].matches('\n').count() + 1
    }
}

/// Read and pre-process every file in `files`.
///
/// The read happens in two passes because a file's test/non-test split is
/// not decidable from the file alone: `#[cfg(test)] mod helpers;` makes
/// the whole of `helpers.rs` test code while leaving no `#[cfg(test)]`
/// inside it. The first pass blanks comments and collects those
/// declarations; the second applies them.
pub(super) fn read_all(files: &[PathBuf]) -> Vec<RustSource> {
    let blanked: Vec<(PathBuf, String)> =
        files.iter().filter_map(|path| Some((path.clone(), blank_comments(&fs::read_to_string(path).ok()?)))).collect();
    let whole_file_tests = test_module_files(&blanked, files);
    blanked
        .into_iter()
        .map(|(path, code)| {
            let (test_code, non_test_code) = if whole_file_tests.contains(&path) {
                (code.clone(), " ".repeat(code.len()))
            } else {
                split_test_code(&code)
            };
            RustSource { path, code, test_code, non_test_code }
        })
        .collect()
}

/// Every `.rs` file under `root`, plus every symlink found on the way.
/// `target/` and `.git/` are skipped: build output and object storage are
/// not source.
pub(super) struct Walk {
    pub(super) rust_files: Vec<PathBuf>,
    pub(super) symlinks: Vec<PathBuf>,
}

/// Walk `root` iteratively — an explicit stack rather than recursion, so
/// a deep tree cannot overflow. Symlinks are recorded but never followed.
pub(super) fn walk(root: &Path) -> Walk {
    let mut rust_files = Vec::new();
    let mut symlinks = Vec::new();
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
                symlinks.push(path);
            } else if file_type.is_dir() {
                if !matches!(entry.file_name().to_str(), Some("target" | ".git")) {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                rust_files.push(path);
            }
        }
    }
    rust_files.sort();
    symlinks.sort();
    Walk { rust_files, symlinks }
}

/// The files a test-only `mod name;` declaration pulls in, resolved
/// against `files` — both the `name.rs` sibling and everything under a
/// `name/` directory, since a whole module subtree reached through a
/// test-only declaration is test code.
fn test_module_files(blanked: &[(PathBuf, String)], files: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut roots = Vec::new();
    for (path, code) in blanked {
        // `foo/mod.rs` declares its children in `foo/`; `foo.rs` declares
        // them in the sibling `foo/` directory.
        let module_dir = if path.file_name().is_some_and(|name| name == "mod.rs") {
            path.parent().map(Path::to_path_buf)
        } else {
            Some(path.with_extension(""))
        };
        let Some(module_dir) = module_dir else {
            continue;
        };
        for name in test_only_module_declarations(code) {
            roots.push(module_dir.join(format!("{name}.rs")));
            roots.push(module_dir.join(&name));
        }
    }
    files.iter().filter(|file| roots.iter().any(|root| file.starts_with(root))).cloned().collect()
}

/// The module names declared as `#[cfg(test)] mod name;` (file-backed, no
/// inline body) in one file's code.
fn test_only_module_declarations(code: &str) -> Vec<String> {
    let mut names = Vec::new();
    for span in cfg_attribute_spans(code) {
        if !span.test_required {
            continue;
        }
        let tail = &code[span.end..];
        let Some(semicolon) = tail.find(';') else {
            continue;
        };
        let declaration = &tail[..semicolon];
        if declaration.contains('{') {
            continue;
        }
        if let Some(name) = declaration.split_whitespace().skip_while(|word| *word != "mod").nth(1) {
            names.push(name.to_string());
        }
    }
    names
}

/// Replace every comment byte with a space, leaving string and character
/// literals — and every byte offset — untouched.
fn blank_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = bytes.to_vec();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = literal_end(bytes, index) {
            index = end;
        } else if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                out[index] = b' ';
                index += 1;
            }
        } else if bytes[index..].starts_with(b"/*") {
            index = blank_block_comment(bytes, &mut out, index);
        } else {
            index += 1;
        }
    }
    String::from_utf8(out).expect("blanking comment bytes with ASCII spaces preserves UTF-8")
}

/// Blank the (possibly nested) block comment opening at `start`,
/// returning the offset just past it.
fn blank_block_comment(bytes: &[u8], out: &mut [u8], start: usize) -> usize {
    let mut depth = 0_usize;
    let mut index = start;
    while index < bytes.len() {
        let opening = bytes[index..].starts_with(b"/*");
        let closing = bytes[index..].starts_with(b"*/");
        if opening || closing {
            depth = if opening {
                depth + 1
            } else {
                depth - 1
            };
            out[index] = b' ';
            out[index + 1] = b' ';
            index += 2;
            if depth == 0 {
                return index;
            }
        } else {
            if bytes[index] != b'\n' {
                out[index] = b' ';
            }
            index += 1;
        }
    }
    index
}

/// End offset of the literal starting at `start`, or `None` when `start`
/// does not open one — a `'` that begins a lifetime, or an `r` that is
/// just an identifier byte.
fn literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    match bytes[start] {
        b'"' => Some(quoted_literal_end(bytes, start + 1)),
        b'\'' => char_literal_end(bytes, start),
        // A raw string opens with `r` / `br` followed by hashes and a
        // quote; anything else beginning with `r` is an identifier.
        b'r' if !preceded_by_identifier(bytes, start) => raw_string_end(bytes, start),
        _ => None,
    }
}

/// End offset of a `"`-quoted literal whose body starts at `body`.
fn quoted_literal_end(bytes: &[u8], body: usize) -> usize {
    let mut cursor = body;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b'"' => return cursor + 1,
            _ => cursor += 1,
        }
    }
    bytes.len()
}

/// End offset of a raw string opening at `start`, or `None` when the `r`
/// is not a raw-string prefix after all.
fn raw_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    raw_string_span(bytes, start).map(|span| span.end)
}

/// Where a raw string's body and the literal itself end.
struct LiteralSpan {
    body: usize,
    body_end: usize,
    end: usize,
}

fn raw_string_span(bytes: &[u8], start: usize) -> Option<LiteralSpan> {
    let mut cursor = start + 1;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'"') {
        return None;
    }
    let terminator: Vec<u8> = iter::once(b'"').chain(iter::repeat_n(b'#', cursor - start - 1)).collect();
    let body = cursor + 1;
    let body_end = (body..bytes.len()).find(|offset| bytes[*offset..].starts_with(&terminator)).unwrap_or(bytes.len());
    Some(LiteralSpan { body, body_end, end: (body_end + terminator.len()).min(bytes.len()) })
}

/// Record one literal body, skipping a range an unterminated literal left
/// straddling a multi-byte character.
fn push_body<'a>(code: &'a str, body: usize, body_end: usize, literals: &mut Vec<(usize, &'a str)>) {
    if body <= body_end && code.is_char_boundary(body) && code.is_char_boundary(body_end) {
        literals.push((body, &code[body..body_end]));
    }
}

/// The string-literal bodies in `code`, each with the offset it starts
/// at. Character literals and lifetimes are skipped; a raw string yields
/// its body verbatim.
pub(super) fn string_literals(code: &str) -> Vec<(usize, &str)> {
    let bytes = code.as_bytes();
    let mut literals = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'"' => {
                let end = quoted_literal_end(bytes, cursor + 1);
                push_body(code, cursor + 1, end.saturating_sub(1), &mut literals);
                cursor = end;
            }
            b'\'' => cursor = char_literal_end(bytes, cursor).unwrap_or(cursor + 1),
            b'r' if !preceded_by_identifier(bytes, cursor) => match raw_string_span(bytes, cursor) {
                Some(span) => {
                    push_body(code, span.body, span.body_end, &mut literals);
                    cursor = span.end;
                }
                None => cursor += 1,
            },
            _ => cursor += 1,
        }
    }
    literals
}

/// End offset of a character literal, or `None` for a lifetime. A char
/// literal closes within a few bytes (`'x'`, `'\n'`, `'\u{1f600}'`); a
/// lifetime never closes at all, so the bounded search separates them.
fn char_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    const MAX_CHAR_LITERAL: usize = 12;
    let mut cursor = start + 1;
    if bytes.get(cursor) == Some(&b'\\') {
        cursor += 1;
    }
    let limit = (start + MAX_CHAR_LITERAL).min(bytes.len());
    while cursor < limit {
        if bytes[cursor] == b'\'' {
            return Some(cursor + 1);
        }
        cursor += 1;
    }
    None
}

fn preceded_by_identifier(bytes: &[u8], index: usize) -> bool {
    // `b` is the byte-string prefix (`br"…"`), not a preceding identifier.
    index > 0 && bytes[index - 1] != b'b' && (bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_')
}

/// One `#[cfg(...)]` attribute and whether its predicate makes `test` a
/// requirement for the item to exist.
struct CfgAttribute {
    /// Offset of the `#`.
    start: usize,
    /// Offset just past the closing `]`.
    end: usize,
    /// `cfg(test)` and `cfg(all(test, …))` require it; `cfg(any(test, …))`
    /// and `cfg(not(test))` do not — an `any` item also compiles into the
    /// library, so it stays non-test code and answers to the stricter
    /// check.
    test_required: bool,
}

/// Split `code` into (test-only code, everything else), each the same
/// length as the input with the other half blanked.
fn split_test_code(code: &str) -> (String, String) {
    let bytes = code.as_bytes();
    let mut test = vec![b' '; bytes.len()];
    let mut non_test = bytes.to_vec();
    for span in cfg_attribute_spans(code) {
        if !span.test_required {
            continue;
        }
        let Some(end) = braced_item_end(bytes, span.end) else {
            continue;
        };
        test[span.start..end].copy_from_slice(&bytes[span.start..end]);
        non_test[span.start..end].fill(b' ');
    }
    let recover = |buffer: Vec<u8>| String::from_utf8(buffer).expect("blanking with ASCII spaces preserves UTF-8");
    (recover(test), recover(non_test))
}

/// Every `#[cfg(...)]` attribute in `code`, in source order.
fn cfg_attribute_spans(code: &str) -> Vec<CfgAttribute> {
    const OPEN: &str = "#[cfg(";
    let mut spans = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = code[cursor..].find(OPEN) {
        let start = cursor + offset;
        let predicate_start = start + OPEN.len();
        let Some(predicate_end) = balanced_end(code.as_bytes(), predicate_start, b'(', b')') else {
            break;
        };
        let end = code[predicate_end..].find(']').map_or(code.len(), |offset| predicate_end + offset + 1);
        spans.push(CfgAttribute { start, end, test_required: requires_test(&code[predicate_start..predicate_end]) });
        cursor = predicate_end;
    }
    spans
}

/// Whether a `cfg` predicate can only hold in a test build. `any(…)` and
/// `not(…)` both break that, so their presence is disqualifying; otherwise
/// a bare `test` identifier — never one inside a string, so
/// `feature = "test-support"` does not count — makes it test-only.
fn requires_test(predicate: &str) -> bool {
    if predicate.contains("any(") || predicate.contains("not(") {
        return false;
    }
    let bytes = predicate.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(end) = literal_end(bytes, cursor) {
            cursor = end;
            continue;
        }
        let word_end = bytes.get(cursor + 4).is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_');
        if bytes[cursor..].starts_with(b"test") && !preceded_by_identifier(bytes, cursor) && word_end {
            return true;
        }
        cursor += 1;
    }
    false
}

/// End offset of the `{…}` block that follows `from`, skipping any
/// further attributes and the item's signature. `None` when a `;` closes
/// the item first (a file-backed `mod name;`) or no block follows.
fn braced_item_end(bytes: &[u8], from: usize) -> Option<usize> {
    let open = (from..bytes.len()).find(|offset| matches!(bytes[*offset], b'{' | b';'))?;
    (bytes[open] == b'{').then(|| balanced_end(bytes, open + 1, b'{', b'}')).flatten().map(|end| end + 1)
}

/// Offset of the delimiter closing the group whose body starts at `from`.
fn balanced_end(bytes: &[u8], from: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 1_usize;
    let mut cursor = from;
    while cursor < bytes.len() {
        if let Some(end) = literal_end(bytes, cursor) {
            cursor = end;
            continue;
        }
        if bytes[cursor] == open {
            depth += 1;
        } else if bytes[cursor] == close {
            depth -= 1;
            if depth == 0 {
                return Some(cursor);
            }
        }
        cursor += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{blank_comments, requires_test, split_test_code};

    #[test]
    fn comment_blanking_preserves_offsets_and_spares_literals() {
        // Tripwire: every check downstream reports a file:line computed
        // from an offset into the blanked buffer, and decides on string
        // literals the blanker must not eat. A blanker that shortens the
        // buffer or swallows a `//` inside a string sends the reader to
        // the wrong line, or drops the evidence a check needs entirely.
        let src = "let path = \"a//b\"; // comment\nlet raw = r#\"x /* y */ z\"#;\nlet tick = '\"';\n";
        let blanked = blank_comments(src);

        assert_eq!(blanked.len(), src.len(), "blanking must not move a byte");
        assert_eq!(blanked.lines().count(), src.lines().count(), "newlines survive blanking");
        assert!(blanked.contains("\"a//b\""), "a `//` inside a string literal survives");
        assert!(!blanked.contains("comment"), "a line comment is blanked");
        assert!(blanked.contains("x /* y */ z"), "a block comment inside a raw string survives");
    }

    #[test]
    fn test_only_regions_split_out_and_any_test_stays_in_the_library() {
        // Tripwire: the dist-consumer checks route a region by this
        // split — test-only code answers to "is the package classified",
        // library code answers to "is the crate a listed resolver". A
        // split that misfiles `cfg(any(test, feature))` as test-only lets
        // a library dist resolver through the weaker of the two.
        let code = blank_comments(
            "fn lib() { keep_me(); }\n\
             #[cfg(test)]\nmod t { fn a() { test_only(); } }\n\
             #[cfg(all(test, feature = \"runtime\"))]\nmod u { fn b() { also_test_only(); } }\n\
             #[cfg(any(test, feature = \"test-support\"))]\nmod v { fn c() { still_library(); } }\n",
        );
        let (test_code, non_test_code) = split_test_code(&code);

        assert!(test_code.contains("test_only") && test_code.contains("also_test_only"));
        assert!(!test_code.contains("keep_me") && !test_code.contains("still_library"));
        assert!(non_test_code.contains("keep_me") && non_test_code.contains("still_library"));
        assert!(!non_test_code.contains("test_only"));
    }

    #[test]
    fn cfg_predicates_classify_by_whether_test_is_required() {
        // Tripwire: `requires_test` decides which of the two dist-consumer
        // checks a region answers to; the `any` / string-literal cases are
        // the ones a naive `contains("test")` gets backwards.
        assert!(requires_test("test"));
        assert!(requires_test("all(test, feature = \"runtime\")"));
        assert!(!requires_test("any(test, feature = \"test-support\")"));
        assert!(!requires_test("not(test)"));
        assert!(!requires_test("feature = \"test-support\""), "`test` inside a string is not the cfg token");
    }
}
