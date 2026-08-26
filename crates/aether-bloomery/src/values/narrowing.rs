//! Which candidates a narrower composition is over, and what it is allowed
//! to touch (ADR-0210).
//!
//! Two members construct in parallel against one sealed base, each verifies
//! green alone, and the tree that holds both refuses to build. The failure
//! belongs to neither candidate — it belongs to the pair — and until now the
//! estate had nowhere to put it, so it charged whichever member happened to be
//! verified on the fold first. That member then declined to repair code it
//! never wrote, correctly, and the bloom stopped.
//!
//! Narrowing is the first half of the answer: read the failing diagnostic's
//! paths against what each candidate actually changed, and name the candidates
//! whose work has to coexist. The second half is the composition the reducer
//! dispatches over exactly those parents, which repairs their coexistence
//! without touching either of them.
//!
//! Everything in this module is a pure read over values the host already holds.
//! The host extracts the diagnostic paths and the per-candidate changed sets;
//! the judgement about whether those add up to a narrower composition, and about
//! what that composition may edit, is here so the reducer and the classifier
//! cannot drift.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::approval::surface_union;
use super::path_in_surface;
use crate::ids::WorkpieceId;

/// One member's standing in the fold under judgement.
///
/// `changed` is the member's candidate diffed against the bloom's sealed base —
/// the same read the containment gate takes — so a path appearing here is a path
/// this member wrote. `surface` is what that member was approved at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FoldContribution {
    /// The member.
    pub workpiece: WorkpieceId,
    /// Repository-relative paths its candidate changed against the sealed base.
    pub changed: Vec<String>,
    /// Its sealed declared surface.
    pub surface: Vec<String>,
}

/// The candidates a narrowed composition is over, and the bound it runs under.
///
/// `bound` is [`surface_union`] over exactly those parents' declared surfaces —
/// derived, never signed. It grants no path no approval already covered: each
/// glob in it is a surface some parent's own signed revision carries, and every
/// parent was admitted under the same bloom-wide policy, so the tier ladder was
/// satisfied for every path here before the collision existed. The composition's
/// tier is therefore the strictest parent's, which is what
/// [`ApprovalPolicy::resolve_surface`](super::ApprovalPolicy::resolve_surface)
/// answers over `bound` directly — most-restrictive-wins over a union is the
/// maximum over its operands.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CompositionParents {
    /// The parents, in canonical id order — the composition's arity is this
    /// list's length. [`narrow_composition`] names one parent when a single
    /// candidate covers the paths, two when a pair does, and refuses every
    /// other count rather than naming a parent set that is a guess. The
    /// whole-bloom instance reads its parents off the bloom rather than through
    /// this value.
    pub parents: Vec<WorkpieceId>,
    /// The diagnostic paths the collision was read off, sorted and deduplicated.
    pub paths: Vec<String>,
    /// The union of the parents' declared surfaces — what the composition may
    /// edit and nothing more.
    pub bound: Vec<String>,
}

impl CompositionParents {
    /// The composition this parent set names.
    ///
    /// [`None`] only for an empty list: [`WorkpieceId::composition_of`] already
    /// spells an id at any non-empty arity, and an empty parent set is not a
    /// collision to repair.
    #[must_use]
    pub fn workpiece(&self) -> Option<WorkpieceId> {
        if self.parents.is_empty() {
            None
        } else {
            Some(WorkpieceId::composition_of(&self.parents))
        }
    }
}

/// Why a failing fold does not narrow to a composition.
///
/// Every arm is a reason to leave the failure where it already was rather than
/// narrow: a parent set that guesses is worse than none, because the composition
/// it names would be handed candidates with nothing to do with the defect and
/// told to make them coexist.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NarrowingRefusal {
    /// The verdict named no paths, so there is nothing to attribute.
    NoDiagnosticPaths,
    /// The member under verification is itself one of the writers. Its own
    /// Verify is the right place for that, and re-routing it to a composition
    /// would hide a defect the member owns.
    VerifiedMemberWrote(WorkpieceId),
    /// No pair of candidates accounts for the named paths — a path nobody in
    /// this bloom wrote, or a collision wider than two. Find the real owner
    /// before inventing one. A single candidate that covers the paths is not
    /// this: that names a one-parent composition.
    NoCoveringPair,
    /// A named path sits outside the union of the parents' surfaces, so the
    /// composition could not legally edit the file the diagnostic points at. A
    /// collision the parents' approvals do not reach is not their collision.
    OutsideTheUnion(Vec<String>),
}

/// Narrow a failing fold to the candidates that collide on it, or refuse to
/// narrow it (ADR-0210).
///
/// `verified` is the member whose Verify produced the verdict — the one this
/// exists to leave untouched. `paths` are the repository-relative paths the
/// diagnostic named. `contributions` is every member of the bloom whose
/// candidate is in the tree under test.
///
/// A single candidate that covers every named path names a one-parent
/// composition over that candidate, under that candidate's own surface, rather
/// than being padded out to a pair. The pair is found by exhaustive search over
/// pairs rather than by a general minimum-set-cover: only a cover of size one
/// or two narrows anything, so checking every pair is both exact and cheap.
/// That is `O(m² · p)` membership tests over a membership the seal door caps
/// and a diagnostic path list a verdict bounds — tens of members against tens
/// of paths, evaluated once per failing fold, not per file. Ties among covering
/// pairs resolve to the canonically first, so the same failing fold attributes
/// the same way however the membership was ordered. Three-or-more stays
/// [`NarrowingRefusal::NoCoveringPair`]: generalizing to arbitrary subsets is a
/// cover problem whose answer is not unique.
///
/// # Errors
/// A [`NarrowingRefusal`] naming why this fold does not narrow to a parent set.
pub fn narrow_composition(
    verified: &WorkpieceId,
    paths: &[String],
    contributions: &[FoldContribution],
) -> Result<CompositionParents, NarrowingRefusal> {
    let mut paths: Vec<String> = paths.to_vec();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Err(NarrowingRefusal::NoDiagnosticPaths);
    }

    if let Some(writer) = contributions.iter().find(|entry| entry.workpiece == *verified && wrote_any(entry, &paths)) {
        return Err(NarrowingRefusal::VerifiedMemberWrote(writer.workpiece.clone()));
    }
    if let Some(sole) = contributions.iter().find(|entry| covers(entry, &paths)) {
        let bound = surface_union(&[sole.surface.as_slice()]);
        let outside: Vec<String> =
            paths.iter().filter(|path| !path_in_surface(&bound, path)).map(String::clone).collect();
        if !outside.is_empty() {
            return Err(NarrowingRefusal::OutsideTheUnion(outside));
        }
        return Ok(CompositionParents { parents: alloc::vec![sole.workpiece.clone()], paths, bound });
    }

    let mut ordered: Vec<&FoldContribution> =
        contributions.iter().filter(|entry| entry.workpiece != *verified && wrote_any(entry, &paths)).collect();
    ordered.sort_by(|left, right| left.workpiece.cmp(&right.workpiece));

    let pair = ordered
        .iter()
        .enumerate()
        .flat_map(|(index, first)| ordered[index + 1..].iter().map(move |second| (*first, *second)))
        .find(|(first, second)| paths.iter().all(|path| wrote(first, path) || wrote(second, path)))
        .ok_or(NarrowingRefusal::NoCoveringPair)?;

    let bound = surface_union(&[pair.0.surface.as_slice(), pair.1.surface.as_slice()]);
    let outside: Vec<String> = paths.iter().filter(|path| !path_in_surface(&bound, path)).map(String::clone).collect();
    if !outside.is_empty() {
        return Err(NarrowingRefusal::OutsideTheUnion(outside));
    }

    let (low, high) = if pair.0.workpiece <= pair.1.workpiece {
        (pair.0, pair.1)
    } else {
        (pair.1, pair.0)
    };
    Ok(CompositionParents { parents: alloc::vec![low.workpiece.clone(), high.workpiece.clone()], paths, bound })
}

/// Whether `entry`'s candidate changed `path`.
fn wrote(entry: &FoldContribution, path: &str) -> bool {
    entry.changed.iter().any(|changed| changed == path)
}

/// Whether `entry`'s candidate changed any of `paths`.
fn wrote_any(entry: &FoldContribution, paths: &[String]) -> bool {
    paths.iter().any(|path| wrote(entry, path))
}

/// Whether `entry`'s candidate changed every one of `paths` on its own.
fn covers(entry: &FoldContribution, paths: &[String]) -> bool {
    paths.iter().all(|path| wrote(entry, path))
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString as _};
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{FoldContribution, NarrowingRefusal, narrow_composition};
    use crate::ids::WorkpieceId;

    /// The two files the night's `E0599` named: the use site the first member
    /// added, and the type definition the second retyped under it.
    const USE_SITE: &str = "xtask/src/transform/verify/mod.rs";
    const DEFINITION: &str = "xtask/src/transform/mod.rs";

    fn workpiece(name: &str) -> WorkpieceId {
        WorkpieceId(name.to_string())
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn contribution(name: &str, changed: &[&str], surface: &[&str]) -> FoldContribution {
        FoldContribution { workpiece: workpiece(name), changed: strings(changed), surface: strings(surface) }
    }

    /// The night this exists for: 5344 added a test calling `.contains()` in
    /// `xtask`, 5346 collapsed the value it calls into an `EvidenceChannel`
    /// enum, each verified green alone, and 5417 — a console member that
    /// touches neither file — was the first verified on the fold of both.
    fn the_night() -> Vec<FoldContribution> {
        vec![
            contribution("issue-5344", &[USE_SITE], &["xtask/**"]),
            contribution(
                "issue-5346",
                &[DEFINITION, "crates/aether-bloomery/src/values/proof.rs"],
                &["crates/aether-bloomery/**", "xtask/**"],
            ),
            contribution(
                "issue-5417",
                &["crates/aether-bloomery-console/src/screen/detail.rs"],
                &["crates/aether-bloomery-console/**"],
            ),
        ]
    }

    /// The paths the night's `E0599` named: the `-->` use site and the `:::`
    /// definition note. Both halves are what makes the collision attributable —
    /// a diagnostic that named only one file names only one writer.
    fn the_diagnostic() -> Vec<String> {
        strings(&[USE_SITE, DEFINITION])
    }

    #[test]
    fn a_collision_between_two_members_names_them_and_not_the_member_being_verified() {
        // Tripwire: the whole defect. The verdict arrives on 5417's Verify, so
        // every member-shaped lever the reducer holds points at 5417 — and 5417
        // wrote none of the failing file. An attribution that lands on it costs
        // a repair lap the member will decline and stops the bloom.
        let attribution = narrow_composition(&workpiece("issue-5417"), &the_diagnostic(), &the_night())
            .expect("two candidates account for the failing path");

        assert_eq!(attribution.parents, vec![workpiece("issue-5344"), workpiece("issue-5346")]);
        assert!(
            !attribution.parents.contains(&workpiece("issue-5417")),
            "the member that happened to verify the fold is not a parent of the collision",
        );
    }

    #[test]
    fn the_bound_is_the_two_parents_surfaces_and_nothing_wider() {
        // Tripwire: the bound is what makes the repair legal without a new
        // signature — every glob in it is a surface one parent was approved at.
        // Folding in a third member's surface, or the bloom's whole membership,
        // would grant the repair paths no approval on this collision covers.
        let attribution = narrow_composition(&workpiece("issue-5417"), &the_diagnostic(), &the_night())
            .expect("the collision attributes");

        assert_eq!(attribution.bound, strings(&["crates/aether-bloomery/**", "xtask/**"]));
        assert!(
            !attribution.bound.iter().any(|glob| glob.contains("console")),
            "the verified member's surface is not part of the bound: {:?}",
            attribution.bound,
        );
    }

    #[test]
    fn the_minted_subject_is_the_same_whichever_parent_is_named_first() {
        // Tripwire: a second refusal of the same pair has to land on the
        // workpiece already repairing it. An id that carried the parents in
        // discovery order would mint a second subject and set two lanes on one
        // seam.
        let forward = narrow_composition(&workpiece("issue-5417"), &the_diagnostic(), &the_night())
            .expect("the collision attributes");
        let mut reversed = the_night();
        reversed.reverse();
        let backward = narrow_composition(&workpiece("issue-5417"), &the_diagnostic(), &reversed)
            .expect("the collision attributes");

        assert_eq!(forward.workpiece(), backward.workpiece());
        assert!(forward.workpiece().expect("a two-parent attribution mints a subject").is_composition());
    }

    #[test]
    fn a_path_one_other_member_owns_narrows_to_the_composition_over_that_member() {
        // Tripwire: the arity this finding is about. The diagnostic names a
        // path one sibling wrote and the member under verification did not.
        // Charging the verified member parks it awaiting a surface it was
        // never approved for; charging the owner reopens reviewed work. The
        // composition over that one candidate is the subject that exists to
        // make a candidate coexist with the tree it landed in.
        let attribution = narrow_composition(
            &workpiece("issue-5417"),
            &strings(&["crates/aether-bloomery/src/values/proof.rs"]),
            &the_night(),
        )
        .expect("one other candidate accounts for every named path");

        assert_eq!(attribution.parents, vec![workpiece("issue-5346")]);
        assert_eq!(attribution.bound, strings(&["crates/aether-bloomery/**", "xtask/**"]));
        assert_eq!(
            attribution.workpiece(),
            Some(WorkpieceId::composition_of(&[workpiece("issue-5346")])),
            "a one-parent attribution still mints a composition, not the owner",
        );
        assert!(
            !attribution.parents.contains(&workpiece("issue-5417")),
            "the member that happened to verify the fold is not a parent",
        );
    }

    #[test]
    fn a_verified_member_that_wrote_the_failing_path_keeps_its_own_finding() {
        // The guard against the mint becoming a way for a member to launder its
        // own defect onto a synthetic: if the member under test wrote the file
        // the diagnostic names, its own Verify is where that belongs.
        assert_eq!(
            narrow_composition(&workpiece("issue-5344"), &the_diagnostic(), &the_night()),
            Err(NarrowingRefusal::VerifiedMemberWrote(workpiece("issue-5344"))),
        );
    }

    #[test]
    fn a_path_no_pair_accounts_for_is_refused_rather_than_attributed() {
        assert_eq!(
            narrow_composition(&workpiece("issue-5417"), &strings(&["crates/aether-render/src/pass.rs"]), &the_night()),
            Err(NarrowingRefusal::NoCoveringPair),
        );
        assert_eq!(
            narrow_composition(&workpiece("issue-5417"), &[], &the_night()),
            Err(NarrowingRefusal::NoDiagnosticPaths),
        );
    }

    #[test]
    fn a_path_outside_the_pairs_union_refuses_the_mint() {
        // Tripwire: the bound is derived from surfaces, and the repair is
        // contained against it like any candidate. Minting a subject whose
        // objective sits outside its own bound would produce a lane that
        // cannot legally do the only thing it was minted for.
        let contributions = vec![
            contribution("issue-a", &["Cargo.lock"], &["crates/example-a/**"]),
            contribution("issue-b", &["Cargo.lock"], &["crates/example-b/**"]),
        ];

        assert_eq!(
            narrow_composition(&workpiece("issue-c"), &strings(&["Cargo.lock"]), &contributions),
            Err(NarrowingRefusal::OutsideTheUnion(strings(&["Cargo.lock"]))),
        );
        assert_eq!(
            narrow_composition(
                &workpiece("issue-c"),
                &strings(&["Cargo.lock", "crates/example-a/src/lib.rs"]),
                &[
                    contribution("issue-a", &["Cargo.lock"], &["crates/example-a/**"]),
                    contribution("issue-b", &["crates/example-a/src/lib.rs"], &["crates/example-b/**"]),
                ]
            ),
            Err(NarrowingRefusal::OutsideTheUnion(strings(&["Cargo.lock"]))),
        );
    }
}
