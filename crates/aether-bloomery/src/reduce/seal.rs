//! The two admission doors: sealing a draft into an active bloom, and
//! superseding a predecessor with a successor that inherits its claims
//! (ADR-0149 §The bloom). Both run the same per-member admission.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use serde::de::DeserializeOwned;

use super::attempt::stage_profile;
use super::{
    BloomStatus, Decision, Decisions, Outcome, SealConflict, SealError, Snapshot, StageProgress, SupersedeError,
};
use crate::digest::Digest;
use crate::ids::BloomId;
use crate::values::{
    BloomSpec, ConfigKind, ConfigRegistry, ConfigResolveError, ConfigScopes, EvidenceKind, MemberCandidate, Membership,
    ModelOverride, ResolvedConfigs, StageCatalog, Transformation, Unproducible,
};

/// Build the seal-time entry-stage dispatch effects for one member: seed its
/// cursor at the entry stage (attempt 1) and dispatch the first attempt against
/// its frozen scope revision. The cursor advance folds into the snapshot; the
/// dispatch is a snapshot-inert outbox effect the host submits (ADR-0149 §The
/// line).
///
/// `checkout` is the git commit the attempt's worker checks out — the bloom's
/// sealed base (`spec.base()`), threaded onto the transformation so the worker
/// builds the candidate against the exact sealed source (ADR-0149 §Execution,
/// #3572). It is distinct from the member's `scope_revision` subject, which is
/// the aether content digest the returned evidence binds to.
fn entry_dispatch_effects(
    bloom: BloomId,
    member: &Membership,
    checkout: Digest,
    bloom_configs: &ConfigRegistry,
    catalog: &StageCatalog,
) -> [Decision; 2] {
    let stage = StageCatalog::entry_stage();
    [
        Decision::AdvanceStage {
            bloom,
            workpiece: member.workpiece.clone(),
            progress: StageProgress { stage, attempts: 1, candidate: None, repair_rolls: 0 },
        },
        Decision::DispatchAttempt {
            bloom,
            workpiece: member.workpiece.clone(),
            stage,
            transformation: Transformation::for_member_stage(stage, member.scope_revision, checkout),
            scope_revision: member.scope_revision,
            candidate: None,
            profile: stage_profile(catalog, stage),
            configs: member.configs.layered_over(bloom_configs),
        },
    ]
}

pub(super) fn reduce_seal(snapshot: &Snapshot, spec: &BloomSpec, configs: &ResolvedConfigs) -> Decisions {
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
    // V1 permits one sealed, unlanded bloom per mainline: refuse while any bloom
    // is still Sealed or Resolved. Landed and Superseded blooms don't block, and
    // a successor seals via Fact::Supersede — exempt, since it nets ≤1 active.
    if let Some(active) = active_unlanded_bloom(snapshot) {
        return Decisions::rejected(Outcome::SealRejected(SealError::ActiveBloomExists(active)));
    }
    // Claim each member, then seed its cursor at the entry stage and dispatch its
    // first attempt — a sealed bloom's members enter the line at `Construct`
    // (ADR-0149 §The line). The claims come first so the dispatch effects attach
    // to a member already in `active`.
    let mut effects = Vec::with_capacity(spec.members().len() * 3);
    for member in spec.members() {
        effects.push(Decision::ClaimMembership { workpiece: member.workpiece.clone(), bloom });
    }
    for member in spec.members() {
        effects.extend(entry_dispatch_effects(bloom, member, spec.base(), spec.configs(), &catalog));
    }
    Decisions { outcome: Outcome::Sealed(bloom), effects }
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
        if !seen.insert(&member.workpiece) {
            return Err(SealError::DuplicateWorkpiece(member.workpiece.clone()));
        }
        if member.approval.kind != EvidenceKind::Approval || !member.approval.validates(&member.subject()) {
            return Err(SealError::UnapprovedMember(member.workpiece.clone()));
        }
    }
    Ok(())
}

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
fn active_unlanded_bloom(snapshot: &Snapshot) -> Option<BloomId> {
    snapshot.blooms.iter().find(|(_, record)| is_active_unlanded(record.status)).map(|(id, _)| *id)
}

pub(super) fn reduce_supersede(
    snapshot: &Snapshot,
    predecessor: &BloomId,
    successor: &BloomSpec,
    configs: &ResolvedConfigs,
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
    // A superseding spec is held to seal's catalog admission too — it must
    // promise the same known line (ADR-0149 §The line).
    if let Err(error) = validate_configs(successor, configs) {
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
    // inherited claim above) enters the line fresh: seed its cursor at the
    // entry stage and dispatch its first attempt against the successor's
    // sealed base — the same entry dispatch a seal runs (#3663). Net-new
    // members, scope-changed members, and re-admitted members whose
    // predecessor never integrated (the wedged-member escape hatch) would
    // otherwise be claimed but never executed, leaving the successor
    // unresolvable.
    let mut every_member_inherited = true;
    for member in successor.members() {
        let inherited =
            record.claims.get(&member.workpiece).is_some_and(|claim| claim.scope_revision == member.scope_revision);
        if !inherited {
            every_member_inherited = false;
            effects.extend(entry_dispatch_effects(
                successor_id,
                member,
                successor.base(),
                successor.configs(),
                &catalog,
            ));
        }
    }
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
        effects.push(Decision::DispatchIntegration { bloom: successor_id, base: successor.base(), members });
    }
    effects.push(Decision::MarkSuperseded { bloom: *predecessor, by: successor_id });
    Decisions { outcome: Outcome::Superseded { predecessor: *predecessor, successor: successor_id }, effects }
}
