//! Repair-lap triage tests (#4959): the extraction rules, the noise rule, and
//! the verdict — including the live incident the triage exists to catch.

use super::{TriageVerdict, changed_surface, named_surface, triage_repair};

/// The finding from bloom `10a1228c`: a coverage gap that fails no gate, naming
/// one symbol and the file it lives in.
const INCIDENT_FINDING: &str = "[wp-golden] `representative()` in \
                                `crates/aether-bloomery/tests/golden_decisions/main.rs` does not reach every \
                                effect family, so the pinned bytes freeze less than the graph.";

/// The dodge, twice over: a real edit, in the file the finding named, that
/// touches nothing the finding is about.
const DODGE_DIFF: &str = "diff --git a/crates/aether-bloomery/tests/golden_decisions/main.rs \
                          b/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                          --- a/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                          +++ b/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                          @@ -530,7 +530,7 @@ fn the_decisions_graph_is_wire_frozen() {\n\
                          \x20    let bytes = to_vec(&decisions);\n\
                          -    let encoded = bytes.unwrap();\n\
                          +    let encoded = bytes.expect(\"a fixture value wire-encodes\");\n\
                          \x20    assert_eq!(encoded, GOLDEN_DECISIONS);\n";

// Tripwire: the incident this whole module exists for. A repair that edits the
// file a finding named while leaving the thing it named alone must bounce — if
// this ever passes, the triage has been widened into a file-level check and the
// dodge that cost two Opus judge rounds is admitted again.
#[test]
fn the_incident_bounces_a_repair_that_edits_the_named_file_but_not_the_named_symbol() {
    let verdict = triage_repair(INCIDENT_FINDING, Some(DODGE_DIFF));

    let TriageVerdict::Dodged(named) = &verdict else {
        panic!("the incident's repair must bounce, got {verdict:?}");
    };
    assert!(named.iter().any(|name| name == "representative"), "the bounce names the symbol that went untouched");
    assert!(verdict.bounces());
}

#[test]
fn a_repair_that_changes_a_line_naming_the_symbol_passes() {
    let repair = "--- a/crates/aether-bloomery/tests/golden_decisions/completeness.rs\n\
                  +++ b/crates/aether-bloomery/tests/golden_decisions/completeness.rs\n\
                  @@ -297,2 +297,3 @@ fn every_wire_reachable_family_is_represented() {\n\
                  +    let effects = representative().effects;\n";

    assert_eq!(triage_repair(INCIDENT_FINDING, Some(repair)), TriageVerdict::Addressed("representative".to_owned()));
}

// The forgiving half of the strictness above: a repair *inside* the named
// function whose changed lines never spell its name still passes, because the
// hunk's section heading says where the change is. Without this the rule would
// bounce the ordinary case — a body edit — and cost a lap every time.
#[test]
fn a_change_under_a_section_heading_naming_the_symbol_passes() {
    let repair = "--- a/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                  +++ b/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                  @@ -240,6 +240,7 @@ fn representative() -> Decisions {\n\
                  +            Decision::RecordCompositionFinding { bloom, finding: finding() },\n";

    assert!(!triage_repair(INCIDENT_FINDING, Some(repair)).bounces());
}

#[test]
fn a_finding_that_names_nothing_extractable_passes() {
    let vague = "The retry loop feels wrong and the naming could be clearer throughout.";

    assert_eq!(triage_repair(vague, Some(DODGE_DIFF)), TriageVerdict::NothingNamed);
}

#[test]
fn an_uninspectable_lap_passes() {
    assert_eq!(triage_repair(INCIDENT_FINDING, None), TriageVerdict::NotInspected, "no diff was filed");
    assert_eq!(triage_repair(INCIDENT_FINDING, Some("")), TriageVerdict::NotInspected, "an empty diff changes nothing");
    let oversized = format!("+{}\n", "x".repeat(super::MAX_TRIAGED_DIFF_BYTES));
    assert_eq!(triage_repair(INCIDENT_FINDING, Some(&oversized)), TriageVerdict::NotInspected, "past the cap");
}

// Tripwire: the whitespace rule. A re-indent that moves a line naming the
// finding's symbol without changing it is not a repair, and a rule that counted
// it would let `cargo fmt` satisfy any finding.
#[test]
fn a_whitespace_only_change_is_not_a_touch() {
    let reindent = "--- a/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                    +++ b/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                    @@ -1,3 +1,3 @@\n\
                    -    let value = representative();\n\
                    +        let value = representative();\n\
                    --- a/src/other.rs\n\
                    +++ b/src/other.rs\n\
                    @@ -9,0 +10,1 @@\n\
                    +const ADDED: usize = 1;\n";

    assert!(
        triage_repair(INCIDENT_FINDING, Some(reindent)).bounces(),
        "re-indenting a line changes nothing; the substantive change is elsewhere and names nothing",
    );
}

// Tripwire: a finding that names only a file is not triaged. A compiler
// diagnostic points at where a symptom surfaced, not at where the fix belongs,
// and mechanical `Verify` failures — the highest-volume repair dispatcher in the
// loop — are exactly that shape. Triaging on the path would bounce honest laps
// that fixed the cause a file over.
#[test]
fn a_finding_that_names_only_a_file_is_not_triaged() {
    let diagnostic = "verify.check failed.\n\nerror[E0308]: mismatched types\n  --> crates/mock/src/lib.rs:7:20";
    let elsewhere = "--- a/crates/mock/src/helper.rs\n\
                     +++ b/crates/mock/src/helper.rs\n\
                     @@ -1,0 +2,1 @@\n\
                     +pub fn widen(value: u16) -> u32 { u32::from(value) }\n";

    assert_eq!(triage_repair(diagnostic, Some(elsewhere)), TriageVerdict::NothingNamed);
    assert!(
        named_surface(diagnostic).paths.iter().any(|path| path == "crates/mock/src/lib.rs"),
        "the path is still extracted — that is what keeps it from being read as a symbol",
    );
}

// Tripwire: `e.g` and `i.e` reach the path shape if the extension floor drops to
// one character, and a finding whose only "path" is prose punctuation would then
// bounce every repair under it — a false bounce manufactured out of nothing.
#[test]
fn prose_abbreviations_are_not_paths() {
    let surface = named_surface("Prefer the typed resolver, e.g. over a hand-hashed id; i.e. `ctx.actor`.");

    assert!(surface.paths.is_empty(), "no path is named, found {:?}", surface.paths);
}

#[test]
fn a_backtick_span_with_whitespace_is_a_quotation_not_a_name() {
    let surface = named_surface("It writes `let mut total = 0;` and never reads `total_spend`.");

    assert_eq!(surface.symbols, vec!["total_spend".to_owned()], "only the whitespace-free span names something");
}

#[test]
fn a_qualified_path_names_its_leaf() {
    let surface = named_surface("`Decision::RecordEvidence` is folded twice.");

    assert_eq!(surface.symbols, vec!["RecordEvidence".to_owned()]);
}

// Tripwire: whole-word matching. `expect` inside `expected` (or `Decision`
// inside `Decisions`) would make almost any diff match almost any finding, which
// is the quiet way an advisory-strict gate becomes an always-pass one.
#[test]
fn a_symbol_matches_on_word_boundaries_only() {
    let changed = changed_surface(
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,0 +2,1 @@\n+    let expected = Decisions::default();\n",
    );

    assert!(!changed.mentions("expect"), "`expect` must not match inside `expected`");
    assert!(!changed.mentions("Decision"), "`Decision` must not match inside `Decisions`");
    assert!(changed.mentions("Decisions"));
    assert!(changed.mentions("expected"));
}

// A changed file's path is part of what the lap changed, so a finding naming a
// module reaches a repair that only renamed or added the file bearing its name.
// Generous on purpose, and safe: a path is something the lap chose to touch.
#[test]
fn a_changed_files_path_is_part_of_the_changed_text() {
    let changed = changed_surface(
        "--- a/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
         +++ b/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
         @@ -1,0 +2,1 @@\n\
         +// added\n",
    );

    assert!(changed.mentions("golden_decisions"), "the changed file's own path names the module it holds");
    assert!(!changed.mentions("aether_bloomery"), "and only the path the diff actually spells");
}

/// The same incident, restated in the classified format (#4961): a mechanical
/// finding naming the check that would have caught it.
const MECHANICAL_FINDING: &str = "MECHANICAL (check: `every_wire_reachable_family_is_represented` in \
                                  `crates/aether-bloomery/tests/golden_decisions/completeness.rs`) — \
                                  `representative()` in `crates/aether-bloomery/tests/golden_decisions/main.rs` \
                                  does not reach every effect family.";

// Tripwire (#4961): a mechanical finding is accepted only when the repair
// contains the check it named, and that is strictly narrower than the ordinary
// rule. This diff edits `representative` — a symbol the finding names, which is
// what the ordinary rule asks for — and adds no check at all, so the finding
// would come back as a review round next time instead of a red gate. Falling
// back to the ordinary rule here is the regression: the lap passes, the judge
// re-reads the same tree, and the mechanical class buys nothing.
#[test]
fn a_mechanical_repair_that_adds_no_named_check_bounces_even_when_it_edits_a_named_symbol() {
    let repair = "--- a/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                  +++ b/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                  @@ -240,6 +240,7 @@ fn representative() -> Decisions {\n\
                  +            Decision::RecordCompositionFinding { bloom, finding: finding() },\n";

    let verdict = triage_repair(MECHANICAL_FINDING, Some(repair));

    let TriageVerdict::Dodged(named) = &verdict else {
        panic!("a mechanical repair without its named check must bounce, got {verdict:?}");
    };
    assert!(
        named.iter().any(|name| name == "every_wire_reachable_family_is_represented"),
        "the bounce names the check the lap owed, not the symbols it happened to touch: {named:?}",
    );
    assert!(
        !named.iter().any(|name| name == "representative"),
        "and it does not re-offer the name the lap already satisfied: {named:?}",
    );
}

// The pass side of the same rule, and the reason the check has to be named as a
// symbol or a path rather than described: a lap that adds the named check is
// credited by it.
#[test]
fn a_mechanical_repair_that_adds_the_named_check_passes() {
    let repair = "--- /dev/null\n\
                  +++ b/crates/aether-bloomery/tests/golden_decisions/completeness.rs\n\
                  @@ -0,0 +1,3 @@\n\
                  +#[test]\n\
                  +fn every_wire_reachable_family_is_represented() {\n";

    assert_eq!(
        triage_repair(MECHANICAL_FINDING, Some(repair)),
        TriageVerdict::Addressed("every_wire_reachable_family_is_represented".to_owned()),
    );
}

// A mechanical finding that named no check falls back to the ordinary rule
// rather than bouncing everything: the class still blocks, and holding a lap to
// a check nobody named would refuse every honest repair of it.
#[test]
fn a_mechanical_finding_without_a_check_is_triaged_by_the_ordinary_rule() {
    let checkless = "MECHANICAL — `representative()` in \
                     `crates/aether-bloomery/tests/golden_decisions/main.rs` misses a family.";
    let repair = "--- a/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                  +++ b/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                  @@ -240,6 +240,7 @@ fn representative() -> Decisions {\n\
                  +            Decision::RecordCompositionFinding { bloom, finding: finding() },\n";

    assert!(!triage_repair(checkless, Some(repair)).bounces());
    assert!(triage_repair(checkless, Some(DODGE_DIFF)).bounces(), "and the dodge still bounces");
}
