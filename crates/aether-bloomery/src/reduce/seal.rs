//! The two admission doors: sealing a draft into an active bloom, and
//! superseding a predecessor with a successor that inherits its claims
//! (ADR-0149 §The bloom). Both run the same per-member admission.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use aether_data::Kind;
use serde::de::DeserializeOwned;

use super::readiness::{ready_entries, successor_entries};
use super::{BloomStatus, Decision, Decisions, Outcome, SealConflict, SealError, Snapshot, SupersedeError};
use crate::ids::{BloomId, WorkpieceId};
use crate::values::{
    BloomSpec, ConfigKind, ConfigResolveError, ConfigScopes, DependencyError, EvidenceKind, MemberCandidate,
    MemberDependency, Membership, ModelOverride, ResolvedConfigs, SpendCeiling, SpendWindow, StageCatalog,
    Unproducible, resolve_member_dependencies,
};

pub(super) fn reduce_seal(
    snapshot: &Snapshot,
    spec: &BloomSpec,
    configs: &ResolvedConfigs,
    spend: &SpendWindow,
    edges: &[MemberDependency],
) -> Decisions {
    let bloom = spec.id();
    // A known id would resurrect and overwrite the existing record — a sealed
    // spec never amends (ADR-0149 §The bloom).
    if snapshot.blooms.contains_key(&bloom) {
        return Decisions::rejected(Outcome::SealRejected(SealError::KnownBloom(bloom)));
    }
    // Every member is verified: non-empty membership, no duplicate workpiece,
    // and each approval binds its own scope revision as an Approval (ADR-0149
    // "verify every member's scope and approval lineage").
    if let Err(error) = validate_member_admission(spec.members()) {
        return Decisions::rejected(Outcome::SealRejected(error));
    }
    if let Err(error) = validate_member_graph(spec.members(), edges) {
        return Decisions::rejected(Outcome::SealRejected(error));
    }
    // Every sealed configuration address must be one the reducer was given
    // content for (ADR-0174) — refused at the door rather than at the dispatch
    // that would park on it. Runs before the catalog check, which resolves
    // through this same set.
    if let Err(error) = validate_configs(spec, configs) {
        return Decisions::rejected(Outcome::SealRejected(error));
    }
    // The sealed catalog must be one the line can actually run, and every
    // member's override must be one that catalog can honour — a bloom is graded
    // against the exact line it promised (ADR-0149 §The line).
    let catalog = match validate_line(spec, configs) {
        Ok(catalog) => catalog,
        Err(error) => return Decisions::rejected(Outcome::SealRejected(error)),
    };
    // All-or-nothing admission: any member already in a foreign active bloom
    // aborts the whole seal, naming the conflict — a failed batch admission
    // leaves no claims (ADR-0149 §The bloom).
    if let Some(conflict) = membership_conflict(snapshot, spec.members(), None) {
        return Decisions::rejected(Outcome::SealRejected(SealError::MembershipConflict(conflict)));
    }
    // A land releases the claim, so the workpiece is immediately re-claimable —
    // but the work itself is already on the operating branch. Refuse the same
    // (workpiece, scope_revision) a landed bloom resolved; a fresh revision is
    // the re-run escape, not a bypass flag.
    if let Some(error) = landed_conflict(snapshot, spec.members()) {
        return Decisions::rejected(Outcome::SealRejected(error));
    }
    // V1 permits one sealed, unlanded bloom per mainline: refuse while any bloom
    // is still Sealed or Resolved. Landed and Superseded blooms don't block, and
    // a successor seals via Fact::Supersede — exempt, since it nets ≤1 active.
    if let Some(active) = active_unlanded_bloom(snapshot) {
        return Decisions::rejected(Outcome::SealRejected(SealError::ActiveBloomExists(active)));
    }
    // Last gate, after every check that names something wrong with the draft:
    // this one names something true about the fleet (ADR-0192). A member-scoped
    // ceiling is refused rather than resolved or ignored — a member must not
    // choose the ceiling that admits its own bloom.
    if let Err(error) = refuse_member_spend_ceiling(spec) {
        return Decisions::rejected(Outcome::SealRejected(error));
    }
    let ceiling = match sealed_config::<SpendCeiling>(ConfigScopes::bloom_wide(spec.configs()), configs) {
        Ok(ceiling) => ceiling,
        Err(error) => return Decisions::rejected(Outcome::SealRejected(error)),
    };
    if let Some(quiesce) = ceiling.as_ref().and_then(|ceiling| ceiling.quiesce(spend)) {
        return Decisions {
            outcome: Outcome::SealQuiesced(quiesce.clone()),
            effects: vec![Decision::RecordSpendQuiesce { quiesce: Some(quiesce) }],
        };
    }
    // Claim each member, then seed the ready ones at the entry stage and
    // dispatch their first attempt. Roots (no incoming edges) enter at
    // `Construct` exactly as today; dependents wait for a resolution claim
    // (ADR-0196). The claims come first so the dispatch effects attach to a
    // member already in `active`.
    let mut effects = Vec::with_capacity(spec.members().len() * 3 + 2);
    if snapshot.spend_quiesce.is_some() {
        effects.push(Decision::RecordSpendQuiesce { quiesce: None });
    }
    for member in spec.members() {
        effects.push(Decision::ClaimMembership { workpiece: member.workpiece.clone(), bloom });
    }
    // Record the catalog admission resolved so the fold reads the record, not
    // a later binary's compiled line (#4944).
    effects.push(Decision::RecordStageCatalog { bloom, catalog: catalog.clone() });
    effects.extend(ready_entries(bloom, spec.members(), edges, &|_| false, spec.configs(), &catalog, spec.base()));
    effects.push(Decision::RecordMemberDependencies { bloom, edges: edges.to_vec() });
    Decisions { outcome: Outcome::Sealed(bloom), effects }
}

/// Record a cross-member declared-surface overlap the seal door observed
/// (#4931).
///
/// A record and nothing else — no effects, no projection change. The overlap is
/// advice the operator acts on, so the reducer's job is to put what the door saw
/// where a replay will find it, next to the seal it was observed at. The
/// admission this warns about is decided by [`reduce_seal`] on its own terms and
/// is not consulted here: a warning that could refuse a seal would be the gate
/// the issue deliberately did not build.
///
/// The pairwise scan itself stays at the door because a sealed [`Membership`]
/// carries no declared surface — see [`crate::Fact::SurfaceOverlap`].
pub(super) fn reduce_surface_overlap(members: &[WorkpieceId], intersection: &[String]) -> Decisions {
    Decisions {
        outcome: Outcome::SurfaceOverlap { members: members.to_vec(), intersection: intersection.to_vec() },
        effects: Vec::new(),
    }
}

/// Refuse a `SpendCeiling` sealed on a member rather than resolving or
/// ignoring it (ADR-0192). A member choosing the ceiling that admits its own
/// bloom is the self-authorization the bloom-wide-only rule exists to close.
///
/// Reported as [`SealError::UnproducibleConfig`] so `SealError` gains no
/// variant: the address is named, and `MisfiledAs("member")` is why the
/// bloom-wide door will not produce it.
fn refuse_member_spend_ceiling(spec: &BloomSpec) -> Result<(), SealError> {
    for member in spec.members() {
        if let Some(address) = member.configs.address::<SpendCeiling>() {
            return Err(SealError::UnproducibleConfig {
                kind: String::from(SpendCeiling::NAME),
                address,
                reason: Unproducible::MisfiledAs(String::from("member")),
            });
        }
    }
    Ok(())
}

/// The per-member admission checks a bloom's membership must pass before it can
/// claim anything — a non-empty set, no duplicate workpiece, and every approval
/// binding its own [`subject`](Membership::subject) as an [`EvidenceKind::Approval`] (ADR-0149
/// "verify every member's scope and approval lineage"). Both the first-door
/// seal and the second-door supersession run it, so a successor is held to the
/// same member validity a fresh seal is. The error is a [`SealError`] — seal's
/// vocabulary is the canonical name for a bad membership; supersession wraps it.
fn validate_member_admission(members: &[Membership]) -> Result<(), SealError> {
    // A bloom resolves into one artifact carrying a claim for every member; an
    // empty membership would trivially resolve and advance mainline on zero
    // evidence.
    if members.is_empty() {
        return Err(SealError::EmptyMembership);
    }
    let mut seen = BTreeSet::new();
    for member in members {
        if member.workpiece.is_composition() {
            return Err(SealError::ReservedWorkpieceId(member.workpiece.clone()));
        }
        if !seen.insert(&member.workpiece) {
            return Err(SealError::DuplicateWorkpiece(member.workpiece.clone()));
        }
        if member.approval.kind != EvidenceKind::Approval || !member.approval.validates(&member.subject()) {
            return Err(SealError::UnapprovedMember(member.workpiece.clone()));
        }
    }
    Ok(())
}

/// Re-check a door-resolved graph the reducer is about to journal: every
/// endpoint is a member, and the directed edges are acyclic. Surfaces are
/// gone by the time the reducer holds the spec, so derivation is the door's;
/// this is the well-formedness half so a direct `Admit` of a cyclic
/// [`Fact::GraphSeal`](crate::Fact::GraphSeal) cannot land.
fn validate_member_graph(members: &[Membership], edges: &[MemberDependency]) -> Result<(), SealError> {
    if edges.is_empty() {
        return Ok(());
    }
    let listed: Vec<(WorkpieceId, &[String])> =
        members.iter().map(|member| (member.workpiece.clone(), EMPTY_SURFACE.as_slice())).collect();
    match resolve_member_dependencies(&listed, edges) {
        Ok(_) => Ok(()),
        Err(DependencyError::UnknownWorkpiece(workpiece)) => Err(SealError::UnknownDependency(workpiece)),
        Err(DependencyError::Cycle(cycle)) => Err(SealError::CyclicDependencies(cycle)),
    }
}

const EMPTY_SURFACE: [String; 0] = [];

/// The seal-time line admission (ADR-0174, #4601): whatever catalog the spec
/// seals must be one the line can run, and every member's model override must be
/// one that catalog can honour. Returns the resolved catalog, which the caller
/// goes on to dispatch against — the door and the dispatch read one value, so
/// they cannot be admitting one line and running another.
///
/// The catalog check is structural rather than equality against the compiled
/// line, which is the whole point — an operator seals a catalog *because* it
/// differs, choosing a cheap harness for construct and an expensive one for
/// review. What cannot differ is that every stage is bound exactly once, every
/// binding names a routable process, and every retry budget is a number the
/// reducer can count to. A catalog failing any of those would seal a bloom whose
/// members wedge with no attempt ever made.
///
/// The override check runs against that same resolved catalog, because which
/// stages fork an agent is exactly what a catalog decides. A member naming a
/// stage the catalog runs no model at has authored a choice nothing resolves.
///
/// A spec sealing neither passes: it runs [`StageCatalog::line`] with no
/// override, both structurally valid by construction. Both admission doors run
/// this, so a successor promises a runnable line exactly as a fresh seal does;
/// supersession wraps the error as [`SupersedeError::InvalidMember`].
fn validate_line(spec: &BloomSpec, configs: &ResolvedConfigs) -> Result<StageCatalog, SealError> {
    let catalog = sealed_config::<StageCatalog>(ConfigScopes::bloom_wide(spec.configs()), configs)?
        .unwrap_or_else(StageCatalog::line);
    catalog.validate().map_err(SealError::UnrunnableStageCatalog)?;

    for member in spec.members() {
        let scopes = ConfigScopes::member_of(&member.configs, spec.configs());
        sealed_config::<ModelOverride>(scopes, configs)?
            .unwrap_or_default()
            .validate(&catalog)
            .map_err(|error| SealError::UnusableModelOverride { workpiece: member.workpiece.clone(), error })?;
    }
    Ok(catalog)
}

/// Resolve a configuration the seal door itself must read, refusing rather than
/// defaulting when a scope sealed one whose content will not produce.
///
/// The typed counterpart of [`validate_configs`]'s name-keyed walk, and the
/// reason both exist. That walk holds no Rust type, so it catches an address
/// with no content and content filed under the wrong kind but cannot try the
/// decode; this closes the third case for the kinds the door reads by type.
/// Without it a catalog whose stored bytes no longer decode — the shape of a
/// configuration authored before a breaking change to its kind — would fall
/// through to the compiled line and seal a bloom running a line its receipt does
/// not name.
fn sealed_config<K: ConfigKind + DeserializeOwned>(
    scopes: ConfigScopes<'_>,
    configs: &ResolvedConfigs,
) -> Result<Option<K>, SealError> {
    // Only a sealed address can fail, so reading it first keeps the error able to
    // name what it was reaching for.
    let Some(address) = scopes.address::<K>() else {
        return Ok(None);
    };
    configs.resolve::<K>(scopes).map_err(|error| SealError::UnproducibleConfig {
        kind: String::from(K::NAME),
        address,
        reason: match error {
            ConfigResolveError::Missing { .. } => Unproducible::Absent,
            ConfigResolveError::KindMismatch { stored, .. } => Unproducible::MisfiledAs(stored),
            ConfigResolveError::Decode { .. } => Unproducible::Undecodable,
        },
    })
}

/// The seal-time configuration admission (ADR-0174): every address the spec's
/// registries seal — bloom-wide and per member — must be one the caller could
/// produce content for.
///
/// Checked at the door because a sealed address is immutable. Content that
/// cannot be produced now will not appear later, so admitting the bloom would
/// only move the failure to a dispatch that parks, after the bloom has claimed
/// its members and blocked the mainline. Refusing here costs the operator one
/// legible rejection instead.
///
/// Both admission doors run it, so a successor promises configuration the
/// reducer can read exactly as a fresh seal does.
fn validate_configs(spec: &BloomSpec, configs: &ResolvedConfigs) -> Result<(), SealError> {
    for registry in spec.config_registries() {
        if let Some((kind, address, reason)) = configs.unproducible_in(registry).next() {
            return Err(SealError::UnproducibleConfig { kind: String::from(kind), address, reason });
        }
    }
    Ok(())
}

/// The first member already held by an active bloom other than `exempt`, if
/// any. `exempt` names a predecessor whose holds are being released in the same
/// decision set (supersession) and so are not conflicts.
fn membership_conflict(snapshot: &Snapshot, members: &[Membership], exempt: Option<&BloomId>) -> Option<SealConflict> {
    members.iter().find_map(|member| {
        let held_by = snapshot.active.get(&member.workpiece)?;
        (Some(held_by) != exempt).then(|| SealConflict { workpiece: member.workpiece.clone(), held_by: *held_by })
    })
}

/// The first proposed member a landed bloom already resolved at the same scope
/// revision, if any. Derived from folded bloom records — replay rebuilds
/// `Landed` status through the land receipt fold, so the set is not a side
/// channel.
///
/// The key is `(workpiece, scope_revision)`, not workpiece id alone: a fresh
/// scope revision is the sealed-record escape for rework or revert-then-redo.
/// The set contains only members of *landed* blooms, so a successor re-proposing
/// its predecessor's still-unlanded members cannot trip this.
fn landed_conflict(snapshot: &Snapshot, members: &[Membership]) -> Option<SealError> {
    members.iter().find_map(|member| {
        snapshot.blooms.iter().find_map(|(bloom, record)| {
            let already = record.status == BloomStatus::Landed
                && record.spec.members().iter().any(|landed| {
                    landed.workpiece == member.workpiece && landed.scope_revision == member.scope_revision
                });
            already.then(|| SealError::WorkpieceAlreadyLanded { workpiece: member.workpiece.clone(), bloom: *bloom })
        })
    })
}

/// Whether a bloom's status is active-and-unlanded — `Sealed` or `Resolved`.
/// The one predicate the V1 one-active-bloom-per-mainline seal guard
/// (`active_unlanded_bloom`) and the boot-time claim-ref reconcile
/// (`control::actor`) both read, so the "which blooms hold claim refs" question
/// has a single answer both sides cannot drift from (ADR-0150 §The claim
/// registry).
#[must_use]
pub fn is_active_unlanded(status: BloomStatus) -> bool {
    matches!(status, BloomStatus::Sealed | BloomStatus::Resolved)
}

/// The id of a bloom still `Sealed` or `Resolved` (an unlanded active bloom),
/// if any — the input to the V1 one-active-bloom-per-mainline guard.
pub(super) fn active_unlanded_bloom(snapshot: &Snapshot) -> Option<BloomId> {
    snapshot.blooms.iter().find(|(_, record)| is_active_unlanded(record.status)).map(|(id, _)| *id)
}

pub(super) fn reduce_supersede(
    snapshot: &Snapshot,
    predecessor: &BloomId,
    successor: &BloomSpec,
    configs: &ResolvedConfigs,
    edges: &[MemberDependency],
) -> Decisions {
    let Some(record) = snapshot.blooms.get(predecessor) else {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::UnknownOrInactivePredecessor));
    };
    // The ADR's primary supersession trigger is a failed land, which happens at
    // Resolved — so both Sealed and Resolved predecessors are supersedable, or
    // a failed land wedges the bloom permanently (ADR-0149 §The bloom).
    if !matches!(record.status, BloomStatus::Sealed | BloomStatus::Resolved) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::UnknownOrInactivePredecessor));
    }
    let successor_id = successor.id();
    if successor_id == *predecessor {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::SelfSupersession));
    }
    // The successor is a second admission into `active`; a successor id that
    // collides with some other already-known bloom would resurrect and
    // overwrite that bloom's record, mirroring `reduce_seal`'s `KnownBloom`
    // guard on the seal door.
    if snapshot.blooms.contains_key(&successor_id) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::KnownSuccessor(successor_id)));
    }
    // The successor is a fresh membership set claiming into `active`, so it must
    // pass the same per-member admission a seal runs before it claims or inherits
    // anything — an empty, duplicate-workpiece, or unapproved successor is
    // refused here rather than admitted on invalid members (ADR-0149).
    if let Err(error) = validate_member_admission(successor.members()) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::InvalidMember(error)));
    }
    if let Err(error) = validate_member_graph(successor.members(), edges) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::InvalidMember(error)));
    }
    // A superseding spec is held to seal's catalog admission too — it must
    // promise the same known line (ADR-0149 §The line).
    if let Err(error) = validate_configs(successor, configs) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::InvalidMember(error)));
    }
    // A member-scoped ceiling is invalid configuration on either door. The
    // spend *comparison* does not gate supersession — a successor may admit
    // while the window is over ceiling so the drain that rolls the window
    // stays open (ADR-0192).
    if let Err(error) = refuse_member_spend_ceiling(successor) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::InvalidMember(error)));
    }
    // And to seal's line admission, so a successor cannot introduce a catalog
    // that cannot run or an override that catalog cannot honour.
    let catalog = match validate_line(successor, configs) {
        Ok(catalog) => catalog,
        Err(error) => return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::InvalidMember(error))),
    };
    // Supersession is a second door into `active`, so it runs the same
    // all-or-nothing conflict scan as seal — but the predecessor's own holds are
    // released in this decision set, so only a foreign bloom's hold conflicts.
    if let Some(conflict) = membership_conflict(snapshot, successor.members(), Some(predecessor)) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::MembershipConflict(conflict)));
    }
    // The same landed-set scan seal runs: a fresh successor member that a
    // landed bloom already resolved is refused, while the predecessor's own
    // unlanded members stay admissible — they live on a Sealed or Resolved
    // record, not a Landed one, so they are not in the set.
    if let Some(error) = landed_conflict(snapshot, successor.members()) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::InvalidMember(error)));
    }
    // A successor that rebases takes mainline with it (#4709) — the resync
    // trigger `reduce_observe_mainline` defers to, landing here because this is
    // the machinery that mints the successor. Mainline only moves while nothing
    // is in flight, so a repository that advances during a bloom leaves the
    // observed head ahead of it, and a wedged bloom never leaves flight on its
    // own: without this the coordinator can never catch up, and every successor
    // inherits a base it cannot land on.
    //
    // Exactly two bases are admissible, and the second is the whole guard: the
    // one mainline is already at, and the one the source last reported. Any
    // other digest would let a caller write the compare-and-swap anchor
    // directly.
    let rebase = (successor.base() != snapshot.mainline).then_some(successor.base());
    if let Some(base) = rebase
        && base != snapshot.observed
    {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::UnobservedBase {
            base,
            observed: snapshot.observed,
        }));
    }
    let mut effects = Vec::new();
    // Release the predecessor's memberships, then claim the successor's, then
    // inherit the predecessor's still-valid resolution claims, then name it
    // superseded — one decision set, applied atomically (a successor atomically
    // inherits its predecessor's claims, ADR-0149 §The bloom).
    for member in record.spec.members() {
        effects.push(Decision::ReleaseMembership { workpiece: member.workpiece.clone(), bloom: *predecessor });
    }
    for member in successor.members() {
        effects.push(Decision::ClaimMembership { workpiece: member.workpiece.clone(), bloom: successor_id });
    }
    effects.push(Decision::RecordStageCatalog { bloom: successor_id, catalog: catalog.clone() });
    // Inherit only a claim whose workpiece the successor re-admits at the same
    // scope revision — an ejected or scope-changed workpiece drops its stale
    // claim (ADR-0149 §The bloom).
    for claim in record.claims.values() {
        let still_valid = successor
            .members()
            .iter()
            .any(|m| m.workpiece == claim.workpiece && m.scope_revision == claim.scope_revision);
        if still_valid {
            effects.push(Decision::InheritClaim { bloom: successor_id, claim: claim.clone() });
        }
    }
    // Every successor member that does not arrive already integrated (an
    // inherited claim above) is a candidate for the entry line. Roots whose
    // dependencies are already inherited enter immediately; dependents wait
    // for those claims the same way a first seal waits (#3663, ADR-0196).
    // Net-new members, scope-changed members, and re-admitted members whose
    // predecessor never integrated (the wedged-member escape hatch) would
    // otherwise be claimed but never executed, leaving the successor
    // unresolvable.
    let (every_member_inherited, entries) = successor_entries(successor_id, successor, record, edges, &catalog);
    effects.extend(entries);
    // A successor every one of whose members arrived already integrated has a
    // complete claim set the instant it seals, and no member left to run — so
    // nothing downstream would ever dispatch its fold. `reduce_integrate`
    // dispatches integration on the claim that *completes* the set, and here no
    // claim arrives: they were all inherited in this same decision. Without
    // this the successor is claimed, complete, and permanently unresolvable —
    // the predecessor's work carried over and then stranded.
    //
    // This is the re-base shape: the same members at the same scope revisions
    // on a new base. Those candidates were built against the predecessor's
    // base, so the successor needs its own fold rather than a reuse of the
    // predecessor's integration — and that fold has to combine rather than
    // state, which is what the merge-based fold is for.
    if every_member_inherited {
        let members = successor
            .members()
            .iter()
            .filter_map(|member| {
                record
                    .claims
                    .get(&member.workpiece)
                    .map(|claim| MemberCandidate { workpiece: member.workpiece.clone(), candidate: claim.candidate })
            })
            .collect();
        effects.push(Decision::DispatchIntegration {
            bloom: successor_id,
            base: successor.base(),
            members,
            // Every candidate was produced under the predecessor's id.
            adopt_from: Some(*predecessor),
        });
    }
    // Last, so the mainline move is part of the same atomic decision set that
    // released the predecessor and claimed the successor: the base a land
    // compare-and-swaps against and the bloom entitled to land on it change
    // together or not at all.
    if let Some(base) = rebase {
        effects.push(Decision::AdvanceMainline { from: snapshot.mainline, to: base });
    }
    effects.push(Decision::MarkSuperseded { bloom: *predecessor, by: successor_id });
    effects.push(Decision::RecordMemberDependencies { bloom: successor_id, edges: edges.to_vec() });
    Decisions { outcome: Outcome::Superseded { predecessor: *predecessor, successor: successor_id }, effects }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use aether_data::Kind;
    use aether_data::wire::{from_bytes, to_vec};

    use super::{reduce_seal, reduce_supersede};
    use crate::digest::Digest;
    use crate::ids::{BloomId, IdempotencyKey, WorkpieceId};
    use crate::reduce::SealError;
    use crate::reduce::{
        BloomStatus, Decision, Decisions, Event, Fact, Outcome, Snapshot, decode_recorded_decisions, reduce,
    };
    use crate::values::{
        BloomDraft, ConfigKind, ConfigRegistry, Evidence, EvidenceKind, MemberDependency, Membership, ResolvedConfigs,
        SpendCeiling, SpendQuiesce, SpendWindow, Unproducible,
    };

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn membership(name: &str, revision: u8) -> Membership {
        let mut member = Membership {
            workpiece: WorkpieceId(name.into()),
            scope_revision: digest(revision),
            configs: ConfigRegistry::default(),
            approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
        };
        member.approval.subject = member.subject();
        member
    }

    fn draft(revision: u8) -> BloomDraft {
        BloomDraft { proposals: vec![membership("wp", revision)], base: digest(0), ..BloomDraft::default() }
    }

    fn ceiling_content(ceiling: &SpendCeiling) -> (ConfigRegistry, ResolvedConfigs) {
        let mut registry = ConfigRegistry::default();
        registry.insert::<SpendCeiling>(ceiling.address());
        let mut configs = ResolvedConfigs::default();
        configs.insert(ceiling.address(), SpendCeiling::NAME, to_vec(ceiling).expect("ceiling encodes"));
        (registry, configs)
    }

    fn draft_with_ceiling(revision: u8, ceiling: &SpendCeiling) -> (BloomDraft, ResolvedConfigs) {
        let (configs, resolved) = ceiling_content(ceiling);
        (BloomDraft { configs, ..draft(revision) }, resolved)
    }

    fn window(total: u64, per_bloom: &[(BloomId, u64)]) -> SpendWindow {
        SpendWindow {
            label: String::from("bloomery/daily/2026-08-14"),
            total_micro_usd: total,
            per_bloom: per_bloom.iter().copied().collect(),
            unaccounted_dispatches: 0,
            unpriced_records: 0,
        }
    }

    // The plausible bug: a window at or over its sealed ceiling still claims
    // members, so the door that should have closed keeps admitting work.
    #[test]
    fn a_window_at_ceiling_quiesces_and_claims_nothing() {
        let ceiling = SpendCeiling { window_micro_usd: Some(10), bloom_micro_usd: None };
        let (draft, configs) = draft_with_ceiling(1, &ceiling);
        let spec = draft.seal();
        let decided = reduce_seal(&Snapshot::new(digest(0)), &spec, &configs, &window(10, &[]), &[]);

        assert_eq!(
            decided.outcome,
            Outcome::SealQuiesced(SpendQuiesce::Window {
                window: String::from("bloomery/daily/2026-08-14"),
                spent_micro_usd: 10,
                ceiling_micro_usd: 10,
            }),
        );
        assert_eq!(
            decided.effects,
            vec![Decision::RecordSpendQuiesce {
                quiesce: Some(SpendQuiesce::Window {
                    window: String::from("bloomery/daily/2026-08-14"),
                    spent_micro_usd: 10,
                    ceiling_micro_usd: 10,
                }),
            }],
            "a quiesced seal records the crossing and claims no member",
        );
    }

    // The plausible bug: the per-bloom axis names the last bloom, or the
    // over-budget bloom is mutated when the next seal is refused.
    #[test]
    fn a_bloom_at_ceiling_quiesces_and_leaves_that_bloom_untouched() {
        let ceiling = SpendCeiling { window_micro_usd: None, bloom_micro_usd: Some(5) };
        let (next, configs) = draft_with_ceiling(2, &ceiling);
        let spec = next.seal();

        let existing = draft(1).seal();
        let existing_id = existing.id();
        let event = Event { idempotency_key: IdempotencyKey("prior".into()), fact: Fact::Seal(existing) };
        let prior = reduce_seal(
            &Snapshot::new(digest(0)),
            match &event.fact {
                Fact::Seal(spec) => spec,
                _ => unreachable!(),
            },
            &ResolvedConfigs::default(),
            &SpendWindow::default(),
            &[],
        );
        let snapshot = Snapshot::new(digest(0)).apply(&event, &prior, &ResolvedConfigs::default());
        let before = snapshot.blooms.get(&existing_id).expect("the prior bloom is on the snapshot").clone();

        // Land the prior bloom so the one-active-bloom gate is not what refuses.
        let mut snapshot = snapshot;
        snapshot.blooms.get_mut(&existing_id).expect("prior bloom").status = BloomStatus::Landed;
        snapshot.active.clear();

        let decided = reduce_seal(&snapshot, &spec, &configs, &window(4, &[(existing_id, 5)]), &[]);
        assert_eq!(
            decided.outcome,
            Outcome::SealQuiesced(SpendQuiesce::Bloom {
                window: String::from("bloomery/daily/2026-08-14"),
                bloom: existing_id,
                spent_micro_usd: 5,
                ceiling_micro_usd: 5,
            }),
        );

        let after = snapshot.apply(
            &Event { idempotency_key: IdempotencyKey("quiesce".into()), fact: Fact::Seal(spec) },
            &decided,
            &configs,
        );
        let still = after.blooms.get(&existing_id).expect("the over-budget bloom stays");
        assert_eq!(still.progress, before.progress, "a quiesced seal must not move the over-budget bloom's cursor");
        assert_eq!(still.claims, before.claims, "or revoke its claims");
        assert_eq!(still.dispatches, before.dispatches, "or touch its in-flight dispatches");
        assert_eq!(still.status, BloomStatus::Landed);
    }

    // The plausible bug: a successful seal after a prior crossing leaves the
    // marker standing, so /view keeps saying the door is closed.
    #[test]
    fn a_seal_under_both_axes_clears_a_standing_marker() {
        let ceiling = SpendCeiling { window_micro_usd: Some(100), bloom_micro_usd: Some(50) };
        let (draft, configs) = draft_with_ceiling(1, &ceiling);
        let spec = draft.seal();
        let mut snapshot = Snapshot::new(digest(0));
        snapshot.spend_quiesce = Some(SpendQuiesce::Window {
            window: String::from("bloomery/daily/2026-08-13"),
            spent_micro_usd: 100,
            ceiling_micro_usd: 80,
        });

        let decided = reduce_seal(&snapshot, &spec, &configs, &window(10, &[]), &[]);
        assert!(matches!(decided.outcome, Outcome::Sealed(_)), "under both axes the door opens: {:?}", decided.outcome);
        assert!(
            decided.effects.iter().any(|effect| matches!(effect, Decision::RecordSpendQuiesce { quiesce: None })),
            "a passing seal beside a standing marker emits the clear: {:?}",
            decided.effects,
        );
        assert!(
            decided.effects.iter().any(|effect| matches!(effect, Decision::ClaimMembership { .. })),
            "and still claims its members",
        );
    }

    // The plausible bug: an absent entry or a None axis still compares against
    // the measured total, so an uncapped fleet quiesces.
    #[test]
    fn an_absent_or_uncapped_ceiling_never_quiesces() {
        let spec = draft(1).seal();
        let decided =
            reduce_seal(&Snapshot::new(digest(0)), &spec, &ResolvedConfigs::default(), &window(u64::MAX, &[]), &[]);
        assert!(matches!(decided.outcome, Outcome::Sealed(_)), "no ceiling is uncapped: {:?}", decided.outcome);

        let ceiling = SpendCeiling { window_micro_usd: None, bloom_micro_usd: None };
        let (draft, configs) = draft_with_ceiling(2, &ceiling);
        let spec = draft.seal();
        let decided = reduce_seal(
            &Snapshot::new(digest(0)),
            &spec,
            &configs,
            &window(u64::MAX, &[(BloomId(digest(1)), u64::MAX)]),
            &[],
        );
        assert!(
            matches!(decided.outcome, Outcome::Sealed(_)),
            "a present None axis is uncapped: {:?}",
            decided.outcome
        );
    }

    // The plausible bug: a member-scoped ceiling is read as the bloom-wide
    // one, or ignored, so a member chooses the number that admits it.
    #[test]
    fn a_member_scoped_ceiling_refuses_rather_than_resolving() {
        let ceiling = SpendCeiling { window_micro_usd: Some(1), bloom_micro_usd: None };
        let mut member = membership("wp", 1);
        member.configs.insert::<SpendCeiling>(ceiling.address());
        member.approval.subject = member.subject();
        let spec = BloomDraft { proposals: vec![member], base: digest(0), ..BloomDraft::default() }.seal();
        let mut configs = ResolvedConfigs::default();
        configs.insert(ceiling.address(), SpendCeiling::NAME, to_vec(&ceiling).expect("ceiling encodes"));

        let decided = reduce_seal(&Snapshot::new(digest(0)), &spec, &configs, &SpendWindow::default(), &[]);
        assert!(
            matches!(
                decided.outcome,
                Outcome::SealRejected(SealError::UnproducibleConfig {
                    ref kind,
                    reason: Unproducible::MisfiledAs(ref stored),
                    ..
                }) if kind == SpendCeiling::NAME && stored == "member"
            ),
            "a member-scoped ceiling refuses: {:?}",
            decided.outcome,
        );
        assert!(decided.effects.is_empty(), "a refused seal claims nothing");
    }

    // The plausible bug: a missing / misfiled / undecodable ceiling falls
    // through to uncapped, so a bloom seals under a policy its receipt names
    // and the door cannot read.
    #[test]
    fn an_unproducible_ceiling_refuses_with_the_matching_reason() {
        let address = digest(9);
        let mut registry = ConfigRegistry::default();
        registry.insert::<SpendCeiling>(address);

        let spec = BloomDraft { configs: registry.clone(), ..draft(1) }.seal();
        let decided =
            reduce_seal(&Snapshot::new(digest(0)), &spec, &ResolvedConfigs::default(), &SpendWindow::default(), &[]);
        assert!(
            matches!(
                decided.outcome,
                Outcome::SealRejected(SealError::UnproducibleConfig {
                    ref kind,
                    reason: Unproducible::Absent,
                    ..
                }) if kind == SpendCeiling::NAME
            ),
            "missing content is Absent: {:?}",
            decided.outcome,
        );

        let mut configs = ResolvedConfigs::default();
        configs.insert(address, String::from("aether.bloomery.price_table"), vec![1, 2, 3]);
        let decided = reduce_seal(&Snapshot::new(digest(0)), &spec, &configs, &SpendWindow::default(), &[]);
        assert!(
            matches!(
                decided.outcome,
                Outcome::SealRejected(SealError::UnproducibleConfig {
                    ref kind,
                    reason: Unproducible::MisfiledAs(ref stored),
                    ..
                }) if kind == SpendCeiling::NAME && stored == "aether.bloomery.price_table"
            ),
            "misfiled content is MisfiledAs: {:?}",
            decided.outcome,
        );

        configs = ResolvedConfigs::default();
        configs.insert(address, SpendCeiling::NAME, vec![0xff, 0xff]);
        let decided = reduce_seal(&Snapshot::new(digest(0)), &spec, &configs, &SpendWindow::default(), &[]);
        assert!(
            matches!(
                decided.outcome,
                Outcome::SealRejected(SealError::UnproducibleConfig {
                    ref kind,
                    reason: Unproducible::Undecodable,
                    ..
                }) if kind == SpendCeiling::NAME
            ),
            "undecodable content is Undecodable: {:?}",
            decided.outcome,
        );
    }

    // The plausible bug: the governor also closes the supersede door, so a
    // wedged bloom cannot drain and the day cannot roll.
    #[test]
    fn supersede_admits_while_the_window_is_over_ceiling() {
        let ceiling = SpendCeiling { window_micro_usd: Some(1), bloom_micro_usd: None };
        let (predecessor_draft, configs) = draft_with_ceiling(1, &ceiling);
        let predecessor_spec = predecessor_draft.seal();
        let predecessor = predecessor_spec.id();
        let seal =
            Event { idempotency_key: IdempotencyKey("prior".into()), fact: Fact::Seal(predecessor_spec.clone()) };
        let prior = reduce_seal(&Snapshot::new(digest(0)), &predecessor_spec, &configs, &SpendWindow::default(), &[]);
        let snapshot = Snapshot::new(digest(0)).apply(&seal, &prior, &configs);

        let (successor_draft, successor_configs) = draft_with_ceiling(2, &ceiling);
        let successor = successor_draft.seal();
        let decided = reduce_supersede(&snapshot, &predecessor, &successor, &successor_configs, &[]);
        assert!(
            matches!(decided.outcome, Outcome::Superseded { .. }),
            "supersession is not spend-gated: {:?}",
            decided.outcome,
        );
    }

    // The plausible bug: an edgeless seal grows a non-empty graph, or inserts
    // the new effect in the middle, so every existing journal row's effect
    // suffix shifts on the next boot replay.
    #[test]
    fn an_edgeless_seal_is_today_plus_an_empty_appended_graph() {
        let spec = draft(1).seal();
        let bloom = spec.id();
        let decided =
            reduce_seal(&Snapshot::new(digest(0)), &spec, &ResolvedConfigs::default(), &SpendWindow::default(), &[]);

        assert!(matches!(decided.outcome, Outcome::Sealed(id) if id == bloom));
        let Some(Decision::RecordMemberDependencies { bloom: recorded, edges }) = decided.effects.last() else {
            panic!("edgeless seal must append the graph, got {:?}", decided.effects.last());
        };
        assert_eq!(*recorded, bloom);
        assert!(edges.is_empty(), "an edgeless seal journals an empty edge list, got {edges:?}");
        assert!(
            decided.effects.iter().filter(|effect| matches!(effect, Decision::RecordMemberDependencies { .. })).count()
                == 1,
            "the graph is appended once: {:?}",
            decided.effects,
        );
        assert!(
            decided.effects.iter().any(|effect| matches!(effect, Decision::ClaimMembership { .. })),
            "today's claims still precede the append",
        );
    }

    // The plausible bug: the door-resolved graph is decided but not folded, so
    // a restarted coordinator loses the edges ADR-0190 says replay must keep.
    // Replay is apply-only over the journaled row — two applies of one
    // in-memory Decisions value would agree even if the wire dropped the graph.
    #[test]
    fn a_seal_with_edges_journals_the_graph_and_replay_folds_it() {
        let spec = BloomDraft {
            proposals: vec![membership("wp-a", 1), membership("wp-b", 2)],
            base: digest(0),
            ..BloomDraft::default()
        }
        .seal();
        let bloom = spec.id();
        let edges =
            vec![MemberDependency { member: WorkpieceId("wp-b".into()), depends_on: WorkpieceId("wp-a".into()) }];
        let event = Event {
            idempotency_key: IdempotencyKey("graph".into()),
            fact: Fact::GraphSeal { predecessor: None, spec, edges: edges.clone() },
        };
        let base = Snapshot::new(digest(0));
        let decided = reduce(&base, &event, &ResolvedConfigs::default(), &SpendWindow::default());
        assert!(matches!(decided.outcome, Outcome::Sealed(id) if id == bloom));
        match decided.effects.last() {
            Some(Decision::RecordMemberDependencies { bloom: recorded, edges: recorded_edges }) => {
                assert_eq!(*recorded, bloom);
                assert_eq!(recorded_edges, &edges);
            }
            other => panic!("expected the resolved graph, got {other:?}"),
        }

        let live = base.apply(&event, &decided, &ResolvedConfigs::default());
        let journaled: Event = from_bytes(&to_vec(&event).expect("event encodes")).expect("event decodes");
        let recorded: Decisions = decode_recorded_decisions(&to_vec(&decided).expect("decisions encode"), None)
            .expect("journaled decisions decode");
        let replayed = base.apply(&journaled, &recorded, &ResolvedConfigs::default());

        assert_eq!(
            live.blooms.get(&bloom).map(|record| &record.dependencies),
            Some(&edges),
            "apply folds the journaled graph onto the record, so equality is not both paths ignoring it",
        );
        assert_eq!(live, replayed, "apply-only replay of the journaled row rebuilds the live snapshot");
    }
}
