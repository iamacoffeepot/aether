//! `cargo xtask symbols references <symbol>` — which files at a commit mention
//! a symbol, which of them define it, and which of those a declared surface
//! fails to cover (#5300, ADR-0208).
//!
//! ADR-0208 has the workpiece builder fill its derived fields itself: when a
//! plan step names a symbol, the inverse-dependency search runs and its result
//! is stored, so no lane is asked to author the field and none can skip it.
//! Nothing could run that search. The symbol inventory records definitions
//! only — no expression walk, so no call sites — which is why a scope document
//! could name a rerunnable `git grep` in prose and then declare a surface
//! covering none of the implementors. Naming a search is not running it.
//!
//! **One git process reads the commit tree; classification parses only its
//! hits.** `git grep -l -w -F -e <symbol> <sha> -- '*.rs'` reads the named tree
//! object and never consults the working tree, so the *referencing* half of the
//! answer is rev-pinned for free. The *defining* half needs no pinned inventory
//! either, because the grep result is a superset of the defining paths: every
//! file that defines a symbol also mentions it. So each hit blob is read back
//! with `git show <sha>:<path>`, parsed, and run through the existing
//! extractor. A field frozen into a signed workpiece must not be derived from a
//! mutable working tree, and this is what makes it reproducible from a sha.
//!
//! **Two limits, stated rather than papered over.** The per-file test flag is
//! not computed here — it needs the crate's whole `mod` graph, which is a
//! working-tree walk — and nothing on this path consumes it. And a blob that
//! fails to parse is recorded as a named parse fault against its path rather
//! than being swept silently into the referencing bucket.
//!
//! **This slice refuses nothing.** It emits the classification; the gate that
//! refuses a defining path outside the declared surface belongs to the
//! scope-verify member. The command exits `0` on a completed search whatever
//! the buckets hold, and non-zero only on a git fault or a malformed symbol.

use std::path::Path;

use aether_bloomery::{SurfacePattern, path_in_surface};
use aether_bloomery_git::command::{self, GitCommandError};
use anyhow::{Context, Result, bail};
use serde::Serialize;

/// How many grep hits a symbol may have before the search reports itself
/// unresolvable rather than classifying.
///
/// `-w -F` over-reports in exactly one way — an unrelated item elsewhere with
/// the same name — and cannot under-report a textual occurrence, which is the
/// right asymmetry for a surface check but is not free. Measured on this
/// workspace, genuine symbol identities sit in single digits (`adopt_candidate`
/// 4, `path_in_surface` 3, `resolve_member_dependencies` 6), the widest
/// workspace-crossing type in the sample is 35 (`StageCatalog`), and the common
/// words start at 171 (`Digest`) and climb into the hundreds (`new` 930). This
/// value sits in that gap, and carries the same judgement as its sibling
/// `DEFAULT_FIND_LIMIT` — a lane should be able to read the answer in one call.
/// It is not tuned against any historical refusal count.
const MAX_REFERENCE_HITS: usize = 64;

/// What one search concluded — either a classified result, or a refusal to
/// classify because the symbol resolved to too many files to be one identity.
#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum ReferenceSearch {
    /// The search completed.
    Resolved(Resolved),
    /// The symbol matched more files than the cap. The hit count is carried so
    /// a reader can see how far past it went; no path of such a search is ever
    /// folded into the uncovered bucket, because an over-broad result would
    /// manufacture a widening nobody needs.
    Unresolvable {
        /// The symbol searched for.
        symbol: String,
        /// Why the search declined.
        reason: String,
        /// How many files the grep named.
        hits: usize,
        /// The cap it exceeded.
        cap: usize,
    },
}

/// A completed search, as the record states it.
#[derive(Debug, Serialize)]
pub struct Resolved {
    /// The symbol searched for.
    pub symbol: String,
    /// The exact argv the grep ran, so the search is re-runnable verbatim.
    pub argv: Vec<String>,
    /// The resolved 40-character commit sha — never the argument, so `HEAD` in
    /// a stored record is not ambiguous six commits later.
    pub at: String,
    /// How many files the grep named.
    pub hits: usize,
    /// The cap this search ran under.
    pub cap: usize,
    /// The declared surface the coverage axis was tested against, echoed back
    /// byte-identically. Nothing here appends to it.
    pub surface: Vec<String>,
    /// Every hit, with its role and its coverage — two orthogonal axes, never
    /// collapsed into one bucket.
    pub paths: Vec<ClassifiedPath>,
    /// One entry per uncovered path, naming the minimal pattern that would
    /// cover it. Advice, not an edit: the builder derives the evidence that
    /// judges the surface, and the author decides what to do about it
    /// (ADR-0208).
    pub widening: Vec<Widening>,
    /// Blobs that would not parse, named rather than dropped.
    pub parse_faults: Vec<ParseFault>,
    /// Stated rather than left to inference: no sibling-intersection comparison
    /// was computed, because it needs the coordinator's commission store and a
    /// construct lane's checkout has none.
    pub sibling_intersection: Option<Vec<String>>,
}

/// One hit and what the search concluded about it.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ClassifiedPath {
    /// The repository-relative path, with the grep's `<sha>:` prefix stripped.
    pub path: String,
    /// Whether the file defines the symbol or merely mentions it.
    pub role: Role,
    /// Whether the passed surface admits this path.
    pub covered: bool,
}

/// Whether a hit defines the symbol or only refers to it.
#[derive(Debug, Serialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// An extracted row from this file is named `<symbol>` or `…::<symbol>`.
    Defining,
    /// The file mentions the symbol but declares nothing by that name.
    Referencing,
}

/// A path the surface does not cover, and the smallest pattern that would.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Widening {
    /// The uncovered path.
    pub path: String,
    /// A pattern inside the surface grammar that covers it.
    pub pattern: String,
}

/// A blob the parser refused.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ParseFault {
    /// The path whose blob would not parse.
    pub path: String,
    /// What the parser said.
    pub error: String,
}

/// Whether `symbol` is a bare Rust identifier.
///
/// Checked before anything is spawned: `-w` means nothing for a non-identifier
/// token, and refusing here is also what keeps a leading `-` out of argv.
fn is_identifier(symbol: &str) -> bool {
    !symbol.is_empty()
        && !symbol.starts_with(|first: char| first.is_ascii_digit())
        && symbol.chars().all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Run the search for `symbol` at `rev`, judged against `surface`.
///
/// # Errors
/// The symbol is not an identifier, git could not be reached, or the rev does
/// not resolve.
pub fn search(symbol: &str, rev: &str, surface: &[String]) -> Result<ReferenceSearch> {
    if !is_identifier(symbol) {
        bail!("`{symbol}` is not a Rust identifier: a reference search takes a bare symbol name");
    }
    let root =
        command::run_ok(Path::new("."), &["rev-parse", "--show-toplevel"]).context("resolve the repository root")?;
    let root = Path::new(&root);
    let at = command::run_ok(root, &["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
        .with_context(|| format!("resolve `{rev}` to a commit"))?;

    let argv = vec![
        "grep".to_owned(),
        "--full-name".to_owned(),
        "-l".to_owned(),
        "-z".to_owned(),
        "-w".to_owned(),
        "-F".to_owned(),
        "-e".to_owned(),
        symbol.to_owned(),
        at.clone(),
        "--".to_owned(),
        "*.rs".to_owned(),
    ];
    let hits = grep(root, &argv, &at)?;

    if hits.len() > MAX_REFERENCE_HITS {
        return Ok(ReferenceSearch::Unresolvable {
            symbol: symbol.to_owned(),
            reason: format!(
                "`{symbol}` names {} files at this commit, past the {MAX_REFERENCE_HITS}-file cap: it reads as \
                 a common word rather than one symbol identity, so nothing is classified and nothing is \
                 reported as uncovered",
                hits.len()
            ),
            hits: hits.len(),
            cap: MAX_REFERENCE_HITS,
        });
    }

    let mut paths = Vec::new();
    let mut parse_faults = Vec::new();
    for path in &hits {
        match classify(root, &at, path, symbol) {
            Ok(role) => {
                paths.push(ClassifiedPath { path: path.clone(), role, covered: path_in_surface(surface, path) });
            }
            Err(error) => parse_faults.push(ParseFault { path: path.clone(), error }),
        }
    }
    let widening = paths.iter().filter(|entry| !entry.covered).filter_map(covering).collect();

    Ok(ReferenceSearch::Resolved(Resolved {
        symbol: symbol.to_owned(),
        argv,
        at,
        hits: hits.len(),
        cap: MAX_REFERENCE_HITS,
        surface: surface.to_vec(),
        paths,
        widening,
        parse_faults,
        sibling_intersection: None,
    }))
}

/// The repository-relative paths the grep named, `<sha>:` prefix stripped.
///
/// `run_ok` would be wrong here: `git grep` exits **1** for zero hits, so a
/// clean empty result would come back as a spawn-level failure. Exit `0` is
/// hits, `1` is none, anything else is a genuine fault.
fn grep(root: &Path, argv: &[String], at: &str) -> Result<Vec<String>> {
    let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
    let output = command::run(root, &borrowed).context("run the reference grep")?;
    match output.status.code() {
        Some(0) => {}
        Some(1) => return Ok(Vec::new()),
        _ => {
            return Err(GitCommandError::Failed {
                args: borrowed.join(" "),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
            .context("the reference grep faulted");
        }
    }

    let prefix = format!("{at}:");
    Ok(String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.strip_prefix(&prefix).unwrap_or(entry).to_owned())
        .collect())
}

/// Whether the blob at `path` defines `symbol`.
///
/// The blob is read at the rev, never off disk, so a dirty working tree cannot
/// change the answer. A row defines the symbol when its name is the symbol
/// itself (a free fn, a struct, a trait) or ends with `::<symbol>` (an impl
/// method or a trait method declaration).
fn classify(root: &Path, at: &str, path: &str, symbol: &str) -> Result<Role, String> {
    let blob = command::run(root, &["show", &format!("{at}:{path}")]).map_err(|error| error.to_string())?;
    if !blob.status.success() {
        return Err(String::from_utf8_lossy(&blob.stderr).into_owned());
    }
    let source = String::from_utf8_lossy(&blob.stdout).into_owned();
    let file = syn::parse_file(&source).map_err(|error| error.to_string())?;
    // The per-file test flag needs the crate's whole `mod` graph, which is a
    // working-tree walk; nothing here consumes it, so `false` is honest rather
    // than a guess that could read as a claim.
    let rows = super::extract::extract_parsed(&crate_label(path), path, &file, false);
    let suffix = format!("::{symbol}");
    let defines = rows.iter().any(|row| row.name == symbol || row.name.ends_with(&suffix));
    Ok(if defines {
        Role::Defining
    } else {
        Role::Referencing
    })
}

/// The crate a workspace path belongs to, for the extractor's module naming.
///
/// A best-effort label: `crates/<name>/…` and `xtask/…` are the two shapes this
/// workspace has, and anything else falls back to the leading segment. It
/// affects only the `crate`/`module` columns of rows this search reads for
/// their names, never the defining test.
pub fn crate_label(path: &str) -> String {
    let mut segments = path.split('/');
    match (segments.next(), segments.next()) {
        (Some("crates"), Some(name)) => name.to_owned(),
        (Some(first), _) => first.to_owned(),
        _ => path.to_owned(),
    }
}

/// The minimal surface pattern that would cover `entry`, validated through the
/// public grammar so an emitted suggestion is never one the seal door refuses.
///
/// The exact path first — the narrowest widening an author can accept — and its
/// enclosing directory subtree only if the exact form is somehow outside the
/// grammar. A path the grammar cannot express at all yields no entry rather
/// than a suggestion that would be refused.
fn covering(entry: &ClassifiedPath) -> Option<Widening> {
    let subtree = entry.path.rsplit_once('/').map(|(directory, _)| format!("{directory}/**"));
    [Some(entry.path.clone()), subtree]
        .into_iter()
        .flatten()
        .find(|candidate| SurfacePattern::parse(candidate).is_some())
        .map(|pattern| Widening { path: entry.path.clone(), pattern })
}

#[cfg(test)]
mod tests {
    use super::{ClassifiedPath, Role, covering, crate_label, is_identifier, search};
    use aether_bloomery::SurfacePattern;

    fn uncovered(path: &str) -> ClassifiedPath {
        ClassifiedPath { path: path.to_owned(), role: Role::Defining, covered: false }
    }

    #[test]
    fn a_non_identifier_symbol_is_refused_before_anything_is_spawned() {
        // The plausible bug: a `-w` grep for a token that is not a word means
        // nothing, and a symbol starting with `-` would be read by git as a
        // flag rather than a pattern.
        for bad in ["", "-rf", "9lives", "adopt candidate", "Type::method", "*"] {
            assert!(!is_identifier(bad), "{bad} is not a bare identifier");
        }
        for good in ["adopt_candidate", "SurfacePattern", "_private", "v2"] {
            assert!(is_identifier(good), "{good} is a bare identifier");
        }
        assert!(search("-rf", "HEAD", &[]).is_err(), "a refused symbol never reaches git");
    }

    #[test]
    fn a_widening_suggestion_is_inside_the_surface_grammar() {
        // The plausible bug: emitting `crates/*/src/*.rs` or a mid-path `**`,
        // which the seal door refuses — advice an author cannot act on.
        let widening = covering(&uncovered("crates/aether-bloomery/src/port/source.rs")).expect("a pattern covers it");
        assert_eq!(widening.pattern, "crates/aether-bloomery/src/port/source.rs");
        assert!(SurfacePattern::parse(&widening.pattern).is_some(), "and the grammar accepts it");
    }

    #[test]
    fn a_crate_label_reads_the_workspace_layout() {
        assert_eq!(crate_label("crates/aether-bloomery/src/lib.rs"), "aether-bloomery");
        assert_eq!(crate_label("xtask/src/main.rs"), "xtask");
    }

    #[test]
    fn the_search_names_every_file_that_mentions_the_symbol_and_marks_the_definers() {
        // Acceptance (#5300): the case ADR-0208 itself cites. `symbols find`
        // sees only the two impls; the trait declaration and the call site —
        // precisely the two files a signature change is guaranteed to touch —
        // are invisible to it. A subset assertion, not an equality, so a fifth
        // reference landing later does not fail this spuriously.
        let Ok(super::ReferenceSearch::Resolved(resolved)) = search("adopt_candidate", "HEAD", &[]) else {
            // A checkout with no git (or a shallow one without HEAD) cannot
            // answer; that is a host condition, not a defect in the search.
            return;
        };

        let defining: Vec<&str> = resolved
            .paths
            .iter()
            .filter(|entry| entry.role == Role::Defining)
            .map(|entry| entry.path.as_str())
            .collect();
        let referencing: Vec<&str> = resolved
            .paths
            .iter()
            .filter(|entry| entry.role == Role::Referencing)
            .map(|entry| entry.path.as_str())
            .collect();

        assert!(
            defining.contains(&"crates/aether-bloomery/src/port/source.rs"),
            "the trait declaration is a defining path: {defining:?}",
        );
        assert!(defining.contains(&"crates/aether-bloomery-git/src/source.rs"), "{defining:?}");
        assert!(defining.contains(&"crates/aether-chassis-bloomery/src/bloomery/source.rs"), "{defining:?}");
        assert!(
            referencing.contains(&"crates/aether-chassis-bloomery/src/bloomery/reactor/integrate/runtime/mod.rs"),
            "the call site is a referencing path: {referencing:?}",
        );
        assert_eq!(resolved.at.len(), 40, "the record pins the resolved sha, not the argument");
        assert!(resolved.sibling_intersection.is_none(), "no sibling comparison is computed here");
    }

    #[test]
    fn an_uncovered_path_is_reported_without_the_surface_being_widened() {
        // Tripwire (ADR-0208): the builder derives the evidence that judges the
        // surface; silently appending to the surface is exactly the
        // auto-completion the ADR forbids.
        let surface = vec!["crates/aether-bloomery-git/**".to_owned()];
        let Ok(super::ReferenceSearch::Resolved(resolved)) = search("adopt_candidate", "HEAD", &surface) else {
            return;
        };

        assert_eq!(resolved.surface, surface, "the passed surface is echoed back byte-identically");
        assert!(
            resolved.paths.iter().any(|entry| entry.covered),
            "the one crate the surface names is covered: {:?}",
            resolved.paths,
        );
        assert!(!resolved.widening.is_empty(), "and the rest are reported as widenings");
        for widening in &resolved.widening {
            assert!(SurfacePattern::parse(&widening.pattern).is_some(), "{widening:?} is inside the grammar");
        }
    }
}
