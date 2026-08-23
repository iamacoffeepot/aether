//! Member-Verify declared-surface containment.
//!
//! A candidate that edits a path no declared-surface glob covers must fail
//! Verify with that path named. The check is a set-membership test over the
//! globs sealed at admission: no new stage; the refusal journals as
//! `verify.containment` (ADR-0209). `Cargo.lock` is structurally shared and
//! machine-maintained, so a dependency-graph-neutral rebuild that touches it
//! is not a violation.

use std::path::Path;

use aether_bloomery::{StageVerdict, VerifyFailure, VerifyFailureSet};
use aether_bloomery_git::command::{self, GitCommandError};

/// The membership test this module is built on, now owned by the domain crate
/// so xtask can reach it too (#5300) — xtask depends on `aether-bloomery` and
/// not on this chassis.
///
/// Re-exported rather than merely imported: `verify::mod`'s own `pub use` list
/// and `batch.rs`'s `super::path_in_surface` both address it through this
/// module, and the name has to be in scope here for [`out_of_surface`], which
/// calls it unqualified.
pub use aether_bloomery::path_in_surface;

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

/// The repository-relative paths `base..HEAD` changed.
///
/// Same read the mechanical umbrella uses to scope a member Verify, so the
/// containment set and the compile-closure set cannot drift onto different
/// diffs. Flags (`--no-renames -z`) come from the git command layer.
pub fn changed_paths(repo: &Path, base: &str) -> Result<Vec<String>, GitCommandError> {
    command::name_only_paths(repo, base, "HEAD")
}

/// The revision the candidate's own delta is measured from: the capture
/// commit's first parent, which is the tree the lane that wrote it was given.
///
/// The candidate is one commit — the host stages the whole worktree and commits
/// once (ADR-0152), so a lane's output is always exactly one commit on top of
/// whatever it checked out. `HEAD^` is therefore the tree that lane started
/// from, whatever that tree was: the member's construct base on a first lap,
/// the previous lap's capture on a repair, and the folded head carrying every
/// sibling's resolved work on a lap the coordinator dispatched onto the fold.
///
/// That last case is why this exists. A member reconciled or refined on the
/// fold produces a candidate whose history runs back through nine siblings'
/// commits, so measuring it against the *bloom base* charges it with every path
/// those siblings changed — forty-five files it never touched — and the repair
/// lane is then told to revert its siblings' work, which re-collides on the
/// next fold.
///
/// Containment stays complete under the narrowing, because it runs at every
/// Verify: each lap's delta is judged at that lap's own gate, and the laps
/// compose. What is lost is only the re-judgement of a delta that already
/// passed.
///
/// [`None`] when `HEAD` has no first parent (a root commit) or git cannot
/// answer; the caller falls back to the range the work order named rather than
/// skipping the gate, because a containment check that quietly does not run is
/// the one failure mode worse than one that measures too much.
#[must_use]
pub fn candidate_delta_base(repo: &Path) -> Option<String> {
    #[allow(clippy::literal_string_with_formatting_args, reason = "git revision syntax, not a format string")]
    let parent = command::run_ok(repo, &["rev-parse", "--verify", "--quiet", "HEAD^"]).ok()?;
    let parent = parent.trim();
    (!parent.is_empty()).then(|| parent.to_owned())
}

/// The paths one candidate's own delta changed that `surface` does not cover.
///
/// The range is [`candidate_delta_base`] — the tree the lane that wrote this
/// candidate was given — falling back to `order_base`, the range the work order
/// named, only when the checkout has no first parent to read. Falling back
/// rather than skipping keeps the gate running on the one shape that has no
/// delta base at all.
///
/// The whole gate in one call so the range and the membership test cannot be
/// assembled differently by the executor and by a test: a containment answer
/// that depends on which caller composed it is the failure this exists under.
///
/// [`None`] when the diff is unreadable, which the caller reports rather than
/// treating as an empty violation set.
#[must_use]
pub fn candidate_violations(repo: &Path, order_base: Option<&str>, surface: &[String]) -> Option<Vec<String>> {
    let base = candidate_delta_base(repo).or_else(|| order_base.map(str::to_owned))?;
    let changed = changed_paths(repo, &base).ok()?;
    Some(out_of_surface(changed.iter().map(String::as_str), surface))
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
/// every path, and unions [`VerifyFailure::Containment`] into the typed set so
/// a concurrent mechanical failure keeps its own identities and a pure
/// containment refusal is no longer reclassified as [`VerifyFailure::Test`].
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
    let failed_verifiers = failed_verifiers.union(VerifyFailureSet::one(VerifyFailure::Containment));
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

    use super::{
        apply_containment, candidate_delta_base, candidate_violations, changed_paths, containment_findings,
        out_of_surface,
    };

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
        assert_eq!(overlay.failed_verifiers, VerifyFailureSet::one(VerifyFailure::Containment));
        assert_eq!(
            overlay.findings.as_deref(),
            Some(containment_findings(&["crates/other/src/lib.rs".to_owned()]).as_str()),
        );
    }

    #[test]
    fn apply_containment_unions_containment_onto_a_mechanical_set_and_prepends_findings() {
        let mechanical = VerifyFailureSet::one(VerifyFailure::Clippy);
        let overlay = apply_containment(
            StageVerdict::VerificationFailed,
            mechanical,
            Some("clippy: unused import".to_owned()),
            &["tests/lane/mod.rs".to_owned()],
        );

        assert_eq!(overlay.verdict, StageVerdict::VerificationFailed);
        assert_eq!(
            overlay.failed_verifiers,
            mechanical.union(VerifyFailureSet::one(VerifyFailure::Containment)),
            "a named mechanical failure keeps its identity and gains containment"
        );
        let findings = overlay.findings.expect("violations produce findings");
        assert!(findings.starts_with("Candidate edits outside the declared surface:"));
        assert!(findings.contains("- tests/lane/mod.rs"));
        assert!(findings.contains("clippy: unused import"));
    }

    #[test]
    fn apply_containment_keeps_a_concurrent_test_failure_and_gains_containment() {
        let overlay = apply_containment(
            StageVerdict::VerificationFailed,
            VerifyFailureSet::one(VerifyFailure::Test),
            Some("1 test failed".to_owned()),
            &["docs/guide/x.md".to_owned()],
        );

        assert_eq!(
            overlay.failed_verifiers,
            [VerifyFailure::Test, VerifyFailure::Containment].into_iter().collect::<VerifyFailureSet>(),
        );
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

    #[test]
    fn a_candidate_written_on_the_fold_is_not_charged_with_its_siblings_paths() {
        // Tripwire: the coordinator dispatches a repair lap onto the folded
        // tree, so the candidate that lap produces carries every sibling's
        // resolved work in its history. Measured against the bloom base — the
        // range the work order names — the gate charges the member with files
        // it never touched, and the repair lane is then told to revert its
        // siblings' work, which re-collides on the very next fold.
        let repo = candidate_repo();
        let order_base = git_head(repo.path());

        rewrite(repo.path(), "crates/sibling-a/src/lib.rs", "pub fn a() -> u8 { 1 }\n");
        rewrite(repo.path(), "crates/sibling-b/src/lib.rs", "pub fn b() -> u8 { 1 }\n");
        commit(repo.path(), "fold");
        let fold = git_head(repo.path());

        rewrite(repo.path(), "crates/owned/src/lib.rs", "pub fn owned() -> u8 { 2 }\n");
        commit(repo.path(), "refine on the fold");

        assert_eq!(
            candidate_delta_base(repo.path()).as_deref(),
            Some(fold.as_str()),
            "the candidate's delta is measured from the tree its lane was given",
        );
        assert_eq!(
            candidate_violations(repo.path(), Some(&order_base), &surface(&["crates/owned/**"])),
            Some(Vec::new()),
            "a member that touched only its own crate is contained however deep the fold beneath it runs",
        );
    }

    #[test]
    fn a_candidate_written_on_the_fold_still_fails_for_its_own_stray_path() {
        // The narrowing must not become an exemption: the member's own delta is
        // still judged, so a stray edit inside the fold lap is named exactly as
        // one on a first lap would be.
        let repo = candidate_repo();
        let order_base = git_head(repo.path());
        rewrite(repo.path(), "crates/sibling-a/src/lib.rs", "pub fn a() -> u8 { 1 }\n");
        commit(repo.path(), "fold");
        rewrite(repo.path(), "crates/other/src/lib.rs", "pub fn other() -> u8 { 3 }\n");
        commit(repo.path(), "refine on the fold");

        assert_eq!(
            candidate_violations(repo.path(), Some(&order_base), &surface(&["crates/owned/**"])),
            Some(vec!["crates/other/src/lib.rs".to_owned()]),
        );
    }

    #[test]
    fn a_root_commit_has_no_delta_base_and_the_caller_falls_back() {
        // The one shape with no first parent to read. Answering `None` is what
        // sends the caller to the range the work order named rather than
        // skipping the gate.
        let dir = tempfile::tempdir().expect("a temp dir for the fixture creates");
        git(dir.path(), &["init", "--object-format=sha1", "--quiet"]);
        git(dir.path(), &["config", "user.name", "containment"]);
        git(dir.path(), &["config", "user.email", "containment@test"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        rewrite(dir.path(), "crates/owned/src/lib.rs", "pub fn owned() -> u8 { 1 }\n");
        commit(dir.path(), "root");

        assert_eq!(candidate_delta_base(dir.path()), None);
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
