//! The workpiece staging route — `POST /workpieces`. Staging is pure in-memory
//! shaping state (nothing durable is claimed until a draft seals), so the route
//! answers synchronously.

use aether_bloomery::Workpiece;

use super::hex;
use super::response::{error_response, json};
use super::state::{ApiCapabilityState, MAX_STAGED_WORKPIECES, Routed};

impl ApiCapabilityState {
    /// `POST /workpieces` — stage a workpiece for later draft membership.
    pub(super) fn stage_workpiece(&mut self, body: &[u8]) -> Routed {
        let workpiece: Workpiece = match hex::from_slice(body) {
            Ok(workpiece) => workpiece,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid workpiece body: {error}"))),
        };
        // Re-staging an existing id overwrites in place; only a net-new id grows
        // the map, so the cap gates new keys and lets an idempotent re-stage
        // through at the ceiling.
        if !self.staged.contains_key(&workpiece.id.0) && self.staged.len() >= MAX_STAGED_WORKPIECES {
            return Routed::Reply(error_response(429, "staged-workpiece budget exhausted"));
        }
        self.staged.insert(workpiece.id.0.clone(), workpiece.clone());
        Routed::Reply(json(201, &workpiece))
    }
}
