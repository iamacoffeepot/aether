//! Which backticked anchors in a plan step make a coverage demand (ADR-0208).
//!
//! The freeze projection runs an inverse search for every backticked
//! identifier a plan step names and hands the resolved defining paths to the
//! verifier as the *refusing* population. That is right for an anchor the
//! workpiece is making a claim about, and wrong for a common word: `truncate`,
//! `notify`, `Message`, `ref_name`, `hex_of`, `Fixture`, `Record` and `title`
//! all resolve definitions in crates the work never touches, so one such word
//! in one plan step refuses the whole run after all its authoring is done.
//!
//! The separator is structural, and it is the declared surface itself. Two
//! populations, measured on this workspace at 2026-08-26:
//!
//! - A genuine anchor has a definition **inside** the surface — the workpiece
//!   edits where the name lives — and the search's value is the *rest* of its
//!   definitions, the impls a signature change is guaranteed to touch.
//!   `adopt_candidate` (ADR-0208's own example) defines in three crates; a
//!   surface naming one of them keeps its demand on the other two, which is
//!   the whole point of running the search.
//! - A common word has **no** definition inside the surface. `notify` resolves
//!   two definitions in `aether-substrate`, `title` one in the console,
//!   `truncate` four across three unrelated crates: nothing about any of them
//!   is a statement about the work, so nothing about them should refuse it.
//!
//! Crate spread alone cannot separate the two — `truncate` spreads across
//! three crates and `adopt_candidate` across three — which is why the
//! load-bearing half of the rule is the surface-admits-a-definition test.
//!
//! The mixed case is a name the surface does define that also resolves in
//! crates the work never touches (`Pending` as a chassis `BaseVerdict` and as
//! substrate offload; `from_config` on a chassis config and in kit-widget).
//! One or two foreign crates sit under [`FOREIGN_CRATE_SPREAD_LIMIT`], so
//! spread alone keeps the demand and the run is refused for a homonym. The
//! separator that is actually true: a signature change inside the surface can
//! only force an edit in a crate in the surface's reverse-dependency closure
//! (the affected-set machinery, asked of the surface's crates). A definition
//! outside that closure shares a name, never an impl the change touches, and
//! is discounted before the spread limit runs on what remains.
//!
//! A discounted anchor is dropped from the refusing population, never from the
//! report: it stays in the projection's `named_symbols`, so the verifier still
//! classifies it into an advisory bucket, and the lane stamps the calibration's
//! own note beside the evidence.

use std::collections::BTreeSet;

use anyhow::Result;

use crate::affected::graph::Workspace;
use crate::symbols::references::crate_label;

/// How many distinct in-closure foreign crates a surface-local anchor's
/// definitions may spread across before the anchor reads as a common word
/// rather than one identity.
///
/// Three, because two is where the widest genuine anchor measured on this
/// workspace sits: `adopt_candidate` defines in `aether-bloomery`,
/// `aether-bloomery-git` and `aether-chassis-bloomery`, so a surface naming one
/// of the three leaves two foreign crates and must keep its demand. This value
/// sits one above that ceiling. It is not tuned against a refusal count, and it
/// is deliberately the weaker half of the mixed-case rule: out-of-closure
/// homonyms are discounted first, and an anchor with no definition inside the
/// surface is discounted at any spread.
const FOREIGN_CRATE_SPREAD_LIMIT: usize = 3;

/// One defining path of an anchor, and whether the declared surface admits it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Definition {
    /// The repository-relative defining path, as the search named it.
    pub path: String,
    /// Whether the declared surface admits that path, decided by the same
    /// `path_in_surface` the verifier and the containment gate use.
    pub covered: bool,
}

/// What the calibration concluded about one anchor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Anchor {
    /// The surface admits at least one definition and the in-closure rest stay
    /// inside the spread limit: a claim about this work, so every in-closure
    /// defining path keeps its coverage demand.
    SurfaceLocal {
        /// Foreign crates discounted because they sit outside the surface's
        /// reverse-dependency closure. Empty when every foreign definition is
        /// in-closure, or there were none.
        homonyms: Vec<String>,
    },
    /// The surface admits no definition at all. The plan mentions the name; it
    /// does not claim the code.
    Foreign {
        /// The crates that do define it, in search order.
        crates: Vec<String>,
    },
    /// The surface admits a definition, and the in-closure remainder also
    /// resolves across enough foreign crates to read as a common word.
    Broad {
        /// The foreign crates that also define it, in search order.
        crates: Vec<String>,
    },
}

impl Anchor {
    /// Whether this anchor's in-closure defining paths enter the refusing
    /// population.
    pub(super) fn demands_coverage(&self) -> bool {
        matches!(self, Self::SurfaceLocal { .. })
    }

    /// The in-closure defining paths that keep a coverage demand, or none when
    /// the whole anchor is discounted.
    pub(super) fn demanding<'a>(
        &self,
        definitions: &'a [Definition],
        closure: &'a BTreeSet<String>,
    ) -> impl Iterator<Item = &'a Definition> {
        let keep = self.demands_coverage();
        definitions.iter().filter(move |definition| keep && in_closure(&definition.path, closure))
    }

    /// The advisory note a discounted anchor carries into the lane's evidence,
    /// so a dropped demand is stated rather than silently absent.
    pub(super) fn note(&self, symbol: &str) -> Option<String> {
        match self {
            Self::SurfaceLocal { homonyms } => (!homonyms.is_empty()).then(|| {
                format!(
                    "`{symbol}` also defines in {} crates outside the declared surface's package-graph closure ({}): \
                     those definitions share a name, not an identity, so they make no coverage demand.",
                    homonyms.len(),
                    homonyms.join(", "),
                )
            }),
            Self::Foreign { crates } => Some(format!(
                "`{symbol}` defines nothing inside the declared surface ({} defines it): the plan names the word \
                 rather than claiming the code, so its definitions make no coverage demand.",
                crates.join(", "),
            )),
            Self::Broad { crates } => Some(format!(
                "`{symbol}` also defines in {} foreign crates ({}), at or past the {FOREIGN_CRATE_SPREAD_LIMIT}-crate \
                 spread limit: it reads as a common word rather than one identity, so its definitions make no \
                 coverage demand.",
                crates.len(),
                crates.join(", "),
            )),
        }
    }
}

/// The reverse-dependency closure of the workspace crates the declared surface
/// names. Patterns that do not belong to a workspace member (`Cargo.lock`,
/// `docs/guide/**`) contribute nothing.
pub(super) fn surface_closure(surface: &[String]) -> Result<BTreeSet<String>> {
    let workspace = Workspace::load()?;
    let members = workspace.members();
    let crates: BTreeSet<String> =
        surface.iter().map(|pattern| crate_label(pattern)).filter(|label| members.contains(label)).collect();
    workspace.reverse_closure_of(&crates)
}

/// Decide whether an anchor's definitions make a coverage demand.
///
/// An anchor the search resolved nothing for is neither: it has no definition
/// to demand coverage of and no spread to read as a common word, and the
/// verifier already reports it as unresolvable, so it is left alone.
///
/// `closure` is the reverse-dependency closure of the declared surface's
/// crates. In the mixed case — the surface admits a definition, and the name
/// also resolves elsewhere — definitions whose crate sits outside that
/// closure are homonyms and are discounted before [`FOREIGN_CRATE_SPREAD_LIMIT`]
/// runs on what remains.
pub(super) fn calibrate(definitions: &[Definition], closure: &BTreeSet<String>) -> Anchor {
    if definitions.is_empty() {
        return Anchor::SurfaceLocal { homonyms: Vec::new() };
    }

    if !definitions.iter().any(|definition| definition.covered) {
        return Anchor::Foreign { crates: foreign_crates(definitions) };
    }

    let remaining_foreign = foreign_crates_in(definitions, |path| in_closure(path, closure));
    let homonyms = foreign_crates_in(definitions, |path| !in_closure(path, closure));
    if remaining_foreign.len() >= FOREIGN_CRATE_SPREAD_LIMIT {
        return Anchor::Broad { crates: remaining_foreign };
    }
    Anchor::SurfaceLocal { homonyms }
}

fn in_closure(path: &str, closure: &BTreeSet<String>) -> bool {
    closure.contains(&crate_label(path))
}

/// The distinct crates of the definitions the surface does not admit, in search
/// order so the note reads in the order the search reported.
fn foreign_crates(definitions: &[Definition]) -> Vec<String> {
    foreign_crates_in(definitions, |_| true)
}

fn foreign_crates_in(definitions: &[Definition], admit: impl Fn(&str) -> bool) -> Vec<String> {
    let mut crates: Vec<String> = Vec::new();
    for definition in definitions.iter().filter(|definition| !definition.covered && admit(&definition.path)) {
        let label = crate_label(&definition.path);
        if !crates.contains(&label) {
            crates.push(label);
        }
    }
    crates
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use aether_bloomery::{
        NamedPath, NamedSymbol, PathOrigin, SCOPE_VERIFY_SCHEMA, ScopeVerifyInput, ScopeVerifyReport, verify_scope,
    };

    use super::{Anchor, Definition, calibrate};

    fn definition(path: &str, covered: bool) -> Definition {
        Definition { path: path.to_owned(), covered }
    }

    fn crates(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn local() -> Anchor {
        Anchor::SurfaceLocal { homonyms: Vec::new() }
    }

    #[test]
    fn a_common_word_defined_nowhere_in_the_surface_makes_no_demand() {
        // Reconstructs the measured class: a plan about the bloomery chassis
        // backticks `truncate`, whose definitions live in three crates the work
        // never touches, and the run is refused after all its authoring.
        let anchor = calibrate(
            &[
                definition("crates/aether-actor/src/log.rs", false),
                definition("crates/aether-bloomery-console/src/screen/transcript/mod.rs", false),
                definition("crates/aether-math/src/color.rs", false),
                definition("crates/aether-math/src/vec.rs", false),
            ],
            &crates(&[]),
        );

        let Anchor::Foreign { crates } = &anchor else {
            panic!("a name the surface defines nowhere is a foreign anchor: {anchor:?}");
        };
        assert_eq!(crates.join(", "), "aether-actor, aether-bloomery-console, aether-math");
        assert!(!anchor.demands_coverage());
        let note = anchor.note("truncate").expect("a discounted anchor states why");
        assert!(note.contains("`truncate`"), "{note}");
        assert!(note.contains("aether-math"), "the note names the crates that do define it: {note}");
    }

    #[test]
    fn one_foreign_crate_is_discounted_too_when_the_surface_admits_no_definition() {
        // Tripwire: calibrating on crate spread alone would keep this demand,
        // and `notify` — two definitions in one foreign crate — is half the
        // measured failure class. The surface, not the spread, is the rule.
        let anchor = calibrate(
            &[
                definition("crates/aether-substrate/src/mail/registry/effect.rs", false),
                definition("crates/aether-substrate/src/scheduler/spin_park.rs", false),
            ],
            &crates(&[]),
        );

        assert_eq!(anchor, Anchor::Foreign { crates: vec!["aether-substrate".to_owned()] });
        assert!(!anchor.demands_coverage());
    }

    #[test]
    fn a_surface_local_anchor_keeps_its_demand_on_the_definitions_outside() {
        // Tripwire (ADR-0208's own example): `adopt_candidate` defines in three
        // crates, the surface names one, and the two impls a signature change
        // must touch are exactly what the search exists to demand. Discounting
        // this shape would leave the check with nothing to catch.
        let definitions = [
            definition("crates/aether-bloomery/src/port/source.rs", true),
            definition("crates/aether-bloomery-git/src/source.rs", false),
            definition("crates/aether-chassis-bloomery/src/bloomery/source.rs", false),
        ];
        let closure = crates(&["aether-bloomery", "aether-bloomery-git", "aether-chassis-bloomery"]);
        let anchor = calibrate(&definitions, &closure);

        assert_eq!(anchor, local());
        assert!(anchor.demands_coverage());
        assert!(anchor.note("adopt_candidate").is_none(), "a kept anchor carries no advisory note");
        let demanding: Vec<&str> =
            anchor.demanding(&definitions, &closure).map(|definition| definition.path.as_str()).collect();
        assert_eq!(
            demanding,
            [
                "crates/aether-bloomery/src/port/source.rs",
                "crates/aether-bloomery-git/src/source.rs",
                "crates/aether-chassis-bloomery/src/bloomery/source.rs",
            ],
        );
        assert!(
            freeze("adopt_candidate", &definitions, &closure, &["crates/aether-bloomery/src/**"]).refused(),
            "the two impls a signature change must touch still refuse a surface that names only one crate",
        );
    }

    #[test]
    fn the_spread_limit_discounts_only_past_its_boundary() {
        // The mixed case: a name the surface does define, that also defines
        // itself across foreign crates. Two foreign crates is the widest
        // genuine anchor measured here, so the boundary must sit above it.
        // The foreign crates sit inside the closure so spread, not homonym
        // discount, is what fires.
        let inside = definition("xtask/src/bloom/mod.rs", true);
        let in_closure = crates(&["xtask", "aether-bloomery-git", "aether-chassis-bloomery", "aether-math"]);
        let two_foreign = calibrate(
            &[
                inside.clone(),
                definition("crates/aether-bloomery-git/src/testing.rs", false),
                definition("crates/aether-chassis-bloomery/src/bloomery/doctor/invariants.rs", false),
            ],
            &in_closure,
        );
        assert_eq!(two_foreign, local(), "two foreign crates is inside the limit");

        let three_foreign = calibrate(
            &[
                inside,
                definition("crates/aether-bloomery-git/src/testing.rs", false),
                definition("crates/aether-chassis-bloomery/src/bloomery/doctor/invariants.rs", false),
                definition("crates/aether-math/src/color.rs", false),
            ],
            &in_closure,
        );
        let Anchor::Broad { crates } = &three_foreign else {
            panic!("three foreign crates is past the limit: {three_foreign:?}");
        };
        assert_eq!(crates.len(), 3);
        assert!(!three_foreign.demands_coverage());
        assert!(three_foreign.note("hex_of").expect("a discounted anchor states why").contains("spread limit"));
    }

    #[test]
    fn out_of_closure_crates_do_not_count_toward_the_spread_limit() {
        // The same three-foreign shape as the spread-limit test, but the
        // foreign crates sit outside the surface's closure: they are homonyms,
        // not spread, so the in-surface definition keeps its demand.
        let definitions = [
            definition("xtask/src/bloom/mod.rs", true),
            definition("crates/aether-bloomery-git/src/testing.rs", false),
            definition("crates/aether-chassis-bloomery/src/bloomery/doctor/invariants.rs", false),
            definition("crates/aether-math/src/color.rs", false),
        ];
        let closure = crates(&["xtask"]);
        let anchor = calibrate(&definitions, &closure);
        let Anchor::SurfaceLocal { homonyms } = &anchor else {
            panic!("out-of-closure foreign crates are homonyms, not spread: {anchor:?}");
        };
        assert_eq!(homonyms.len(), 3);
        assert!(anchor.demands_coverage());
        assert_eq!(anchor.demanding(&definitions, &closure).count(), 1);
    }

    #[test]
    fn an_anchor_the_search_resolved_nothing_for_is_left_alone() {
        // It has no definition to demand and no spread to read as a word; the
        // verifier's unresolvable bucket is what reports it.
        let anchor = calibrate(&[], &crates(&[]));

        assert_eq!(anchor, local());
        assert!(anchor.note("never_written").is_none(), "an empty search is not a generic-word finding");
    }

    #[test]
    fn a_definition_inside_a_multi_crate_surface_counts_wherever_it_sits() {
        // The covered flag is the surface's answer, not a crate-name
        // comparison: a surface naming two crates keeps an anchor whose only
        // admitted definition sits in the second.
        let anchor = calibrate(
            &[
                definition("crates/aether-bloomery-git/src/source.rs", false),
                definition("crates/aether-chassis-bloomery/src/bloomery/source.rs", true),
            ],
            &crates(&["aether-bloomery-git", "aether-chassis-bloomery"]),
        );

        assert_eq!(anchor, local());
    }

    #[test]
    fn measured_homonym_refusals_freeze_once_out_of_closure_defs_are_discounted() {
        // The five 2026-09-14 inverse-search refusals: each word is defined
        // inside the surface and also, as an unrelated item of the same name,
        // in a crate the work never touches. One or two foreign crates sit
        // under the spread limit, so spread alone keeps the demand. Discounting
        // the out-of-closure homonym is what lets the freeze through.
        let bloomery = crates(&["aether-bloomery", "aether-chassis-bloomery", "aether-harness-bloomery"]);
        let console = crates(&["aether-bloomery-console"]);
        let chassis = crates(&["aether-chassis-bloomery", "aether-harness-bloomery"]);
        let github = crates(&["aether-bloomery-github"]);

        assert_homonym_freezes(
            "Pending",
            &[
                definition("crates/aether-chassis-bloomery/src/benchmark/kinds.rs", true),
                definition("crates/aether-substrate/src/actor/native/offload/blocking.rs", false),
            ],
            &bloomery,
            &["crates/aether-chassis-bloomery/src/**"],
        );
        assert_homonym_freezes(
            "caret",
            &[
                definition("crates/aether-bloomery-console/src/screen/mod.rs", true),
                definition("crates/aether-kit-widget/src/text_edit.rs", false),
            ],
            &console,
            &["crates/aether-bloomery-console/**"],
        );
        assert_homonym_freezes(
            "Journal",
            &[
                definition("crates/aether-bloomery-console/src/dto/mod.rs", true),
                definition("crates/aether-bloomery/tests/calibration.rs", false),
                definition("crates/aether-bloomery/tests/metrics.rs", false),
            ],
            &console,
            &["crates/aether-bloomery-console/src/**"],
        );
        assert_homonym_freezes(
            "from_config",
            &[
                definition("crates/aether-chassis-bloomery/src/bloomery/config.rs", true),
                definition("crates/aether-kit-widget/src/set/numeric.rs", false),
                definition("crates/aether-kit-widget/src/set/virtual_list/mod.rs", false),
            ],
            &chassis,
            &["crates/aether-chassis-bloomery/src/**"],
        );
        assert_homonym_freezes(
            "MemberView",
            &[
                definition("crates/aether-bloomery-github/src/lib.rs", true),
                definition("crates/aether-bloomery-console/src/dto/mod.rs", false),
            ],
            &github,
            &["crates/aether-bloomery-github/src/**"],
        );
    }

    fn assert_homonym_freezes(symbol: &str, definitions: &[Definition], closure: &BTreeSet<String>, surface: &[&str]) {
        let report = freeze(symbol, definitions, closure, surface);
        assert!(!report.refused(), "{symbol} still refused after homonym discount: {report:?}");
        let anchor = calibrate(definitions, closure);
        let Anchor::SurfaceLocal { homonyms } = &anchor else {
            panic!("{symbol} should remain a surface-local claim: {anchor:?}");
        };
        assert!(!homonyms.is_empty(), "{symbol} must record the discounted crate: {anchor:?}");
        let note = anchor.note(symbol).expect("a discounted homonym states why");
        assert!(note.contains("package-graph closure"), "{symbol} note: {note}");
    }

    fn freeze(
        symbol: &str,
        definitions: &[Definition],
        closure: &BTreeSet<String>,
        surface: &[&str],
    ) -> ScopeVerifyReport {
        let anchor = calibrate(definitions, closure);
        verify_scope(&ScopeVerifyInput {
            schema: SCOPE_VERIFY_SCHEMA,
            named_paths: anchor
                .demanding(definitions, closure)
                .map(|definition| NamedPath {
                    path: definition.path.clone(),
                    origin: PathOrigin::InverseSearch { symbol: symbol.to_owned() },
                })
                .collect(),
            named_symbols: vec![NamedSymbol {
                symbol: symbol.to_owned(),
                definitions: definitions.iter().map(|definition| definition.path.clone()).collect(),
            }],
            declared_surface: surface.iter().map(|pattern| (*pattern).to_owned()).collect(),
        })
    }
}
