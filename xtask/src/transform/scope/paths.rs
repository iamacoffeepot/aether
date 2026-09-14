//! Which tokens in a plan step are repository paths.
//!
//! A plan step is prose, and prose uses slashes: "size/model routing",
//! "wedge/park", "Debug/hex". Reading every slashed token as a path puts those
//! words in the refusing population, the verifier reports them as unresolvable,
//! and the run fails on nonsense (measured on a live scope run, 2026-09-13).
//!
//! So the tree at the subject revision decides. A token's first segment has to
//! be something that is actually there, and a bare token — one the author did
//! not mark as code — has to be a top-level *directory* or carry an extension
//! the tree uses. The test is the entry a path starts at rather than the path
//! itself, so a file the plan will create still reads as a path.

use std::collections::BTreeSet;
use std::path::Path;

use aether_bloomery_git::command;
use anyhow::{Context, Result};

/// What the subject tree offers a path-shaped token: its top-level entries, the
/// subset of those that are directories, and the file extensions it uses.
pub(super) struct TreeIndex {
    /// Every top-level entry name — directories and root files alike.
    entries: BTreeSet<String>,
    /// The top-level entries that are directories.
    directories: BTreeSet<String>,
    /// Every file extension present in the tree.
    extensions: BTreeSet<String>,
}

/// Index the tree at `rev`.
///
/// # Errors
/// Git could not be reached, or the rev does not resolve.
pub(super) fn index(rev: &str) -> Result<TreeIndex> {
    let root =
        command::run_ok(Path::new("."), &["rev-parse", "--show-toplevel"]).context("resolve the repository root")?;
    Ok(TreeIndex::from_listing(
        &command::run_ok(Path::new(&root), &["ls-tree", "-r", "--name-only", rev])
            .with_context(|| format!("list the tree at `{rev}`"))?,
    ))
}

impl TreeIndex {
    /// Read one `git ls-tree -r --name-only` listing. Recursive, so a top-level
    /// directory appears only as the first segment of a path and a root file as
    /// a segmentless line — which is exactly the distinction the bare-token
    /// rule needs.
    fn from_listing(listing: &str) -> Self {
        let mut index = Self { entries: BTreeSet::new(), directories: BTreeSet::new(), extensions: BTreeSet::new() };
        for path in listing.lines().map(str::trim).filter(|line| !line.is_empty()) {
            match path.split_once('/') {
                Some((first, _)) => {
                    index.entries.insert(String::from(first));
                    index.directories.insert(String::from(first));
                }
                None => {
                    index.entries.insert(String::from(path));
                }
            }
            if let Some(extension) = Path::new(path).extension().and_then(|extension| extension.to_str()) {
                index.extensions.insert(String::from(extension));
            }
        }
        index
    }

    /// Whether `path` reads as a repository path rather than prose.
    ///
    /// A backticked span was marked as code deliberately, so it need only start
    /// at a real top-level entry. A bare token has to earn it: its first
    /// segment is a top-level directory, or it ends in an extension the tree
    /// uses.
    fn admits(&self, path: &str, quoted: Quoted) -> bool {
        let first = path.split('/').next().unwrap_or(path);
        match quoted {
            Quoted::Yes => self.entries.contains(first),
            Quoted::No => self.directories.contains(first) || self.uses_extension(path),
        }
    }

    fn uses_extension(&self, path: &str) -> bool {
        Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| self.extensions.contains(extension))
    }
}

/// Whether the author marked the token as code. Backticks are the one signal of
/// intent available in a plan step, and they buy the looser rule.
#[derive(Clone, Copy)]
enum Quoted {
    Yes,
    No,
}

/// The repository paths `text` names, in first-mention order, deduplicated.
pub(super) fn extract_paths(text: &str, tree: &TreeIndex) -> Vec<String> {
    let quoted = backtick_spans(text).into_iter().map(|span| (span, Quoted::Yes));
    let bare = bare_tokens(text).into_iter().map(|token| (token, Quoted::No));

    let mut paths: Vec<String> = Vec::new();
    for (candidate, quoted) in quoted.chain(bare) {
        let Some(path) = as_repo_path(candidate).filter(|path| tree.admits(path, quoted)) else {
            continue;
        };
        if !paths.iter().any(|existing| existing == &path) {
            paths.push(path);
        }
    }
    paths
}

/// The path shape a token has to have before the tree is consulted at all.
fn as_repo_path(token: &str) -> Option<String> {
    let token = token.trim();
    let token = token.strip_suffix("(create)").map_or(token, str::trim);
    let token = token.trim_end_matches(['.', ',', ';', ':', ')']);
    if token.contains("://") || token.starts_with('/') || token.contains('\\') {
        return None;
    }
    if !token.contains('/') {
        return None;
    }
    if token.chars().any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '*'))) {
        return None;
    }
    Some(token.to_owned())
}

pub(super) fn backtick_spans(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        spans.push(&rest[..end]);
        rest = &rest[end + 1..];
    }
    spans
}

fn bare_tokens(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '*')))
        .filter(|token| !token.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{TreeIndex, extract_paths};

    fn tree() -> TreeIndex {
        TreeIndex::from_listing(
            "Cargo.toml\ncrates/aether-bloomery/src/reduce/seal.rs\ndocs/adr/0208-scope.md\nxtask/src/bloom/mod.rs\n",
        )
    }

    #[test]
    fn prose_that_happens_to_carry_a_slash_is_not_a_path() {
        // The measured class (2026-09-13, a live Grok scope run): "size/model",
        // "wedge/park" and "Debug/hex" entered the refusing population as
        // paths, the verifier reported them unresolvable, and the run failed on
        // words. Nothing in the tree starts at any of those segments.
        let step = "Carry the size/model routing through the wedge/park transition, logging the Debug/hex form and \
                    the surface/crates/reads split.";
        assert!(extract_paths(step, &tree()).is_empty(), "{:?}", extract_paths(step, &tree()));
    }

    #[test]
    fn a_real_path_still_resolves_bare_or_backticked() {
        // The other half: tightening must not stop reading the paths a step
        // genuinely names, whether the author marked them as code or not.
        let bare = "Pin the seal reducer in crates/aether-bloomery/src/reduce/seal.rs and the client in \
                    xtask/src/bloom/mod.rs.";
        assert_eq!(
            extract_paths(bare, &tree()),
            vec!["crates/aether-bloomery/src/reduce/seal.rs", "xtask/src/bloom/mod.rs"],
        );

        let quoted = "Pin `crates/aether-bloomery/src/reduce/seal.rs` beside `xtask/src/bloom/mod.rs`.";
        assert_eq!(
            extract_paths(quoted, &tree()),
            vec!["crates/aether-bloomery/src/reduce/seal.rs", "xtask/src/bloom/mod.rs"],
        );
    }

    #[test]
    fn a_file_the_step_will_create_reads_as_a_path() {
        // The test is the top-level entry a path starts at, never the path
        // itself: a new file under an existing directory is still a path, or a
        // step could not name what it adds.
        let step = "Add xtask/src/transform/scope/paths.rs (create) for the token rule.";
        assert_eq!(extract_paths(step, &tree()), vec!["xtask/src/transform/scope/paths.rs"]);
    }
}
