//! Workpiece routes. `POST /workpieces` still stages an in-memory handle for
//! draft shaping; `GET /workpieces` lists durable open commissions (#5048).

use aether_bloomery::Workpiece;

use super::commission_reader::workpieces_from_list;
use super::hex;
use super::response::{error_response, json};
use super::state::{ApiCapabilityState, MAX_STAGED_WORKPIECES, Routed};
use crate::api::dto::WorkpiecesView;
use crate::store::{ListCommissions, ListCommissionsResult};

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

    /// `GET /workpieces` — durable open commissions, not the in-memory staged map.
    pub(super) fn list_open_workpieces() -> Routed {
        Routed::ListOpenWorkpieces(ListCommissions { status: Some("open".to_owned()) })
    }
}

/// Render the open-commission list as [`WorkpiecesView`].
pub(super) fn list_response(result: ListCommissionsResult) -> aether_http::HttpServerResponse {
    match workpieces_from_list(result) {
        Ok(workpieces) => json(200, &WorkpiecesView { workpieces }),
        Err(response) => response,
    }
}
