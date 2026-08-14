//! What a finding names: the surface a repair lap has to touch.
//!
//! Deliberately shallow. The extraction reads a finding the way a person
//! skimming it does — the things it put in backticks, and the file paths it
//! spelled out — and nothing cleverer, because the whole point of the host-side
//! triage is that it is decidable without a model. Every rule below is stated
//! once here and exercised in the sibling tests; a finding whose vocabulary the
//! rules do not reach names nothing, and naming nothing passes.

/// Characters trimmed off both ends of a token before it is classified — the
/// prose punctuation a finding wraps a name in. The underscore is deliberately
/// absent: a leading `_` is part of a Rust identifier, not decoration.
const TRIM: &[char] = &['`', '\'', '"', '(', ')', ',', ';', ':', '.', '!', '?', '[', ']', '{', '}', '<', '>', '*'];

/// Rust keywords that reach the identifier shape and are never the thing a
/// finding is about. Short on purpose: the list exists to drop `` `fn` `` and
/// `` `mut` ``, not to police vocabulary.
const KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "pub", "use", "mod", "impl", "for", "match", "struct", "enum", "trait", "const", "static",
    "self", "super", "crate", "where", "while", "loop", "else", "move", "dyn", "ref", "type", "unsafe", "return",
];

/// The file extensions a path-shaped token may end in.
///
/// Enumerated rather than inferred from the token's shape, because "a dot with
/// letters after it" is also what `ctx.actor`, `self.field`, and `e.g.` look
/// like — and a phantom path is worse than a missed one: it is a name no diff
/// can ever match, which turns an advisory-strict check into a bounce
/// manufactured out of prose. A finding naming a file this list does not cover
/// names one fewer thing, and naming fewer things passes more laps.
const EXTENSIONS: &[&str] = &[
    "rs", "toml", "md", "yml", "yaml", "json", "lock", "sh", "py", "wgsl", "html", "css", "js", "ts", "txt", "sql",
    "dsl", "obj", "sfz", "wav", "ttf", "png", "svg", "wasm", "ron", "ini", "cfg",
];

/// The shortest identifier the extraction will treat as a named symbol. Two
/// characters is the ambiguous zone — `id`, `to`, `of` read as prose as often as
/// they read as code — and a finding about a two-character symbol still names
/// the file it lives in.
const MIN_SYMBOL_LEN: usize = 3;

/// What a finding names, split by how a diff is checked against it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct NamedSurface {
    /// Backtick-quoted identifiers — `representative`, `reduce_attempt_completed`.
    /// Checked against what the repair *changed*.
    pub symbols: Vec<String>,
    /// Path-shaped tokens — `golden_decisions.rs`, `crates/aether-bloomery/src/reduce/decision.rs`.
    /// Checked against which files the repair changed.
    pub paths: Vec<String>,
}

impl NamedSurface {
    /// Everything named, symbols first, for a message that has to say what the
    /// lap was expected to touch.
    #[must_use]
    pub fn named(&self) -> Vec<String> {
        self.symbols.iter().chain(self.paths.iter()).cloned().collect()
    }
}

/// The surface the **named checks** of `finding`'s mechanical findings describe
/// (#4961), or an empty surface when it states none.
///
/// A mechanical finding names the check that would have caught it, and the
/// format requires that check to be spelled as the symbol or path the repair
/// adds or changes — so it arrives here already in the shape the extraction
/// reads, and the rules are the ones below rather than a second set. What the
/// caller does with it is the one thing that differs: this surface is what a
/// repair *must* contain, where the finding's own surface is only what it may
/// contain.
///
/// The classification is the domain crate's, so the lane that authors the format
/// and the triage that enforces it read it the same way.
#[must_use]
pub fn named_check_surface(finding: &str) -> NamedSurface {
    let mut surface = NamedSurface::default();
    for check in aether_bloomery::classify_findings(finding).named_checks() {
        let named = named_surface(check);
        for symbol in named.symbols {
            push_unique(&mut surface.symbols, &symbol);
        }
        for path in named.paths {
            push_unique(&mut surface.paths, &path);
        }
    }
    surface
}

/// The surface `finding` names.
///
/// Two rules, and only two.
///
/// - **A backtick span that reads as an identifier is a symbol.**
///   `` `representative()` `` and `` `Decision::RecordEvidence` `` do; a
///   backticked code line does not, because whitespace is not an identifier
///   character — a quotation names nothing. The span is trimmed of prose
///   punctuation first, and an identifier takes its last `::` segment: a finding
///   naming `Decision::RecordEvidence` is about `RecordEvidence`.
/// - **A path-shaped token is a path**, inside backticks or bare, because
///   findings spell paths out either way. Path-shaped means: nothing but
///   `[A-Za-z0-9._/-]`, ending in one of the stated [`EXTENSIONS`].
///
/// Generous by design. A false bounce costs the workpiece a lap; a false pass
/// costs a judge round, so where the rules are unsure they extract *more*, and
/// more names mean more ways for a repair to prove itself on-task.
#[must_use]
pub fn named_surface(finding: &str) -> NamedSurface {
    let mut surface = NamedSurface::default();
    for span in backtick_spans(finding) {
        classify(span, &mut surface);
    }
    for token in finding.split_whitespace() {
        let trimmed = strip_line_column(token.trim_matches(TRIM));
        if path_shaped(trimmed) {
            push_unique(&mut surface.paths, trimmed);
        }
    }
    surface
}

/// `token` with a trailing `:line` / `:line:column` suffix removed — the shape
/// every compiler and linter diagnostic spells a location in, and the most
/// common way a finding names a file at all.
fn strip_line_column(token: &str) -> &str {
    let mut token = token;
    for _ in 0..2 {
        if let Some((head, tail)) = token.rsplit_once(':')
            && !tail.is_empty()
            && tail.chars().all(|c| c.is_ascii_digit())
        {
            token = head;
        }
    }
    token
}

/// File a trimmed backtick span into whichever half of the surface it belongs to.
fn classify(span: &str, surface: &mut NamedSurface) {
    let trimmed = strip_line_column(span.trim_matches(TRIM));
    if trimmed.is_empty() {
        return;
    }
    if path_shaped(trimmed) {
        push_unique(&mut surface.paths, trimmed);
        return;
    }
    let leaf = trimmed.rsplit("::").next().unwrap_or(trimmed);
    if is_symbol(leaf) {
        push_unique(&mut surface.symbols, leaf);
    }
}

/// The text between backtick pairs, in order. An unterminated trailing span is
/// dropped: it is prose the writer never closed, not a name.
fn backtick_spans(finding: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = finding;
    while let Some((_, after_open)) = rest.split_once('`') {
        let Some((span, after_close)) = after_open.split_once('`') else {
            break;
        };
        spans.push(span);
        rest = after_close;
    }
    spans
}

/// Whether `token` reads as a Rust identifier a finding would name.
fn is_symbol(token: &str) -> bool {
    token.len() >= MIN_SYMBOL_LEN
        && !KEYWORDS.contains(&token)
        && token.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `token` reads as a file path a finding would name: path characters
/// only, ending in one of the [`EXTENSIONS`].
pub(super) fn path_shaped(token: &str) -> bool {
    token.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
        && token.rsplit_once('.').is_some_and(|(stem, extension)| !stem.is_empty() && EXTENSIONS.contains(&extension))
}

/// Append `value` unless it is already there — the surface is a set the caller
/// reads in first-named order, not a bag.
fn push_unique(into: &mut Vec<String>, value: &str) {
    if !into.iter().any(|held| held == value) {
        into.push(value.to_owned());
    }
}
