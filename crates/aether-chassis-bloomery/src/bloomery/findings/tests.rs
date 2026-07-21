//! Findings-decomposition tests (ADR-0153): the block/tag grammar and the
//! fail-closed completeness rule that gates narrowing the implication.

use super::{FindingsDecomposition, decompose_findings};

fn roster(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

#[test]
fn complete_attribution_slices_per_member_and_narrows() {
    let findings = "[wp-auth] The retry loop drops the last attempt.\nSecond line of the same finding.\n\n\
                    [wp-render] The pass order inverts the depth test.\n\n\
                    [wp-auth] The token refresh races the tick.";
    let decomposition = decompose_findings(findings, &roster(&["wp-auth", "wp-render"]));

    assert!(decomposition.is_complete());
    assert_eq!(decomposition.owners(), vec!["wp-auth".to_owned(), "wp-render".to_owned()]);
    assert_eq!(
        decomposition.slices[0].1,
        "[wp-auth] The retry loop drops the last attempt.\nSecond line of the same finding.\n\n\
         [wp-auth] The token refresh races the tick.",
        "a member's blocks join verbatim in input order, tags kept",
    );
    assert_eq!(decomposition.slices[1].1, "[wp-render] The pass order inverts the depth test.");
    assert_eq!(decomposition.unattributed, None);
}

#[test]
fn an_untagged_or_unknown_tagged_block_fails_the_attribution_closed() {
    // One attributed block plus one untagged: the slice still forms, but the
    // decomposition is incomplete — the caller must not narrow the implication.
    let untagged = decompose_findings(
        "[wp-auth] A real finding.\n\nSomething cross-cutting about the whole diff.",
        &roster(&["wp-auth"]),
    );
    assert!(!untagged.is_complete());
    assert_eq!(untagged.unattributed.as_deref(), Some("Something cross-cutting about the whole diff."));

    // A tag outside the roster is a hallucinated member — unattributed, never a
    // slice the reducer would reject as NotAMember.
    let unknown = decompose_findings("[wp-ghost] A finding for a member that does not exist.", &roster(&["wp-auth"]));
    assert!(!unknown.is_complete());
    assert!(unknown.slices.is_empty());
    assert_eq!(unknown.unattributed.as_deref(), Some("[wp-ghost] A finding for a member that does not exist."));
}

#[test]
fn a_tag_on_a_continuation_line_never_attributes() {
    // The block's owner is decided by its first line only — a quoted `[id]`
    // mid-block must not re-route the block to that member.
    let decomposition =
        decompose_findings("The setup section quotes\n[wp-auth] inside a continuation line.", &roster(&["wp-auth"]));
    assert!(decomposition.slices.is_empty());
    assert!(decomposition.unattributed.is_some());
}

#[test]
fn empty_findings_decompose_to_nothing() {
    assert_eq!(decompose_findings("", &roster(&["wp-auth"])), FindingsDecomposition::default());
    assert_eq!(decompose_findings("\n\n  \n", &roster(&["wp-auth"])), FindingsDecomposition::default());
    // Nothing attributed and nothing unattributed — but also no owners, so the
    // completeness gate still refuses to narrow.
    assert!(!decompose_findings("", &roster(&["wp-auth"])).is_complete());
}
