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

/// Bloom 9680c483's dispatch-7846 findings (issue-6029), verbatim from
/// `dispatch-7846-step-0-evidence/evidence.json`: three clippy diagnostics that
/// each locate themselves, the source snippets rustc renders under them, and
/// the distiller's omission notice.
///
/// The snippet gutter is why this run bisected. rustc right-aligns the line
/// number in a gutter sized to the widest number in the block, so `110 |`,
/// `115 |`, `512 | /` and the `...   |` elision all start at column zero.
const CLIPPY: &str = "\
The previous candidate failed verification. It already carries the work order's change; what follows is what verification said is wrong with it. Fix these.

### verify.clippy

warning: consider bringing this path into scope with the `use` keyword
   --> crates/aether-chassis-bloomery/src/bloomery/executor/local/harness_usage.rs:110:22
    |
110 |     if let Ok(xdg) = std::env::var(\"XDG_DATA_HOME\")
    |                      ^^^^^^^^^^^^^
    |
    = help: for further information visit https://rust-lang.github.io/rust-clippy/rust-1.97.0/index.html#absolute_paths
    = note: requested on the command line with `-W clippy::absolute-paths`


warning: consider bringing this path into scope with the `use` keyword
   --> crates/aether-chassis-bloomery/src/bloomery/executor/local/harness_usage.rs:115:24
    |
115 |     Some(PathBuf::from(std::env::var(\"HOME\").ok()?).join(\".local/share/muse\"))
    |                        ^^^^^^^^^^^^^
    |
    = help: for further information visit https://rust-lang.github.io/rust-clippy/rust-1.97.0/index.html#absolute_paths


warning: this function has too many lines (132/100)
   --> crates/aether-chassis-bloomery/src/bloomery/reactor/executor/runtime/mod.rs:512:1
    |
512 | / fn terminate_live_order(
513 | |     store: &mut dyn StoreBackend,
514 | |     mut artifacts: Option<&mut ArtifactsCapabilityState>,
515 | |     executor: &dyn ExecutorPort,
...   |
519 | |     captures: &mut Vec<CancelledCapture>,
520 | | ) -> Vec<Admit> {
    | |_______________^
    |
    = help: for further information visit https://rust-lang.github.io/rust-clippy/rust-1.97.0/index.html#too_many_lines
    = note: `-W clippy::too-many-lines` implied by `-W clippy::pedantic`
    = help: to override `-W clippy::pedantic` add `#[allow(clippy::too_many_lines)]`


… 9 further diagnostics omitted
";

/// Bloom 9680c483's dispatch-7849 findings (retrospect-6a1fe13e6695), verbatim
/// from `dispatch-7849-step-0-evidence/evidence.json`: two gate sections of
/// `E0063` diagnostics, closing with rustc's explain notice, cargo's compile
/// and build-failed lines, and the verify lane's own exit report.
const MISSING_FIELDS: &str = "\
The previous candidate failed verification. It already carries the work order's change; what follows is what verification said is wrong with it. Fix these.

### verify.clippy

error[E0063]: missing fields `scope_model_override` and `scope_seat` in initializer of `aether_bloomery::CommissionShowView`
  --> xtask/src/bloom/amend/tests.rs:49:5
   |
49 |     CommissionShowView {
   |     ^^^^^^^^^^^^^^^^^^ missing `scope_model_override` and `scope_seat`


error[E0063]: missing fields `scope_model_override` and `scope_seat` in initializer of `aether_bloomery::CommissionShowView`
    --> xtask/src/bloom/mod.rs:1166:18
     |
1166 |         as_json(&aether_bloomery::CommissionShowView {
     |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ missing `scope_model_override` and `scope_seat`


error[E0063]: missing fields `model_override` and `seat` in initializer of `aether_bloomery::ScopeRunOpenedView`
    --> xtask/src/bloom/mod.rs:1919:30
     |
1919 |                     as_json(&aether_bloomery::ScopeRunOpenedView {
     |                              ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ missing `model_override` and `seat`


error[E0063]: missing fields `model_override` and `seat` in initializer of `aether_bloomery::ScopeRunOpenedView`
    --> xtask/src/bloom/mod.rs:1958:30
     |
1958 |                     as_json(&aether_bloomery::ScopeRunOpenedView {
     |                              ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ missing `model_override` and `seat`


For more information about this error, try `rustc --explain E0063`.

… 4 further diagnostics omitted

### verify.test

error[E0063]: missing fields `scope_model_override` and `scope_seat` in initializer of `CommissionShowView`
  --> xtask/src/bloom/amend/tests.rs:49:5
   |
49 |     CommissionShowView {
   |     ^^^^^^^^^^^^^^^^^^ missing `scope_model_override` and `scope_seat`

error[E0063]: missing fields `scope_model_override` and `scope_seat` in initializer of `CommissionShowView`
    --> xtask/src/bloom/mod.rs:1166:18
     |
1166 |         as_json(&aether_bloomery::CommissionShowView {
     |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ missing `scope_model_override` and `scope_seat`

error[E0063]: missing fields `model_override` and `seat` in initializer of `ScopeRunOpenedView`
    --> xtask/src/bloom/mod.rs:1919:30
     |
1919 |                     as_json(&aether_bloomery::ScopeRunOpenedView {
     |                              ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ missing `model_override` and `seat`

error[E0063]: missing fields `model_override` and `seat` in initializer of `ScopeRunOpenedView`
    --> xtask/src/bloom/mod.rs:1958:30
     |
1958 |                     as_json(&aether_bloomery::ScopeRunOpenedView {
     |                              ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ missing `model_override` and `seat`

For more information about this error, try `rustc --explain E0063`.
error: could not compile `xtask` (bin \"xtask\" test) due to 4 previous errors
warning: build failed, waiting for other jobs to finish...
error: command `/home/imateapot/.rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu/bin/cargo test --no-run --message-format json-render-diagnostics --package aether-bloomery --package aether-bloomery-console --package aether-bloomery-git --package aether-bloomery-github --package aether-bloomery-rest --package aether-chassis-bloomery --package aether-harness-bloomery --package aether-math --package xtask --all-features` exited with code 101
Command exited with non-zero status 101
";

/// Bloom `0c5a157e`'s `dispatch-8330-step-1` findings, verbatim down to the end
/// of the one diagnostic the distiller kept.
///
/// A `match_same_arms` lint, red because `-D warnings` is what the gate means
/// by a failure. Three of its lines sit at column zero without being findings:
/// the **bare `...`** rustc writes where a multi-line span is elided, the
/// **`help:` header** of the structured suggestion, and the suggestion's own
/// **`NNNN ~` replacement lines**, whose marker stands where the gutter pipe
/// would.
const SUGGESTION: &str = "\
### verify.clippy

warning: these match arms have identical bodies
    --> crates/aether-bloomery/src/reduce/coordination.rs:1884:13
     |
1884 | /             MemberVerifyOutcome::PassedStandalone { .. }
1885 | |             | MemberVerifyOutcome::PassedIn { .. }
1886 | |             | MemberVerifyOutcome::Survived { .. } => {}
     | |________________________________________________________^
...
1889 |               MemberVerifyOutcome::AwaitingSuppression { .. } => {}
     |               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
     |
     = help: if this is unintentional make the arms return different values
     = note: `-W clippy::match-same-arms` implied by `-W clippy::pedantic`
help: otherwise merge the patterns into a single arm
     |
1884 |             MemberVerifyOutcome::PassedStandalone { .. }
1885 |             | MemberVerifyOutcome::PassedIn { .. }
1886 ~             | MemberVerifyOutcome::Survived { .. } | MemberVerifyOutcome::AwaitingSuppression { .. } => {}
1887 |             // A parked member names no failure and joins no survivor group, so
1888 |             // it blocks nothing behind it.
1889 ~             }
     |
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
fn a_clippy_diagnostics_source_snippet_is_not_a_second_pathless_finding() {
    // Bloom 9680c483's dispatch-7846. Every snippet line rustc starts at column
    // zero — `110 |`, `512 | /`, the `...   |` elision — used to open a record
    // of its own, and a record naming no path is what the run reported as "a
    // finding under verify.clippy names no path".
    let section = only(CLIPPY, "verify.clippy");
    assert_eq!(
        section.paths,
        [
            "crates/aether-chassis-bloomery/src/bloomery/executor/local/harness_usage.rs",
            "crates/aether-chassis-bloomery/src/bloomery/reactor/executor/runtime/mod.rs",
        ],
    );
    assert!(!section.complete, "only the nine diagnostics the distiller dropped keep this section from discriminating");
}

#[test]
fn rustcs_and_cargos_closing_notices_do_not_make_a_located_gate_bisect() {
    // Bloom 9680c483's dispatch-7849. Its `verify.test` section carries no
    // omission notice, so once the snippet gutter and the build's closing
    // lines stop reading as findings the section discriminates on its own —
    // which is what an `Eject` bloom needed to charge the member and stop.
    let section = only(MISSING_FIELDS, "verify.test");
    assert_eq!(section.paths, ["xtask/src/bloom/amend/tests.rs", "xtask/src/bloom/mod.rs"]);
    assert!(section.complete, "an explain notice and a build tally are not findings that failed to name a path");
}

#[test]
fn a_clippy_suggestion_is_part_of_the_diagnostic_it_suggests_for() {
    // Bloom 0c5a157e's dispatch-8330. The gate held one diagnostic and that
    // diagnostic stated its file on the line under itself, yet the section read
    // as three further findings that named nothing — the bare elision marker,
    // the suggestion header, and the `~` replacement lines — so a gate that had
    // already answered "who owns this" was sent to be answered again.
    let section = only(SUGGESTION, "verify.clippy");

    assert_eq!(section.paths, ["crates/aether-bloomery/src/reduce/coordination.rs"]);
    assert!(section.complete, "the section's one diagnostic named its path, so the gate discriminates");
}

#[test]
fn a_warning_severity_lint_charges_the_member_whose_file_it_names() {
    // `-D warnings` is what makes a pedantic lint red, so most clippy gates
    // fail at warning severity rather than `error:`. The ownership reading
    // turns on the location a diagnostic states, never on its severity word.
    let extents = changed(&[
        ("issue-6032", &["crates/aether-bloomery/src/reduce/coordination.rs"]),
        ("issue-6041", &["crates/aether-bloomery/src/reduce/eject.rs"]),
    ]);

    assert_eq!(
        attribute_gate(&only(SUGGESTION, "verify.clippy"), &extents),
        GateAttribution::Attributed(vec![member("issue-6032")]),
    );
}

#[test]
fn a_section_whose_whole_block_arrived_indented_still_names_its_path() {
    // A findings text whose first content line under the heading is indented
    // used to be dropped line by line, because a continuation with no record
    // above it has nothing to continue. The section then held no findings at
    // all and the gate bisected.
    let section = only("### verify.docs\n\n  error: bad link\n    --> crates/a/src/lib.rs:1:1\n", "verify.docs");

    assert_eq!(section.paths, ["crates/a/src/lib.rs"]);
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

/// A rustc `E0308` with both of its spans, the shape a cross-member type
/// change produces: the mismatch is reported at the call site and the
/// definition it disagrees with is pointed at on a `note:` continuation.
///
/// Unlike the fixtures above this is not a captured section — bloom 9680c483's
/// two red runs were each single-member, so no production section this
/// repository has kept names two members at once. The lines are rustc's own
/// rendering of a two-span diagnostic, which is what the reader has to survive.
const TWO_SPAN: &str = "\
### verify.clippy

error[E0308]: mismatched types
   --> crates/aether-chassis-bloomery/src/bloomery/coordination.rs:88:31
    |
 88 |     let plan = build_plan(state, requests);
    |                               ^^^^^^^^ expected `Vec<MemberVerifyRequest>`, found `&[MemberVerifyRequest]`
    |
note: function defined here
   --> crates/aether-bloomery/src/reduce/coordination.rs:274:4
";

// The plausible bug: a red shared run whose finding sits in one member's file
// because *another* member changed the definition it disagrees with is read as
// the first member's lone defect, and that member is ejected while the pair
// that actually disagree is never named. The annotation does not change
// attribution — it records the pair so the bounce can be classified.
#[test]
fn a_finding_naming_two_members_writes_records_the_pair() {
    let extents = changed(&[
        ("issue-1", &["crates/aether-chassis-bloomery/src/bloomery/coordination.rs"]),
        ("issue-2", &["crates/aether-bloomery/src/reduce/coordination.rs"]),
    ]);
    let mentions = super::semantic_mentions(&only(TWO_SPAN, "verify.clippy"), &extents);

    assert_eq!(mentions.len(), 1, "one diagnostic names both writes: {mentions:?}");
    assert_eq!(mentions[0].site, member("issue-1"), "the site is where the diagnostic was reported");
    assert_eq!(mentions[0].implicated, vec![member("issue-2")], "the definition's owner is implicated, not charged");
}

// Tripwire: the annotation must stay silent on the ordinary case. Two findings
// in two members' files are two independent defects, and reporting them as a
// mutual conflict would make every multi-member red run look like one.
#[test]
fn independent_findings_in_two_members_files_record_no_pair() {
    let extents =
        changed(&[("issue-1", &["xtask/src/bloom/amend/tests.rs"]), ("issue-2", &["xtask/src/bloom/mod.rs"])]);

    assert!(
        super::semantic_mentions(&only(MISSING_FIELDS, "verify.test"), &extents).is_empty(),
        "each E0063 names one path, so no finding names two writes",
    );
}

// Bloom 0c5a157ecbe4's dispatch-8311 (retrospect-6a1fe13e6695) is this shape:
// the candidate added two fields to `CommissionShowView` in `aether-bloomery`
// and left xtask's initializers — inside its own reverse-dependency closure —
// unrepaired. It ran alone, and no sibling in the bloom wrote an xtask path.
//
// The plausible bug: an annotation eager enough to implicate whoever happens to
// be in the run would name a member here, turning a candidate's own miss of a
// reader in its closure into a fabricated two-member conflict and sending an
// innocent sibling to reconcile.
#[test]
fn a_candidates_own_miss_in_its_closure_implicates_nobody() {
    let extents = changed(&[
        ("retrospect-6a1fe13e6695", &["crates/aether-bloomery/src/values/api.rs"]),
        ("issue-6031", &["xtask/src/transform/verify/mod.rs"]),
    ]);

    assert!(
        super::semantic_mentions(&only(MISSING_FIELDS, "verify.test"), &extents).is_empty(),
        "the findings name only paths no member wrote, so there is no pair to record",
    );
}
