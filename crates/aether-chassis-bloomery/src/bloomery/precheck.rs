//! A small, rebuildable view of the coordinator's pre-check decisions.
//!
//! The journal remains authoritative. The two host reactors keep only the
//! latest pre-check state for each opted-in bloom, reading new journal rows
//! after a cursor instead of rebuilding every member and proof on every tick.

use std::collections::BTreeMap;

use rusqlite::ffi::{Error as SqliteError, SQLITE_ERROR};

use aether_bloomery::{
    BloomId, Decision, Digest, PrecheckPlan, PrecheckResult, PrecheckState, Topic, WorkpieceId,
    decode_recorded_decisions,
};

use crate::store::StoreBackend;

const JOURNAL_PAGE_ROWS: u32 = 256;
const MAX_PAGES_PER_TURN: usize = 8;

#[derive(Default)]
pub struct PrecheckProjection {
    cursor: u64,
    states: BTreeMap<BloomId, PrecheckState>,
    catching_up: bool,
    observed: bool,
}

impl PrecheckProjection {
    /// Refresh from recorded decisions. `false` means the bounded replay still
    /// has work; no speculative side effect may use the incomplete view.
    ///
    /// # Errors
    /// A store read or a recorded decision's schema could not be decoded.
    pub fn refresh(&mut self, store: &mut dyn StoreBackend) -> rusqlite::Result<bool> {
        for _ in 0..MAX_PAGES_PER_TURN {
            let rows = store.replay_journal_after(self.cursor, JOURNAL_PAGE_ROWS)?;
            let caught_up = rows.len() < JOURNAL_PAGE_ROWS as usize;
            for row in rows {
                let decisions = decode_recorded_decisions(&row.decisions, row.decisions_schema_digest.as_deref())
                    .map_err(|error| projection_error(format!("pre-check journal row {}: {error}", row.sequence)))?;
                for effect in decisions.effects {
                    if let Decision::RecordPrecheckState { bloom, state } = effect {
                        self.observed = true;
                        match state {
                            Some(state) => {
                                self.states.insert(bloom, *state);
                            }
                            None => {
                                self.states.remove(&bloom);
                            }
                        }
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

    /// Refresh only when this host has pre-check work, then restore diagnostics
    /// for the exact failed node that the final fold joined. Speculative red
    /// results never become a member or composition repair's work order.
    pub fn prepare_dispatch(&mut self, store: &mut dyn StoreBackend) -> rusqlite::Result<bool> {
        if !self.observed
            && !store.has_outbox_topic(Topic::OfferPrecheck.as_str())?
            && !store.has_outbox_topic(Topic::DispatchPrecheck.as_str())?
        {
            return Ok(true);
        }
        self.observed = true;
        if !self.refresh(store)? {
            return Ok(false);
        }
        for (bloom, state) in self.states() {
            let Some(joined) = &state.final_join else {
                continue;
            };
            if !matches!(&state.result, Some(PrecheckResult::Failed { node, .. }) if *node == joined.digest()) {
                continue;
            }
            let Some(findings) = store.lookup_review_findings(bloom.0.as_bytes(), &findings_key(joined.digest()))?
            else {
                continue;
            };
            let marker = format!("Aggregate pre-check {}", joined.digest().to_hex());
            let previous =
                store.lookup_review_findings(bloom.0.as_bytes(), WorkpieceId::COMPOSITION)?.unwrap_or_default();
            if !previous.contains(&marker) {
                let combined = format!("{previous}\n\n{marker}\n\n{findings}");
                store.record_review_findings(bloom.0.as_bytes(), WorkpieceId::COMPOSITION, &combined)?;
            }
        }
        Ok(true)
    }

    #[must_use]
    pub fn get(&self, bloom: &BloomId) -> Option<&PrecheckState> {
        self.states.get(bloom)
    }

    pub fn states(&self) -> impl Iterator<Item = (&BloomId, &PrecheckState)> {
        self.states.iter()
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    #[must_use]
    pub fn catching_up(&self) -> bool {
        self.catching_up
    }
}

/// Scratch refs are private to an immutable plan, with the same derivation on
/// preparation and terminal cleanup.
pub fn namespace(plan: &PrecheckPlan) -> BloomId {
    let mut bytes = b"aether.bloomery.aggregate-precheck:".to_vec();
    bytes.extend_from_slice(plan.digest().as_bytes());
    BloomId(Digest::of_wire_bytes(&bytes))
}

/// A rebuildable diagnostic cache in the existing findings table. The journal
/// decides which node may feed a repair; this key carries no proof authority.
pub fn findings_key(node: Digest) -> String {
    format!("precheck:{}", node.to_hex())
}

fn projection_error(message: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(SqliteError::new(SQLITE_ERROR), Some(message))
}
