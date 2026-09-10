//! Bounded journal projection for eager integration and reservation wakeups.

use std::collections::BTreeMap;

use aether_bloomery::{
    BloomId, CandidatePreparationPlan, CoordinationState, Decision, Digest, Event, Fact, IdempotencyKey,
    IntegrationAppendPlan, SharedRunPhase, SharedRunPlan, StableHeadReservation, WorkpieceId,
    decode_recorded_decisions,
};
use rusqlite::ffi::{Error as SqliteError, SQLITE_ERROR};

use crate::store::StoreBackend;

const JOURNAL_PAGE_ROWS: u32 = 256;
const MAX_PAGES_PER_TURN: usize = 8;

#[derive(Default)]
pub(super) struct CoordinationProjection {
    cursor: u64,
    states: BTreeMap<BloomId, CoordinationState>,
    candidate_plans: BTreeMap<(BloomId, WorkpieceId), Digest>,
    catching_up: bool,
}

impl CoordinationProjection {
    /// Refresh from recorded decisions. No reservation wake may use a partial
    /// projection because a later replay page can contain its release or a new
    /// generation.
    pub(super) fn refresh(&mut self, store: &mut dyn StoreBackend) -> rusqlite::Result<bool> {
        for _ in 0..MAX_PAGES_PER_TURN {
            let rows = store.replay_journal_after(self.cursor, JOURNAL_PAGE_ROWS)?;
            let caught_up = rows.len() < JOURNAL_PAGE_ROWS as usize;
            for row in rows {
                let decisions = decode_recorded_decisions(&row.decisions, row.decisions_schema_digest.as_deref())
                    .map_err(|error| projection_error(format!("coordination journal row {}: {error}", row.sequence)))?;
                for effect in decisions.effects {
                    match effect {
                        Decision::DispatchCandidatePreparation { plan } => {
                            self.candidate_plans.insert((plan.bloom, plan.workpiece.clone()), plan.digest());
                        }
                        Decision::RecordCoordinationState { bloom, state } => {
                            if let Some(state) = state {
                                self.states.insert(bloom, state);
                            } else {
                                self.states.remove(&bloom);
                                self.candidate_plans.retain(|(owner, _), _| *owner != bloom);
                            }
                        }
                        _ => {}
                    }
                }
                self.cursor = row.sequence;
            }
            if caught_up {
                self.catching_up = false;
                return Ok(true);
            }
        }
        self.catching_up = true;
        Ok(false)
    }

    pub(super) fn expired(&self, now_unix_millis: u64) -> Vec<Event> {
        self.states
            .iter()
            .filter_map(|(bloom, state)| {
                let reservation = state.integration.reservation.as_ref()?;
                (reservation.deadline_unix_millis <= now_unix_millis)
                    .then(|| expiration_event(*bloom, reservation.clone()))
            })
            .collect()
    }

    pub(super) fn catching_up(&self) -> bool {
        self.catching_up
    }

    pub(super) fn is_current_append(&self, plan: &IntegrationAppendPlan) -> bool {
        self.states.get(&plan.bloom).is_some_and(|state| {
            state.integration.in_flight.as_ref().is_some_and(|current| current.digest() == plan.digest())
        })
    }

    pub(super) fn is_current_preparation(&self, plan: &CandidatePreparationPlan) -> bool {
        self.candidate_plans.get(&(plan.bloom, plan.workpiece.clone())) == Some(&plan.digest())
            && self
                .states
                .get(&plan.bloom)
                .is_some_and(|state| state.integration.generation.digest() == plan.context.starting_head.generation)
    }

    pub(super) fn is_current_shared_run(&self, plan: &SharedRunPlan) -> bool {
        let bloom = plan
            .composition
            .as_ref()
            .map(|composition| composition.bloom)
            .or_else(|| plan.requests.first().map(|request| request.bloom));
        bloom.and_then(|bloom| self.states.get(&bloom)).is_some_and(|state| {
            state.runs.iter().any(|run| run.phase == SharedRunPhase::Preparing && run.plan.digest() == plan.digest())
                && plan.composition.as_ref().is_none_or(|composition| composition.base == state.integration.head)
        })
    }
}

fn expiration_event(bloom: BloomId, reservation: StableHeadReservation) -> Event {
    // Use the recorded deadline as the observation instant. This makes a wake
    // byte-for-byte reproducible after a crash while still proving the cadence
    // reached the deadline; process-local wall time is only the selection gate.
    let observed_at_unix_millis = reservation.deadline_unix_millis;
    Event {
        idempotency_key: IdempotencyKey(format!(
            "aether.bloomery.stable-head-expired:{}:{}:{}:{}:{}",
            bloom.0.to_hex(),
            reservation.generation.to_hex(),
            reservation.node.to_hex(),
            reservation.owner.0,
            reservation.deadline_unix_millis,
        )),
        fact: Fact::StableHeadReservationExpired { bloom, reservation, observed_at_unix_millis },
    }
}

fn projection_error(message: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(SqliteError::new(SQLITE_ERROR), Some(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_bloomery::WorkpieceId;
    use aether_bloomery::testing::digest;

    #[test]
    fn expiration_is_deterministic_and_waits_for_the_original_deadline() {
        let bloom = BloomId(digest(1));
        let reservation = StableHeadReservation {
            owner: WorkpieceId("repair".to_owned()),
            generation: digest(2),
            node: digest(3),
            movement_count: 4,
            deadline_unix_millis: 5_000,
            hold: digest(4),
        };
        let first = expiration_event(bloom, reservation.clone());
        let replayed = expiration_event(bloom, reservation);
        assert_eq!(first, replayed);
        assert!(matches!(first.fact, Fact::StableHeadReservationExpired { observed_at_unix_millis: 5_000, .. }));
    }
}
