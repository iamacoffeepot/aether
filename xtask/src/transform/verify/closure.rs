//! The candidate's reverse-dependency closure: which workspace packages its
//! diff can possibly have broken (#4895).
//!
//! A failing test binary that links nothing the diff touched cannot be the
//! diff's fault. That is the whole discrimination signal the verify umbrella
//! reads a failing `verify.test` run against: a failure inside the closure is a
//! finding about the candidate, and a failure outside it is a statement about
//! the host the run happened on.
//!
//! The graph analysis is not this module's — `cargo xtask affected` already
//! maps changed paths onto the workspace package graph and takes the reverse
//! closure for CI's test selection, screening graph-shaping paths to "the whole
//! workspace is affected" before any analysis runs. This module names the diff
//! and asks that question; a second walk would be a second, quietly divergent
//! answer to it.
//!
//! Everything here fails towards the candidate. An unresolvable diff, an
//! unreadable graph, a change whose blast radius the graph cannot bound, and a
//! diff that comes back empty all yield no closure at all, and a run with no
//! closure classifies nothing out — exactly the behaviour this module did not
//! exist for. The forgiving direction is the dangerous one: a defect excused as
//! weather is a defect that reaches `main`.

use std::collections::BTreeSet;
use std::process::Command;

use crate::affected::reverse_dependency_closure;
use crate::cargo::run_captured;

/// How many package names an evidence rendering of the closure prints before it
/// says how many more there are. The reader needs to recognize the closure, not
/// to enumerate a hundred-crate workspace.
const MAX_NAMED_PACKAGES: usize = 8;

/// The workspace packages the candidate's diff can have broken.
pub(super) struct Closure {
    packages: BTreeSet<String>,
}

impl Closure {
    /// Whether `package` is inside the closure — the question a failing test's
    /// own crate is put to.
    pub(super) fn contains(&self, package: &str) -> bool {
        self.packages.contains(package)
    }

    /// The closure as the evidence states it, so a reader can check a
    /// classification rather than take it on trust.
    pub(super) fn describe(&self) -> String {
        let named: Vec<&str> = self.packages.iter().take(MAX_NAMED_PACKAGES).map(String::as_str).collect();
        let omitted = self.packages.len() - named.len();
        let list = named.join(", ");
        if omitted == 0 {
            return format!("{} package(s): {list}", self.packages.len());
        }
        format!("{} package(s): {list}, and {omitted} more", self.packages.len())
    }
}

#[cfg(test)]
impl Closure {
    /// A closure over exactly these packages, for exercising the umbrella's
    /// discrimination without a git repository or a package graph.
    pub(super) fn of(packages: &[&str]) -> Self {
        Self { packages: packages.iter().map(|package| (*package).to_owned()).collect() }
    }
}

/// The closure for the candidate this run verifies, or `None` when no bounded
/// one could be computed.
///
/// `diff_base` is the work order's own, when it named one. Absent, the diff is
/// taken against the merge base with `origin/main` — the same base
/// `cargo xtask affected` and `scripts/check-suppressions.py` default to, so
/// "the candidate's diff" means one thing across the lane, the gate, and CI's
/// test selection. The working tree is unioned in either way: a member lane
/// runs under the working-tree contract, where the candidate's change is not
/// committed at all, and a union can only widen the closure.
pub(super) fn resolve(diff_base: Option<&str>) -> Option<Closure> {
    let mut changed: BTreeSet<String> = committed_paths(diff_base).into_iter().collect();
    changed.extend(working_tree_paths());

    let packages = reverse_dependency_closure(&changed.into_iter().collect::<Vec<String>>()).ok().flatten()?;
    Some(Closure { packages })
}

/// The paths the candidate's committed range names, or none when the range
/// cannot be resolved.
fn committed_paths(diff_base: Option<&str>) -> Vec<String> {
    let Some(base) = diff_base.map(str::to_owned).or_else(|| git(&["merge-base", "origin/main", "HEAD"])) else {
        return Vec::new();
    };

    git(&["diff", "--name-only", "--no-renames", "-z", base.trim(), "HEAD"]).as_deref().map(paths).unwrap_or_default()
}

/// The paths the working tree differs from `HEAD` on, tracked and untracked.
///
/// `--no-renames` is what keeps a file moved between crates attributed to both
/// of them: rename detection reports only the destination, and the crate the
/// file left is exactly the one whose build the move can have broken.
fn working_tree_paths() -> Vec<String> {
    let tracked = git(&["diff", "--name-only", "--no-renames", "-z", "HEAD"]).as_deref().map(paths).unwrap_or_default();
    let untracked =
        git(&["ls-files", "--others", "--exclude-standard", "-z"]).as_deref().map(paths).unwrap_or_default();
    [tracked, untracked].concat()
}

/// Run one read-only git query, or `None` for anything short of a clean success
/// — a git that is not there, a repository it will not read, output that is not
/// UTF-8. Every one of those means the diff was not seen, which the caller
/// reads as an unbounded closure.
fn git(args: &[&str]) -> Option<String> {
    let mut query = Command::new("git");
    query.args(args);

    let output = run_captured(query).ok()?;
    output.status.success().then(|| String::from_utf8(output.stdout).ok()).flatten()
}

/// Split a `-z` path list. NUL-separated output is what makes this a split
/// rather than a parse: git quotes and escapes unusual bytes in every other
/// format, so a path with a space in it would arrive as something no package's
/// directory prefix matches.
fn paths(stdout: &str) -> Vec<String> {
    stdout.split('\0').filter(|path| !path.is_empty()).map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::{Closure, paths};

    #[test]
    fn a_nul_separated_list_splits_on_the_separator_rather_than_on_whitespace() {
        // Tripwire: `-z` is what makes a path with a space in it survive, and a
        // split on lines or whitespace would silently truncate one into a path
        // no package owns — which reads as "this change touched something
        // outside every crate" and unbounds the closure for the whole run.
        let split = paths("crates/aether-render/src/lib.rs\0docs/some note.md\0");

        assert_eq!(split, ["crates/aether-render/src/lib.rs", "docs/some note.md"]);
        assert!(paths("").is_empty(), "an empty diff names no paths");
    }

    #[test]
    fn a_large_closure_is_named_in_part_and_says_how_much_it_left_out() {
        // The rendering is a receipt: an operator reading "classified out" has
        // to be able to check the closure it was classified against. A silently
        // truncated list would make a wrong closure look like a right one.
        let closure = Closure::of(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);

        let described = closure.describe();

        assert!(described.starts_with("10 package(s): a, b, c, d, e, f, g, h,"), "got: {described}");
        assert!(described.ends_with("and 2 more"), "got: {described}");
        assert_eq!(Closure::of(&["a", "b"]).describe(), "2 package(s): a, b");
    }
}
