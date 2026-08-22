//! Reading the reducer's per-member answers back out of the journal.
//!
//! Two questions share one source and have no other: which members of a bloom
//! actually resolved into it, and whether any bloom ever resolved a given
//! workpiece. Both are folded state rather than an event anyone can grep for —
//! a claim is inherited across a supersession as a *decision*, never re-admitted
//! as a fact — so replaying the journal is the only honest way to ask, and a
//! per-bloom scan for `Fact::Integrate` would silently answer "no" for every
//! member that arrived through a successor.
//!
//! The replay is a whole-journal read, affordable at both call sites: a bloom
//! lands once, and a commission is reopened by hand. Nothing here decides
//! anything — each caller states its own fail-closed direction for an answer it
//! could not get.

use std::collections::BTreeSet;

use aether_bloomery::{BloomId, Event, Snapshot, WorkpieceId, decode_recorded_decisions};
use aether_data::wire::from_bytes;

use super::runtime::{StoreBackend, resolved_configs};

/// Replay the journal into a snapshot, folding each row's *recorded* decisions.
///
/// Recorded rather than recomputed: the decisions column holds what the
/// coordinator applied when the row was written, so a replay of it reconstructs
/// the board that exists rather than the board this binary's reducer would
/// decide today.
///
/// A row that does not decode is skipped with a warning rather than propagated.
/// One unreadable row costs the answer that row's contribution, never the whole
/// read — and every consumer here reads a missing contribution as "did not
/// resolve", which is the recoverable direction at both call sites.
pub fn replay_snapshot(store: &mut dyn StoreBackend) -> rusqlite::Result<Snapshot> {
    let configs = resolved_configs(store)?;
    let mut snapshot = Snapshot::default();
    for record in store.replay_journal()? {
        let Ok(event) = from_bytes::<Event>(&record.event) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::store",
                sequence = record.sequence,
                "journal event did not decode; leaving it out of the membership replay",
            );
            continue;
        };
        let Ok(decisions) = decode_recorded_decisions(&record.decisions, record.decisions_schema.as_deref()) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::store",
                sequence = record.sequence,
                "journal decisions did not decode; leaving the row out of the membership replay",
            );
            continue;
        };
        snapshot = snapshot.apply(&event, &decisions, &configs);
    }
    Ok(snapshot)
}

/// The members of `bloom` whose work actually resolved into it.
///
/// `claims` is the reducer's own per-member resolution, inheritance from a
/// superseded predecessor included; `withdrawn` is the one-way exit a member
/// takes when an operator pulls it out of a walking bloom (#5327). A withdrawn
/// member produces no claim and contributes no candidate to the fold, so it is
/// not part of what landed even though it is still named in the sealed spec.
///
/// An unknown bloom answers with the empty set: this crate has no record that
/// anything resolved, and inventing one is the failure this exists to stop.
pub fn resolved_members(store: &mut dyn StoreBackend, bloom: &BloomId) -> rusqlite::Result<BTreeSet<WorkpieceId>> {
    let snapshot = replay_snapshot(store)?;
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Ok(BTreeSet::new());
    };
    Ok(record.claims.keys().filter(|workpiece| !record.withdrawn.contains_key(*workpiece)).cloned().collect())
}

/// The bloom that resolved `workpiece`, when one did.
///
/// The reopen door's guard reads this: a commission whose workpiece some bloom
/// resolved is landed for the ordinary reason, and putting it back in the line
/// would re-run work that is already in mainline. Any bloom counts, not the
/// newest one — a resolution is not undone by a later bloom naming the same
/// workpiece, and choosing between two would need an ordering the snapshot's
/// digest-keyed map does not carry.
pub fn resolving_bloom(store: &mut dyn StoreBackend, workpiece: &WorkpieceId) -> rusqlite::Result<Option<BloomId>> {
    let snapshot = replay_snapshot(store)?;
    Ok(snapshot.blooms.iter().find_map(|(bloom, record)| {
        (record.claims.contains_key(workpiece) && !record.withdrawn.contains_key(workpiece)).then_some(*bloom)
    }))
}
