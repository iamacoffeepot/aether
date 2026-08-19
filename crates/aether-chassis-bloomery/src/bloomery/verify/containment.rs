//! Member-Verify declared-surface containment.
//!
//! A candidate that edits a path no declared-surface glob covers must fail
//! Verify with that path named. The check is a set-membership test over the
//! globs sealed at admission: no new stage, no new wire identity. `Cargo.lock`
//! is structurally shared and machine-maintained, so a dependency-graph-neutral
//! rebuild that touches it is not a violation.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::Path;
use std::process::Command;

use aether_bloomery::{StageVerdict, SurfacePattern, VerifyFailure, VerifyFailureSet};

/// The workspace lockfile any member's rebuild may rewrite.
const LOCKFILE: &str = "Cargo.lock";

/// Paths in `changed` that sit outside every glob in `surface`.
///
/// `Cargo.lock` is skipped. Globs outside the surface grammar are ignored
/// rather than treated as covering anything — the same fail-closed parse the
/// seal door already applies.
#[must_use]
pub fn out_of_surface<'a>(changed: impl IntoIterator<Item = &'a str>, surface: &[String]) -> Vec<String> {
    let mut violations: Vec<String> = changed
        .into_iter()
        .filter(|path| *path != LOCKFILE && !path_in_surface(surface, path))
        .map(str::to_owned)
        .collect();
    violations.sort();
    violations.dedup();
    violations
}

/// Whether `path` matches any declared-surface glob.
#[must_use]
pub fn path_in_surface(surface: &[String], path: &str) -> bool {
    surface.iter().filter_map(|glob| SurfacePattern::parse(glob)).any(|pattern| match pattern {
        SurfacePattern::Exact(exact) => path == exact,
        SurfacePattern::Subtree(prefix) => {
            path == prefix || path.starts_with(&prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')
        }
    })
}

/// The repository-relative paths `base..HEAD` changed.
///
/// Same read the mechanical umbrella uses to scope a member Verify, so the
/// containment set and the compile-closure set cannot drift onto different
/// diffs.
pub fn changed_paths(repo: &Path, base: &str) -> Result<Vec<String>, ChangedPathsError> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["diff", "--name-only", "--no-ext-diff", "-z", base, "HEAD"])
        .output()
        .map_err(ChangedPathsError::Spawn)?;
    if !output.status.success() {
        return Err(ChangedPathsError::Git(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
    }
    String::from_utf8(output.stdout).map_or(Err(ChangedPathsError::Encoding), |stdout| {
        Ok(stdout.split('\0').filter(|path| !path.is_empty()).map(str::to_owned).collect())
    })
}

/// Why [`changed_paths`] could not read the candidate diff.
#[derive(Debug)]
pub enum ChangedPathsError {
    /// `git` itself would not start.
    Spawn(io::Error),
    /// `git diff` exited non-zero.
    Git(String),
    /// The name-only listing was not UTF-8.
    Encoding,
}

impl fmt::Display for ChangedPathsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "spawn git diff: {error}"),
            Self::Git(stderr) if stderr.is_empty() => write!(f, "git diff failed"),
            Self::Git(stderr) => write!(f, "git diff failed: {stderr}"),
            Self::Encoding => write!(f, "git diff produced non-UTF-8 output"),
        }
    }
}

impl Error for ChangedPathsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::Git(_) | Self::Encoding => None,
        }
    }
}

/// The verdict, typed set, and findings after the containment gate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ContainmentOverlay {
    /// The Verify verdict, failed when any path sat outside the surface.
    pub verdict: StageVerdict,
    /// The typed set the reducer accounts. Unchanged when containment holds.
    pub failed_verifiers: VerifyFailureSet,
    /// Findings naming every violating path, or the mechanical findings when
    /// containment holds.
    pub findings: Option<String>,
}

/// Overlay containment onto a member-Verify result.
///
/// An empty `violations` is a no-op. A nonempty list fails the verdict, names
/// every path, and — when the mechanical umbrella named nothing — stamps
/// [`VerifyFailure::Test`] so the reducer enters Refine rather than treating
/// an empty set as an unjudged re-run. A ninth identity would need a wider
/// mask; this reuses the "candidate is wrong" class and leaves the paths as
/// the named failure.
#[must_use]
pub fn apply_containment(
    verdict: StageVerdict,
    failed_verifiers: VerifyFailureSet,
    findings: Option<String>,
    violations: &[String],
) -> ContainmentOverlay {
    if violations.is_empty() {
        return ContainmentOverlay { verdict, failed_verifiers, findings };
    }

    let named = containment_findings(violations);
    let findings = Some(match findings {
        Some(existing) if !existing.is_empty() => format!("{named}\n\n{existing}"),
        _ => named,
    });
    let failed_verifiers = if failed_verifiers.is_empty() {
        VerifyFailureSet::one(VerifyFailure::Test)
    } else {
        failed_verifiers
    };
    ContainmentOverlay { verdict: StageVerdict::VerificationFailed, failed_verifiers, findings }
}

fn containment_findings(violations: &[String]) -> String {
    let mut findings = String::from("Candidate edits outside the declared surface:\n");
    for path in violations {
        findings.push_str("\n- ");
        findings.push_str(path);
    }
    findings
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use aether_bloomery::{StageVerdict, VerifyFailure, VerifyFailureSet};
    use tempfile::TempDir;

    use super::{apply_containment, changed_paths, containment_findings, out_of_surface};

    #[test]
    fn an_in_surface_path_is_not_a_violation() {
        assert!(
            out_of_surface(["crates/owned/src/lib.rs"], &surface(&["crates/owned/**"])).is_empty(),
            "a path the surface covers must pass"
        );
    }

    #[test]
    fn an_out_of_surface_path_is_named() {
        assert_eq!(
            out_of_surface(["crates/other/src/lib.rs"], &surface(&["crates/owned/**"])),
            ["crates/other/src/lib.rs"],
        );
    }

    #[test]
    fn cargo_lock_is_exempt() {
        assert!(
            out_of_surface(["Cargo.lock", "crates/owned/src/lib.rs"], &surface(&["crates/owned/**"])).is_empty(),
            "the shared lockfile is not a surface violation"
        );
    }

    #[test]
    fn every_violating_path_is_named_and_sorted() {
        assert_eq!(
            out_of_surface(
                ["crates/z/src/lib.rs", "crates/a/src/lib.rs", "crates/owned/src/lib.rs"],
                &surface(&["crates/owned/**"]),
            ),
            ["crates/a/src/lib.rs", "crates/z/src/lib.rs"],
        );
    }

    #[test]
    fn an_exact_glob_does_not_cover_a_sibling() {
        assert_eq!(
            out_of_surface(["crates/owned/src/other.rs"], &surface(&["crates/owned/src/lib.rs"])),
            ["crates/owned/src/other.rs"],
        );
    }

    #[test]
    fn a_subtree_does_not_cover_a_prefix_sibling() {
        assert_eq!(
            out_of_surface(["crates/owned-extra/src/lib.rs"], &surface(&["crates/owned/**"])),
            ["crates/owned-extra/src/lib.rs"],
        );
    }

    #[test]
    fn apply_containment_fails_a_passing_verify_and_names_the_path() {
        // Pre-fix this candidate passed Verify: the mechanical umbrella never
        // looked at the declared surface. The gate must flip the verdict and
        // name every violating path.
        let overlay = apply_containment(
            StageVerdict::VerificationPassed,
            VerifyFailureSet::EMPTY,
            None,
            &["crates/other/src/lib.rs".to_owned()],
        );

        assert_eq!(overlay.verdict, StageVerdict::VerificationFailed);
        assert_eq!(overlay.failed_verifiers, VerifyFailureSet::one(VerifyFailure::Test));
        assert_eq!(
            overlay.findings.as_deref(),
            Some(containment_findings(&["crates/other/src/lib.rs".to_owned()]).as_str()),
        );
    }

    #[test]
    fn apply_containment_keeps_a_mechanical_set_and_prepends_findings() {
        let mechanical = VerifyFailureSet::one(VerifyFailure::Clippy);
        let overlay = apply_containment(
            StageVerdict::VerificationFailed,
            mechanical,
            Some("clippy: unused import".to_owned()),
            &["tests/lane/mod.rs".to_owned()],
        );

        assert_eq!(overlay.verdict, StageVerdict::VerificationFailed);
        assert_eq!(overlay.failed_verifiers, mechanical, "a named mechanical failure stays the accounting identity");
        let findings = overlay.findings.expect("violations produce findings");
        assert!(findings.starts_with("Candidate edits outside the declared surface:"));
        assert!(findings.contains("- tests/lane/mod.rs"));
        assert!(findings.contains("clippy: unused import"));
    }

    #[test]
    fn apply_containment_is_a_no_op_when_the_candidate_is_contained() {
        let overlay = apply_containment(StageVerdict::VerificationPassed, VerifyFailureSet::EMPTY, None, &[]);
        assert_eq!(overlay.verdict, StageVerdict::VerificationPassed);
        assert!(overlay.failed_verifiers.is_empty());
        assert_eq!(overlay.findings, None);
    }

    #[test]
    fn a_candidate_that_edits_outside_its_surface_fails_the_gate_with_that_path_named() {
        // Execution: a real base..HEAD diff whose only non-lockfile change sits
        // outside the declared surface. Today the mechanical Verify would pass
        // this candidate; the containment gate must fail it and name the path.
        let repo = candidate_repo();
        let base = git_head(repo.path());
        rewrite(repo.path(), "crates/other/src/lib.rs", "pub fn other() -> u8 { 2 }\n");
        rewrite(repo.path(), "Cargo.lock", "# rebuilt\n");
        commit(repo.path(), "out of surface");

        let changed = changed_paths(repo.path(), &base).expect("the candidate diff is readable");
        let violations = out_of_surface(changed.iter().map(String::as_str), &surface(&["crates/owned/**"]));
        let overlay = apply_containment(StageVerdict::VerificationPassed, VerifyFailureSet::EMPTY, None, &violations);

        assert_eq!(violations, ["crates/other/src/lib.rs"]);
        assert_eq!(overlay.verdict, StageVerdict::VerificationFailed);
        assert!(
            overlay.findings.as_deref().is_some_and(|findings| findings.contains("crates/other/src/lib.rs")),
            "the violating path is named, got {:?}",
            overlay.findings,
        );
    }

    fn surface(globs: &[&str]) -> Vec<String> {
        globs.iter().map(|glob| (*glob).to_owned()).collect()
    }

    fn candidate_repo() -> TempDir {
        let dir = tempfile::tempdir().expect("a temp dir for the fixture creates");
        git(dir.path(), &["init", "--object-format=sha1", "--quiet"]);
        git(dir.path(), &["config", "user.name", "containment"]);
        git(dir.path(), &["config", "user.email", "containment@test"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        git(dir.path(), &["config", "core.autocrlf", "false"]);
        rewrite(dir.path(), "crates/owned/src/lib.rs", "pub fn owned() -> u8 { 1 }\n");
        rewrite(dir.path(), "crates/other/src/lib.rs", "pub fn other() -> u8 { 1 }\n");
        rewrite(dir.path(), "Cargo.lock", "# seed\n");
        commit(dir.path(), "seed");
        dir
    }

    fn rewrite(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the fixture parent dir creates");
        }
        fs::write(&path, contents).expect("the fixture file writes");
    }

    fn commit(root: &Path, message: &str) {
        git(root, &["add", "-A"]);
        git(root, &["commit", "--quiet", "--message", message]);
    }

    fn git_head(root: &Path) -> String {
        let output = Command::new("git").current_dir(root).args(["rev-parse", "HEAD"]).output().expect("git starts");
        assert!(output.status.success(), "git rev-parse HEAD failed");
        String::from_utf8(output.stdout).expect("HEAD is utf-8").trim().to_owned()
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git").current_dir(root).args(args).output().expect("git starts");
        assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    }
}
