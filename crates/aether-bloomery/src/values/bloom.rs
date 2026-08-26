//! The one-way bloom lifecycle (ADR-0149 §The bloom).
//!
//! ```text
//! BloomDraft   --seal-->   BloomSpec   --resolve-->   ResolvedBloom   --land-->   LandingReceipt
//! (mutable)                (immutable)                (one artifact)             (mainline moved)
//! ```
//!
//! The mutation happens only in the draft. [`BloomDraft::seal`] freezes a
//! draft into a canonically-ordered [`BloomSpec`] whose id is the digest of
//! its own bytes; there is no API to mutate a sealed spec.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::iter::once;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::{BloomId, WorkpieceId};
use crate::values::{ConfigRegistry, ConfigScopes, Evidence, Forecast, surface_intersection};

/// One workpiece's admission into a bloom: its identity, the exact scope
/// revision the bloom pins, and the approval evidence bound to that
/// revision.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Membership {
    /// The admitted workpiece.
    pub workpiece: WorkpieceId,
    /// The exact scope-revision digest sealed into the bloom.
    pub scope_revision: Digest,
    /// This member's configuration, resolved ahead of the bloom's (ADR-0174).
    /// Empty is the ordinary case — a member configures only what it wants to
    /// differ from the bloom-wide choice.
    pub configs: ConfigRegistry,
    /// The approval evidence, bound to this member's [`subject`](Self::subject).
    pub approval: Evidence,
}

/// The exact content a member's approval binds to (ADR-0174): its workpiece, the
/// scope revision it was approved at, and the configuration it will run under.
///
/// The binding covers configuration because a receipt that reads "approved" over
/// a model nobody approved is the same divergence a sealed-but-ignored digest
/// was. The override used to sit inside the scope revision, so a signature over
/// the revision covered it by accident; sealing it in the member's registry
/// instead makes that coverage something the subject has to state.
///
/// `approval` itself is excluded, since the subject is what the approval is
/// *of* — including it would be circular.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MemberSubject {
    /// The workpiece under approval.
    pub workpiece: WorkpieceId,
    /// The scope revision the approval was granted at.
    pub scope_revision: Digest,
    /// The configuration the member will run under.
    pub configs: ConfigRegistry,
}

impl ContentAddressed for MemberSubject {
    const DOMAIN: &'static str = "aether.bloomery.member_subject";
}

impl Membership {
    /// The digest this member's `approval` must bind — and, for an above-auto
    /// member, the exact bytes an author signs.
    #[must_use]
    pub fn subject(&self) -> Digest {
        digest_of(&MemberSubject {
            workpiece: self.workpiece.clone(),
            scope_revision: self.scope_revision,
            configs: self.configs.clone(),
        })
    }
}

/// One directed member-dependency edge (ADR-0196): `member` cannot start until
/// `depends_on` has resolved. The pair is the wire value the seal's effect
/// vocabulary journals — `Membership` does not carry it.
///
/// Dispatch gates on **declared** edges only (ADR-0204). A surface-derived
/// overlap edge still appears in [`ResolvedDependencies::edges`] so a
/// declared edge against that ordering is still a named cycle, but it does
/// not hold a member out of Construct.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct MemberDependency {
    /// The dependent workpiece.
    pub member: WorkpieceId,
    /// The workpiece it waits for.
    pub depends_on: WorkpieceId,
}

/// The door-resolved member graph, split by provenance (ADR-0204).
///
/// [`edges`](Self::edges) is the union: declared edges plus one ordering
/// edge per overlapping surface pair, in canonical workpiece order. Cycle
/// detection walks this set. [`declared`](Self::declared) is the authored
/// subset — the only edges the door journals and the reducer uses to gate
/// construct dispatch. Derived overlap edges are not dispatch gates;
/// integration and fold still read sealed member order, which is the same
/// later-depends-on-earlier sequence the derived edges named.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ResolvedDependencies {
    /// Declared ∪ derived, sorted and de-duplicated. Cycle-checked.
    pub edges: Vec<MemberDependency>,
    /// Authored edges only, sorted and de-duplicated. Dispatch gates.
    pub declared: Vec<MemberDependency>,
}

/// Why a seal's member-dependency graph was refused (ADR-0196).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DependencyError {
    /// An edge names a workpiece that is not a member of the bloom being sealed.
    UnknownWorkpiece(WorkpieceId),
    /// The resolved graph contains a cycle. The vec names the members on the
    /// cycle, walking from the back-edge target around to it again.
    Cycle(Vec<WorkpieceId>),
}

/// Union `declared` with one ordering edge per pair of `members` whose
/// declared surfaces intersect, in canonical `WorkpieceId` order; refuse a
/// cycle or an edge that names a non-member.
///
/// The later-canonical member of an overlapping pair depends on the earlier
/// one — the same leading key [`BloomDraft::seal`] sorts memberships on — so
/// two listings of the same set produce the same graph. Declared-edge
/// endpoints are still validated against the unsorted member set. Both
/// returned sets are sorted and de-duplicated so two seals that decide the
/// same graph journal the same bytes. Cycle detection walks the union:
/// a declared edge against the grain of a derived overlap is still a loop.
pub fn resolve_member_dependencies(
    members: &[(WorkpieceId, &[String])],
    declared: &[MemberDependency],
) -> Result<ResolvedDependencies, DependencyError> {
    let ids: BTreeSet<&WorkpieceId> = members.iter().map(|(id, _)| id).collect();
    let mut authored = BTreeSet::new();
    for edge in declared {
        if !ids.contains(&edge.member) {
            return Err(DependencyError::UnknownWorkpiece(edge.member.clone()));
        }
        if !ids.contains(&edge.depends_on) {
            return Err(DependencyError::UnknownWorkpiece(edge.depends_on.clone()));
        }
        authored.insert(edge.clone());
    }
    let mut edges = authored.clone();
    let mut ordered: Vec<_> = members.iter().collect();
    ordered.sort_by(|left, right| left.0.cmp(&right.0));
    for (index, (workpiece, surface)) in ordered.iter().enumerate() {
        for (peer, peer_surface) in &ordered[index + 1..] {
            if !surface_intersection(surface, peer_surface).is_empty() {
                edges.insert(MemberDependency { member: (*peer).clone(), depends_on: (*workpiece).clone() });
            }
        }
    }
    let edges: Vec<MemberDependency> = edges.into_iter().collect();
    if let Some(cycle) = dependency_cycle(&edges) {
        return Err(DependencyError::Cycle(cycle));
    }
    Ok(ResolvedDependencies { edges, declared: authored.into_iter().collect() })
}

const WHITE: u8 = 0;
const GRAY: u8 = 1;
const BLACK: u8 = 2;

/// The members of one directed cycle in `edges`, or `None` when the graph is
/// acyclic. Iterative: the stack is `(node, next-child index)` plus the path
/// of gray nodes, so a back edge reconstructs the loop without recursing.
fn dependency_cycle(edges: &[MemberDependency]) -> Option<Vec<WorkpieceId>> {
    let mut adj: BTreeMap<&WorkpieceId, Vec<&WorkpieceId>> = BTreeMap::new();
    let mut nodes = BTreeSet::new();
    for edge in edges {
        adj.entry(&edge.member).or_default().push(&edge.depends_on);
        nodes.insert(&edge.member);
        nodes.insert(&edge.depends_on);
    }

    let mut color: BTreeMap<&WorkpieceId, u8> = nodes.iter().copied().map(|id| (id, WHITE)).collect();

    for start in nodes {
        if color.get(start).copied() != Some(WHITE) {
            continue;
        }
        let mut stack = vec![(start, 0usize)];
        let mut path = vec![start];
        color.insert(start, GRAY);
        while let Some((node, child_idx)) = stack.pop() {
            let children = adj.get(node).map_or(&[][..], Vec::as_slice);
            if child_idx < children.len() {
                stack.push((node, child_idx + 1));
                let next = children[child_idx];
                match color.get(next).copied().unwrap_or(WHITE) {
                    GRAY => {
                        let start_at = path.iter().position(|id| *id == next).unwrap_or(0);
                        let mut cycle: Vec<WorkpieceId> = path[start_at..].iter().map(|id| (*id).clone()).collect();
                        cycle.push(next.clone());
                        return Some(cycle);
                    }
                    WHITE => {
                        color.insert(next, GRAY);
                        path.push(next);
                        stack.push((next, 0));
                    }
                    _ => {}
                }
            } else {
                color.insert(node, BLACK);
                path.pop();
            }
        }
    }
    None
}

/// A mutable bloom in shaping: membership proposals, base, and forecast.
/// Drafts overlap harmlessly and claim nothing (ADR-0149 §The bloom) — only
/// sealing takes claims.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct BloomDraft {
    /// The proposed memberships, in any order — [`seal`](Self::seal)
    /// canonicalizes them.
    pub proposals: Vec<Membership>,
    /// The one base source digest the bloom seals against.
    pub base: Digest,
    /// The bloom-wide configuration (ADR-0174), resolved when no member seals
    /// its own entry for a kind. A sealed
    /// [`StageCatalog`](crate::StageCatalog) entry here is what chooses agents
    /// per stage; sealing none runs the compiled line.
    pub configs: ConfigRegistry,
    /// The sealed forecast study grades against.
    pub forecast: Forecast,
}

impl BloomDraft {
    /// Freeze the draft into an immutable spec with a canonical member order.
    ///
    /// Members are sorted over their full content and de-duplicated, so a draft
    /// and the same draft with its proposals in any other order, or with an
    /// exact proposal repeated, seal to byte-identical specs and therefore the
    /// same [`BloomId`]. Ordering over the full member content is what makes the
    /// key a total order: sorting on any single field leaves two members sharing
    /// that field order-undetermined, so their input position leaks into the id.
    /// The id is then a stable function of the member *set*, not its order —
    /// even for a degenerate set the reducer will later reject at admission.
    ///
    /// The sort leads on `workpiece` because that is the member's identity; the
    /// remaining keys break ties among proposals that name the same workpiece.
    #[must_use]
    pub fn seal(&self) -> BloomSpec {
        let mut members = self.proposals.clone();
        members.sort_by(|a, b| {
            a.workpiece
                .cmp(&b.workpiece)
                .then_with(|| a.scope_revision.cmp(&b.scope_revision))
                .then_with(|| a.configs.cmp(&b.configs))
                .then_with(|| a.approval.subject.cmp(&b.approval.subject))
                .then_with(|| a.approval.kind.cmp(&b.approval.kind))
                .then_with(|| a.approval.detail.cmp(&b.approval.detail))
        });
        members.dedup();
        BloomSpec { members, base: self.base, configs: self.configs.clone(), forecast: self.forecast }
    }
}

/// An immutable, sealed bloom (ADR-0149 §The bloom).
///
/// Fields are private and there is no mutation API: a sealed spec never
/// amends. Changed membership, scope, base, or configuration creates a *successor*
/// bloom (see [`crate::reduce()`]), never an edit. The only constructors are
/// [`BloomDraft::seal`] and deserialization (journal replay).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BloomSpec {
    members: Vec<Membership>,
    base: Digest,
    configs: ConfigRegistry,
    forecast: Forecast,
}

impl ContentAddressed for BloomSpec {
    const DOMAIN: &'static str = "aether.bloomery.bloom_spec";
}

impl BloomSpec {
    /// The bloom's identity: the digest of its canonical spec bytes.
    #[must_use]
    pub fn id(&self) -> BloomId {
        BloomId(digest_of(self))
    }

    /// The sealed memberships, in canonical order.
    #[must_use]
    pub fn members(&self) -> &[Membership] {
        &self.members
    }

    /// The one base source digest this bloom sealed against — landing is a
    /// compare-and-swap on this base.
    #[must_use]
    pub const fn base(&self) -> Digest {
        self.base
    }

    /// The bloom-wide configuration registry (ADR-0174) — the outer scope every
    /// member's lookup falls through to.
    #[must_use]
    pub const fn configs(&self) -> &ConfigRegistry {
        &self.configs
    }

    /// Every configuration registry this spec seals: the bloom-wide one, then
    /// one per member in canonical order.
    ///
    /// The whole configuration surface a sealed bloom names, which is what a
    /// caller walks to know what content it must be able to produce before the
    /// spec can be admitted or run.
    pub fn config_registries(&self) -> impl Iterator<Item = &ConfigRegistry> {
        once(&self.configs).chain(self.members.iter().map(|member| &member.configs))
    }

    /// The scope chain a lookup on `member`'s behalf walks: that member's
    /// registry, then this bloom's.
    #[must_use]
    pub const fn scopes<'a>(&'a self, member: &'a Membership) -> ConfigScopes<'a> {
        ConfigScopes::member_of(&member.configs, &self.configs)
    }

    /// The sealed forecast.
    #[must_use]
    pub const fn forecast(&self) -> Forecast {
        self.forecast
    }
}

/// One member's contribution to an integration fold: which workpiece, and the
/// candidate tree it claimed.
///
/// The fold needs both. The tree is what a same-base single-member fold states
/// directly; the workpiece is what addresses that member's candidate *ref*,
/// which is what a fold has to merge when it combines work built against
/// different points — several members onto one branch, or a bloom caught up to
/// a moved base. A tree carries no ancestry, so it cannot be merged; only the
/// commit its ref points at can.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MemberCandidate {
    /// The member whose candidate this is — the ref address's workpiece segment.
    pub workpiece: WorkpieceId,
    /// The claimed candidate tree.
    pub candidate: Digest,
}

/// A per-member claim that a candidate resolves its workpiece on the final
/// tree, with evidence bound to the candidate digest (ADR-0149 §The bloom).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ResolutionClaim {
    /// The workpiece this claim resolves.
    pub workpiece: WorkpieceId,
    /// The scope revision this candidate was integrated against. A successor
    /// inherits a predecessor's claim only when it re-admits the same workpiece
    /// at this same revision — a scope-changed member drops its stale claim
    /// (ADR-0149 §The bloom).
    pub scope_revision: Digest,
    /// The exact candidate digest that resolves it.
    pub candidate: Digest,
    /// Evidence bound to `candidate`.
    pub evidence: Evidence,
}

/// The one artifact a bloom's execution produces: the final tree, its
/// integration lineage, and a resolution claim for every member workpiece
/// (ADR-0149 §The bloom).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ResolvedBloom {
    /// The bloom that resolved.
    pub bloom: BloomId,
    /// The final integrated tree digest.
    pub tree: Digest,
    /// The landable head commit's digest (distinct from `tree`), carried from
    /// the integrate outcome so `land` swaps mainline onto a commit rather than
    /// the bare artifact tree.
    pub head: Digest,
    /// The integration lineage (the checkpoints that built the tree).
    pub lineage: Vec<Digest>,
    /// One resolution claim per frozen member.
    pub resolution_claims: Vec<ResolutionClaim>,
}

/// The receipt of a compare-and-swap land: mainline moved from the sealed
/// base to the new head, and the next bloom seals on this receipt (ADR-0149
/// §The bloom).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LandingReceipt {
    /// The bloom that landed.
    pub bloom: BloomId,
    /// The mainline head the bloom sealed against.
    pub previous_base: Digest,
    /// The new mainline head after the swap.
    pub new_head: Digest,
}

#[cfg(test)]
mod tests {
    use super::{DependencyError, MemberDependency, resolve_member_dependencies};
    use crate::ids::WorkpieceId;

    fn wp(name: &str) -> WorkpieceId {
        WorkpieceId(name.to_owned())
    }

    fn edge(member: &str, depends_on: &str) -> MemberDependency {
        MemberDependency { member: wp(member), depends_on: wp(depends_on) }
    }

    #[test]
    fn overlapping_surfaces_derive_an_ordering_edge_in_canonical_order() {
        // The later-canonical member of an overlapping pair must wait for the
        // earlier one. Pairing the other way, or emitting no edge, would leave
        // the known collision to fold-time Reconcile — the bug this derivation
        // exists to close.
        let bloom = ["crates/aether-bloomery/**".to_owned()];
        let file = ["crates/aether-bloomery/src/values/price.rs".to_owned()];
        let docs = ["docs/guide/**".to_owned()];
        let members = [(wp("wp-a"), bloom.as_slice()), (wp("wp-b"), file.as_slice()), (wp("wp-c"), docs.as_slice())];

        let resolved = resolve_member_dependencies(&members, &[]).expect("acyclic");

        assert_eq!(resolved.edges, [edge("wp-b", "wp-a")], "one overlapping pair is one later-depends-on-earlier edge");
        assert!(resolved.declared.is_empty(), "an overlap is not a declared dispatch gate");
    }

    #[test]
    fn derived_edges_are_identical_for_every_permutation_of_three_overlapping_members() {
        // Three pairwise-overlapping members: a first/last-only swap would
        // still leave the middle member's direction input-order dependent.
        // Every permutation must produce the same edge list as the canonical
        // workpiece order.
        let bloom = ["crates/aether-bloomery/**".to_owned()];
        let values = ["crates/aether-bloomery/src/values/**".to_owned()];
        let file = ["crates/aether-bloomery/src/values/price.rs".to_owned()];
        let named = [(wp("wp-a"), bloom.as_slice()), (wp("wp-b"), values.as_slice()), (wp("wp-c"), file.as_slice())];
        let expected = [edge("wp-b", "wp-a"), edge("wp-c", "wp-a"), edge("wp-c", "wp-b")];

        for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            let members = [named[order[0]].clone(), named[order[1]].clone(), named[order[2]].clone()];
            let resolved = resolve_member_dependencies(&members, &[]).expect("acyclic");
            assert_eq!(resolved.edges, expected, "permutation must match the canonical edge list");
            assert!(resolved.declared.is_empty(), "overlap-only permutation journals no declared gate");
        }
    }

    #[test]
    fn sorting_keeps_declared_edges_and_does_not_invent_disjoint_ones() {
        // Canonicalizing input order must not drop a declared-only edge and
        // must not invent one for a disjoint surface. Either would make
        // order independence a data loss.
        let bloom = ["crates/aether-bloomery/**".to_owned()];
        let file = ["crates/aether-bloomery/src/lib.rs".to_owned()];
        let docs = ["docs/guide/**".to_owned()];
        let named = [(wp("wp-a"), bloom.as_slice()), (wp("wp-b"), file.as_slice()), (wp("wp-c"), docs.as_slice())];
        let declared = [edge("wp-c", "wp-a")];
        let expected = [edge("wp-b", "wp-a"), edge("wp-c", "wp-a")];

        for order in [[0, 1, 2], [2, 1, 0], [1, 2, 0]] {
            let members = [named[order[0]].clone(), named[order[1]].clone(), named[order[2]].clone()];
            let resolved = resolve_member_dependencies(&members, &declared).expect("acyclic");
            assert_eq!(resolved.edges, expected);
            assert_eq!(resolved.declared, declared, "a disjoint declared edge is not dropped by overlap derivation");
        }
    }

    #[test]
    fn declared_edges_union_with_derived_and_dedup() {
        // A declared edge that the surfaces would also derive must appear once.
        // Emitting it twice would make two seals of the same graph journal
        // different edge lists, and a silent drop of the declared-only edge
        // would lose an operator sequencing that no surface overlap predicted.
        let bloom = ["crates/aether-bloomery/**".to_owned()];
        let file = ["crates/aether-bloomery/src/lib.rs".to_owned()];
        let other = ["crates/aether-http/**".to_owned()];
        let members = [(wp("wp-a"), bloom.as_slice()), (wp("wp-b"), file.as_slice()), (wp("wp-c"), other.as_slice())];

        let resolved =
            resolve_member_dependencies(&members, &[edge("wp-b", "wp-a"), edge("wp-c", "wp-a")]).expect("acyclic");

        assert_eq!(resolved.edges, [edge("wp-b", "wp-a"), edge("wp-c", "wp-a")]);
        assert_eq!(
            resolved.declared,
            [edge("wp-b", "wp-a"), edge("wp-c", "wp-a")],
            "a declared edge that surfaces also derive stays declared"
        );
    }

    #[test]
    fn a_declared_edge_against_a_derived_overlap_is_still_a_cycle() {
        // Derived B→A (canonical overlap order) plus declared A→B is a loop.
        // Checking only the declared subset would admit it, then journal a
        // graph no scheduler can fire.
        let bloom = ["crates/aether-bloomery/**".to_owned()];
        let file = ["crates/aether-bloomery/src/lib.rs".to_owned()];
        let members = [(wp("wp-a"), bloom.as_slice()), (wp("wp-b"), file.as_slice())];

        match resolve_member_dependencies(&members, &[edge("wp-a", "wp-b")]) {
            Err(DependencyError::Cycle(cycle)) => {
                assert!(
                    cycle.contains(&wp("wp-a")) && cycle.contains(&wp("wp-b")),
                    "cycle names both members: {cycle:?}"
                );
            }
            other => panic!("expected a named cycle, got {other:?}"),
        }
    }

    #[test]
    fn a_cycle_is_refused_naming_its_members() {
        // A↔B is the smallest cycle an operator can write. Reporting only one
        // side, or succeeding, would journal a graph no scheduler can fire.
        let empty: [String; 0] = [];
        let members = [(wp("wp-a"), empty.as_slice()), (wp("wp-b"), empty.as_slice())];

        match resolve_member_dependencies(&members, &[edge("wp-a", "wp-b"), edge("wp-b", "wp-a")]) {
            Err(DependencyError::Cycle(cycle)) => {
                assert!(
                    cycle.contains(&wp("wp-a")) && cycle.contains(&wp("wp-b")),
                    "cycle names both members: {cycle:?}"
                );
            }
            other => panic!("expected a named cycle, got {other:?}"),
        }
    }

    #[test]
    fn an_edge_to_a_non_member_is_refused_naming_it() {
        // An edge pointing outside the bloom cannot be scheduled against this
        // membership. Swallowing it, or naming the in-bloom end instead, would
        // hide the workpiece the operator actually misspelled.
        let empty: [String; 0] = [];
        let members = [(wp("wp-a"), empty.as_slice())];

        match resolve_member_dependencies(&members, &[edge("wp-a", "wp-z")]) {
            Err(DependencyError::UnknownWorkpiece(named)) => assert_eq!(named, wp("wp-z")),
            other => panic!("expected the outsider named, got {other:?}"),
        }
    }
}
