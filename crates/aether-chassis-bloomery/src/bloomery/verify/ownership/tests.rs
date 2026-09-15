//! The two decisions this module owns: what a findings text names, and who
//! owns what it named.
//!
//! Every fixture below is a shape production actually produced — the two
//! findings bloom 0f16e207's run 84DB66FF reported in full, a rustc diagnostic
//! as the distiller renders it, and a nextest record — because the parser's
//! only job is to survive them, and a fixture invented here would prove it
//! survives an invention.

use aether_bloomery::WorkpieceId;

use super::{ExtentSource, GateAttribution, MemberExtents, PathOwner, attribute_gate, gate_findings};

/// Run 84DB66FF's step-0 findings, verbatim: a suppression scanner naming two
/// locations on two unindented lines.
const SUPPRESS: &str = "\
The previous candidate failed verification.

### verify.suppress

crates/aether-chassis-bloomery/src/store/schema/tests.rs:238 — ignore — #[ignore = \"needs a store\"]
xtask/src/bloom/roll/mod.rs:144 — allow(clippy::disallowed_methods) — // aether-suppression-request: rolled by hand
";

/// Run A05DA0B9's step-0 findings: one rustdoc diagnostic, its location on the
/// indented continuation line rustc spells it on.
const DOCS: &str = "\
### verify.docs

error: public documentation for `rows` links to private item `READ_TABLES`
  --> crates/aether-chassis-bloomery/src/store/read_everything.rs:52:30
";

fn member(name: &str) -> WorkpieceId {
    WorkpieceId(name.to_owned())
}

fn changed(members: &[(&str, &[&str])]) -> MemberExtents {
    MemberExtents {
        source: ExtentSource::ChangedPaths,
        members: members
            .iter()
            .map(|(name, paths)| (member(name), paths.iter().map(|path| (*path).to_owned()).collect()))
            .collect(),
    }
}

fn only(findings: &str, gate: &str) -> super::GateFindings {
    gate_findings(findings)
        .into_iter()
        .find(|section| section.gate == gate)
        .unwrap_or_else(|| panic!("the findings carry a {gate} section"))
}

#[test]
fn a_suppression_scanners_lines_each_name_their_own_path() {
    let section = only(SUPPRESS, "verify.suppress");
    assert_eq!(
        section.paths,
        ["crates/aether-chassis-bloomery/src/store/schema/tests.rs", "xtask/src/bloom/roll/mod.rs"],
    );
    assert!(section.complete, "both scanner lines named a path, so the section discriminates");
}

#[test]
fn a_rustc_diagnostic_is_one_finding_with_its_indented_location() {
    let section = only(DOCS, "verify.docs");
    assert_eq!(section.paths, ["crates/aether-chassis-bloomery/src/store/read_everything.rs"]);
    assert!(section.complete);
}

#[test]
fn prose_before_the_first_gate_heading_belongs_to_no_gate() {
    // The findings open with "The previous candidate failed verification." —
    // path-free prose that must not become a pathless finding of the first
    // gate, which would bisect every red run that carries the opener.
    assert_eq!(gate_findings(SUPPRESS).len(), 1, "only the headed section is a gate");
}

#[test]
fn a_nextest_record_without_a_location_leaves_the_gate_inconclusive() {
    // nextest renders "N tests failed." above its records, and a TIMEOUT or
    // ABORT record carries no panic location at all. Either way the section
    // holds a finding that names nothing, and the paths its siblings name are
    // not the whole gate.
    let section = only(
        "\
### verify.test

2 tests failed.

example-a::shell timed_out

example-a::shell panicking_test
  crates/example-a/src/shell.rs:12:5
  assertion failed
",
        "verify.test",
    );
    assert_eq!(section.paths, ["crates/example-a/src/shell.rs"], "what it did name is still read");
    assert!(!section.complete, "a record naming no path means the gate does not discriminate");
}

#[test]
fn a_findings_text_with_no_recognizable_path_names_none() {
    let section = only("### verify.deps\n\nunused dependency: serde\n", "verify.deps");
    assert!(section.paths.is_empty());
    assert!(!section.complete);
}

#[test]
fn one_owner_attributes_and_the_evidence_can_name_the_path() {
    let extents = changed(&[("issue-6023", &["crates/a/src/lib.rs"]), ("issue-6026", &["crates/b/src/lib.rs"])]);
    let findings = only("### verify.suppress\n\ncrates/a/src/lib.rs:3 — ignore — #[ignore]\n", "verify.suppress");

    assert_eq!(attribute_gate(&findings, &extents), GateAttribution::Attributed(vec![member("issue-6023")]));
}

#[test]
fn a_path_no_member_changed_bisects_rather_than_charging_anyone() {
    // The base or an inherited head carries it. This is the case the ADR's
    // rejection of path-owner attribution was about, and it is exactly the one
    // that still buys probes.
    let extents = changed(&[("issue-6023", &["crates/a/src/lib.rs"])]);
    let findings = only("### verify.docs\n\nerror: bad link\n  --> crates/base/src/lib.rs:1:1\n", "verify.docs");

    assert!(matches!(attribute_gate(&findings, &extents), GateAttribution::Inconclusive(_)));
}

#[test]
fn a_path_two_members_changed_bisects() {
    let shared = "crates/shared/src/lib.rs";
    let extents = changed(&[("issue-6023", &[shared]), ("issue-6026", &[shared])]);
    let findings = only(&format!("### verify.fmt\n\nDiff in {shared} at line 4:\n"), "verify.fmt");

    assert!(matches!(attribute_gate(&findings, &extents), GateAttribution::Inconclusive(_)));
}

#[test]
fn two_owned_paths_charge_both_owners_in_composition_order() {
    let extents = changed(&[("issue-6026", &["crates/b/src/lib.rs"]), ("issue-6023", &["crates/a/src/lib.rs"])]);
    let findings = only(
        "### verify.suppress\n\ncrates/a/src/lib.rs:3 — ignore\ncrates/b/src/lib.rs:9 — ignore\n",
        "verify.suppress",
    );

    assert_eq!(
        attribute_gate(&findings, &extents),
        GateAttribution::Attributed(vec![member("issue-6026"), member("issue-6023")]),
    );
}

#[test]
fn a_bare_file_name_matches_only_on_a_segment_boundary() {
    // A finding may spell a location as the bare file the tool printed. The
    // suffix match that makes that usable must not let `sub-lib.rs` answer for
    // `lib.rs`, which would hand the gate to the wrong member.
    let extents = changed(&[("owner", &["crates/a/src/lib.rs"]), ("other", &["crates/b/src/sub-lib.rs"])]);

    assert_eq!(extents.owner("lib.rs"), PathOwner::One(member("owner")));
}

#[test]
fn the_declared_surface_fallback_reads_globs_rather_than_exact_paths() {
    // With no candidate delta available the surface stands in. It is coarser by
    // construction — a permission rather than a record — so the evidence has to
    // say which reading was used, and the glob membership has to actually run.
    let extents = MemberExtents {
        source: ExtentSource::DeclaredSurface,
        members: vec![
            (member("issue-6023"), vec!["crates/a/**".to_owned()]),
            (member("issue-6026"), vec!["crates/b/**".to_owned()]),
        ],
    };

    assert_eq!(extents.owner("crates/a/src/deep/nested.rs"), PathOwner::One(member("issue-6023")));
    assert_eq!(extents.owner("crates/c/src/lib.rs"), PathOwner::Unowned);
}
