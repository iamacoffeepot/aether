//! The work-order overlay a fold collision hands its Reconcile lap (ADR-0189,
//! ADR-0218 §Amendment: reconcile is scoped to the merge).
//!
//! Every path that can send a member back to Reconcile — the legacy fold, the
//! eager append, the reconcile preparation, and the shared-run placement —
//! composes the same overlay here rather than writing its own prose. Two
//! reasons, and the second is the expensive one.
//!
//! The overlay is *parsed* downstream: the executor drain reads the paths back
//! out of `## Conflicting paths` to hold a Reconcile whose seam a sibling is
//! already rebuilding. A path list written as a bare `Conflicting paths:`
//! line is invisible to that reader, so the sibling hold silently stops
//! working for whichever collision path spelled it differently.
//!
//! And the overlay is the *only* thing that tells the resumed author session
//! what it is being paid to do. A lap whose order says "reproduce this
//! member's intent" re-authors a change that is already proved, at the price of
//! a whole construction session — bloom `0f16e207`'s `dispatch-7168` spent 225
//! model calls and 15.0 minutes doing exactly that before its verify could even
//! start. The merge is the only new work, so the order names the merge.

use core::fmt::Write;

/// The standing merge-only contract. Static text: the situational half is the
/// caller's `situation` line and the variable halves are the two sections
/// below, the way ADR-0214 separates instruction from context.
///
/// It names the two sections in prose rather than reproducing their headings,
/// because the executor drain recovers the path list by splitting this same
/// string on the literal heading — a contract that quoted it would put a second
/// match ahead of the real section.
const MERGE_ONLY_CONTRACT: &str = "\
Resolve the merge, and only the merge. Your change on this member is already verified as it stands — keep it, do \
not re-author it, do not re-derive it from the work order, and do not widen it. You are checked out on the folded \
head that moved under you; the conflicting paths listed below are the only places the two sides disagree, and the \
conflicted candidate below is the contribution the fold could not place. Reconcile those paths, leave every other \
file of your change byte-identical, and stay inside the declared surface.";

/// Compose one conflicted member's Reconcile overlay.
///
/// `situation` is the collision's own sentence — which plan could not place
/// what onto which head — and is omitted when the caller has nothing
/// situational to say. `paths` are the colliding paths in the order the source
/// reported them; `diff` is the member's own contribution, which the folded
/// checkout does not carry.
pub(super) fn fold_conflict_overlay(situation: Option<&str>, paths: &[String], diff: &str) -> String {
    let mut overlay = String::from("## Fold conflict\n\n");
    if let Some(situation) = situation.map(str::trim).filter(|situation| !situation.is_empty()) {
        overlay.push_str(situation);
        overlay.push_str("\n\n");
    }
    overlay.push_str(MERGE_ONLY_CONTRACT);
    overlay.push('\n');

    if !paths.is_empty() {
        overlay.push_str("\n## Conflicting paths\n\n");
        for path in paths {
            let _ = writeln!(overlay, "- {path}");
        }
    }

    let trimmed = diff.trim();
    if !trimmed.is_empty() {
        overlay.push_str("\n## Conflicted candidate\n\n```diff\n");
        overlay.push_str(trimmed);
        if !trimmed.ends_with('\n') {
            overlay.push('\n');
        }
        overlay.push_str("```\n");
    }
    overlay
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tripwire: the executor drain recovers the colliding paths by splitting on
    // the `## Conflicting paths` heading and reading `- ` items
    // (`reactor::executor::runtime::fold_conflict_paths`). Every overlay this
    // module composes must be readable by that parser, whichever collision path
    // produced it — a bare `Conflicting paths:` line parses as no paths at all,
    // and the sibling-Reconcile hold then never fires.
    #[test]
    fn every_overlay_states_the_merge_only_contract_in_parseable_sections() {
        let paths = alloc_paths(&["crates/a/src/lib.rs", "crates/b/src/mod.rs"]);
        let overlay = fold_conflict_overlay(
            Some("Shared run 0a could not place composition 0b onto head 0c."),
            &paths,
            "diff --git a b\n+line\n",
        );

        assert!(overlay.starts_with("## Fold conflict\n\n"), "{overlay}");
        assert!(overlay.contains("Shared run 0a could not place composition 0b onto head 0c."), "{overlay}");
        assert!(overlay.contains("Resolve the merge, and only the merge."), "{overlay}");
        assert!(overlay.contains("already verified as it stands"), "{overlay}");
        let (_, listed) = overlay.split_once("## Conflicting paths\n").expect("the parseable heading");
        assert!(listed.contains("- crates/a/src/lib.rs\n"), "{overlay}");
        assert!(listed.contains("- crates/b/src/mod.rs\n"), "{overlay}");
        assert!(overlay.contains("## Conflicted candidate\n\n```diff\ndiff --git a b\n+line\n```\n"), "{overlay}");
    }

    // Tripwire: a collision with nothing situational to say still carries the
    // contract. The legacy fold has no plan sentence, and an overlay that
    // degraded to an empty task there would leave that lap unscoped.
    #[test]
    fn a_situationless_collision_still_carries_the_contract() {
        let overlay = fold_conflict_overlay(None, &[], "");

        assert!(overlay.starts_with("## Fold conflict\n\nResolve the merge, and only the merge."), "{overlay}");
        assert!(!overlay.contains("## Conflicting paths"), "{overlay}");
        assert!(!overlay.contains("## Conflicted candidate"), "{overlay}");
    }

    fn alloc_paths(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }
}
