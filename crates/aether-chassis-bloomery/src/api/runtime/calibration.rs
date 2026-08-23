//! `GET /calibration` — the measured capability ledger (ADR-0184).
//!
//! The read an operator argues a calibration edit from: what each
//! `(harness, model, effort)` actually did at each stage, and how many
//! observations back it. Like every durable read here the route only forwards
//! and defers; the control core owns the fold, and what lives here is the ask
//! and the rendering of its answer.

use aether_bloomery::{CalibrationDocument, Query, QuerySelector};
use aether_data::wire::from_bytes;
use aether_http::HttpServerResponse;

use super::response::{error_response, json};
use super::state::Routed;

/// `GET /calibration` — read the ledger the control core folded beside its
/// snapshot, together with the forecast grade of the blooms that produced it.
pub(super) fn read() -> Routed {
    Routed::Query(Query { selector: QuerySelector::Calibration })
}

/// Render the control core's calibration reply.
pub(super) fn calibration_response(document: &[u8]) -> HttpServerResponse {
    match from_bytes::<CalibrationDocument>(document) {
        Ok(document) => json(200, &document),
        Err(error) => error_response(500, &format!("calibration document decode failed: {error}")),
    }
}
