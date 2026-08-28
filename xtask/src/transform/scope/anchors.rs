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
//! load-bearing half of the rule is the surface-admits-a-definition test and
//! [`FOREIGN_CRATE_SPREAD_LIMIT`] is only the second guard, for the mixed case
//! where a common word happens to also be defined inside the surface.
//!
//! A discounted anchor is dropped from the refusing population, never from the
//! report: it stays in the projection's `named_symbols`, so the verifier still
//! classifies it into an advisory bucket, and the lane stamps the calibration's
//! own note beside the evidence.

use crate::symbols::references::crate_label;

/// How many distinct foreign crates a surface-local anchor's definitions may
/// spread across before the anchor reads as a common word rather than one
/// identity.
///
/// Three, because two is where the widest genuine anchor measured on this
/// workspace sits: `adopt_candidate` defines in `aether-bloomery`,
/// `aether-bloomery-git` and `aether-chassis-bloomery`, so a surface naming one
/// of the three leaves two foreign crates and must keep its demand. This value
/// sits one above that ceiling. It is not tuned against a refusal count, and it
/// is deliberately the weaker half of the rule: an anchor with no definition
/// inside the surface is discounted at any spread.
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
    /// The surface admits at least one definition and the rest stay inside the
    /// spread limit: a claim about this work, so every defining path keeps its
    /// coverage demand.
    SurfaceLocal,
    /// The surface admits no definition at all. The plan mentions the name; it
    /// does not claim the code.
    Foreign {
        /// The crates that do define it, in search order.
        crates: Vec<String>,
    },
    /// The surface admits a definition, and the anchor also resolves across
    /// enough foreign crates to read as a common word.
    Broad {
        /// The foreign crates that also define it, in search order.
        crates: Vec<String>,
    },
}

impl Anchor {
    /// Whether this anchor's defining paths enter the refusing population.
    pub(super) fn demands_coverage(&self) -> bool {
        matches!(self, Self::SurfaceLocal)
    }

    /// The advisory note a discounted anchor carries into the lane's evidence,
    /// so a dropped demand is stated rather than silently absent.
    pub(super) fn note(&self, symbol: &str) -> Option<String> {
        match self {
            Self::SurfaceLocal => None,
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

/// Decide whether an anchor's definitions make a coverage demand.
///
/// An anchor the search resolved nothing for is neither: it has no definition
/// to demand coverage of and no spread to read as a common word, and the
/// verifier already reports it as unresolvable, so it is left alone.
pub(super) fn calibrate(definitions: &[Definition]) -> Anchor {
    if definitions.is_empty() {
        return Anchor::SurfaceLocal;
    }

    let foreign = foreign_crates(definitions);
    if !definitions.iter().any(|definition| definition.covered) {
        return Anchor::Foreign { crates: foreign };
    }
    if foreign.len() >= FOREIGN_CRATE_SPREAD_LIMIT {
        return Anchor::Broad { crates: foreign };
    }
    Anchor::SurfaceLocal
}

/// The distinct crates of the definitions the surface does not admit, in search
/// order so the note reads in the order the search reported.
fn foreign_crates(definitions: &[Definition]) -> Vec<String> {
    let mut crates: Vec<String> = Vec::new();
    for definition in definitions.iter().filter(|definition| !definition.covered) {
        let label = crate_label(&definition.path);
        if !crates.contains(&label) {
            crates.push(label);
        }
    }
    crates
}

#[cfg(test)]
mod tests {
    use super::{Anchor, Definition, calibrate};

    fn definition(path: &str, covered: bool) -> Definition {
        Definition { path: path.to_owned(), covered }
    }

    #[test]
    fn a_common_word_defined_nowhere_in_the_surface_makes_no_demand() {
        // Reconstructs the measured class: a plan about the bloomery chassis
        // backticks `truncate`, whose definitions live in three crates the work
        // never touches, and the run is refused after all its authoring.
        let anchor = calibrate(&[
            definition("crates/aether-actor/src/log.rs", false),
            definition("crates/aether-bloomery-console/src/screen/transcript/mod.rs", false),
            definition("crates/aether-math/src/color.rs", false),
            definition("crates/aether-math/src/vec.rs", false),
        ]);

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
        let anchor = calibrate(&[
            definition("crates/aether-substrate/src/mail/registry/effect.rs", false),
            definition("crates/aether-substrate/src/scheduler/spin_park.rs", false),
        ]);

        assert_eq!(anchor, Anchor::Foreign { crates: vec!["aether-substrate".to_owned()] });
        assert!(!anchor.demands_coverage());
    }

    #[test]
    fn a_surface_local_anchor_keeps_its_demand_on_the_definitions_outside() {
        // Tripwire (ADR-0208's own example): `adopt_candidate` defines in three
        // crates, the surface names one, and the two impls a signature change
        // must touch are exactly what the search exists to demand. Discounting
        // this shape would leave the check with nothing to catch.
        let anchor = calibrate(&[
            definition("crates/aether-bloomery/src/port/source.rs", true),
            definition("crates/aether-bloomery-git/src/source.rs", false),
            definition("crates/aether-chassis-bloomery/src/bloomery/source.rs", false),
        ]);

        assert_eq!(anchor, Anchor::SurfaceLocal);
        assert!(anchor.demands_coverage());
        assert!(anchor.note("adopt_candidate").is_none(), "a kept anchor carries no advisory note");
    }

    #[test]
    fn the_spread_limit_discounts_only_past_its_boundary() {
        // The mixed case: a name the surface does define, that also defines
        // itself across foreign crates. Two foreign crates is the widest
        // genuine anchor measured here, so the boundary must sit above it.
        let inside = definition("xtask/src/bloom/mod.rs", true);
        let two_foreign = calibrate(&[
            inside.clone(),
            definition("crates/aether-bloomery-git/src/testing.rs", false),
            definition("crates/aether-chassis-bloomery/src/bloomery/doctor/invariants.rs", false),
        ]);
        assert_eq!(two_foreign, Anchor::SurfaceLocal, "two foreign crates is inside the limit");

        let three_foreign = calibrate(&[
            inside,
            definition("crates/aether-bloomery-git/src/testing.rs", false),
            definition("crates/aether-chassis-bloomery/src/bloomery/doctor/invariants.rs", false),
            definition("crates/aether-math/src/color.rs", false),
        ]);
        let Anchor::Broad { crates } = &three_foreign else {
            panic!("three foreign crates is past the limit: {three_foreign:?}");
        };
        assert_eq!(crates.len(), 3);
        assert!(!three_foreign.demands_coverage());
        assert!(three_foreign.note("hex_of").expect("a discounted anchor states why").contains("spread limit"));
    }

    #[test]
    fn an_anchor_the_search_resolved_nothing_for_is_left_alone() {
        // It has no definition to demand and no spread to read as a word; the
        // verifier's unresolvable bucket is what reports it.
        let anchor = calibrate(&[]);

        assert_eq!(anchor, Anchor::SurfaceLocal);
        assert!(anchor.note("never_written").is_none(), "an empty search is not a generic-word finding");
    }

    #[test]
    fn a_definition_inside_a_multi_crate_surface_counts_wherever_it_sits() {
        // The covered flag is the surface's answer, not a crate-name
        // comparison: a surface naming two crates keeps an anchor whose only
        // admitted definition sits in the second.
        let anchor = calibrate(&[
            definition("crates/aether-bloomery-git/src/source.rs", false),
            definition("crates/aether-chassis-bloomery/src/bloomery/source.rs", true),
        ]);

        assert_eq!(anchor, Anchor::SurfaceLocal);
    }
}
