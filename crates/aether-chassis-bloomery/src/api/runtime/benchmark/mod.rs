//! `POST /benchmark` — the operator door onto a benchmark run (ADR-0184, issue
//! #4871).
//!
//! Shaped like the operator doors next door (`/blooms/{id}/hold`,
//! `/blooms/{id}/members/{workpiece}/repair`, `/blooms/{id}/supersede`): a JSON
//! body that has to state a `reason` and name an `operator`, refused through the
//! same [`unstated`] gate, admitting through the same control core. No new
//! authentication path is invented here — a benchmark run is an operator
//! decision about the pipeline, which is exactly what those doors already are.
//!
//! What is new is the **trial-mode gate**, and it comes first: a coordinator
//! whose journal is live-classed is refused `409` before its body is even
//! parsed. That is a refusal and never a warning, because a benchmark's blooms
//! are indistinguishable from live ones once they are in a journal — the store's
//! class is the only thing that separates them (ADR-0184, issue #5794).
//!
//! # The hops
//!
//! Two, and only one of them is awaited. [`seal`] holds both.
//!
//! Each member's work order is written to its dispatch-description row
//! fire-and-forget, through the same `RecordDispatchDescription` the seal door
//! uses and for the same stated reason: a record *about* an admission must not
//! be able to fail the admission it describes. The run's one seal then goes out
//! as a tracked `Admit`, and the request is held across it so the reply can be
//! the run's own report — the set version and the caveats its cells are read
//! under — rather than the shared outcome rendering.

pub(super) mod seal;
#[cfg(test)]
mod tests;

use aether_actor::Manual;
use aether_bloomery::{Digest, StoreClass};
use aether_http::{HttpServerRequest, HttpServerResponse};
use aether_substrate::actor::native::NativeCtx;
use serde::Deserialize;

use super::blooms::unstated;
use super::hex;
use super::response::error_response;
use super::state::{ApiCapabilityState, Routed};
use crate::benchmark::require_trial_mode;

/// `POST /benchmark` — replay landed history across profile cells.
#[derive(Deserialize)]
pub(super) struct BenchmarkRequest {
    /// What to call this golden-task set.
    pub(super) set: String,
    /// The base every bloom seals on — the set's version (see
    /// [`GoldenTaskSet`](crate::benchmark::GoldenTaskSet)).
    pub(super) base: Digest,
    /// The landed pull requests to draw tasks from.
    pub(super) pull_requests: Vec<u64>,
    /// One recorded `aether.bloomery.model_override` address per profile cell.
    pub(super) cells: Vec<Digest>,
    /// How many members to seal per `(task, cell)`.
    pub(super) samples: u32,
    /// The recorded `aether.bloomery.model_process_instructions` bundle the
    /// bloom pins (ADR-0214). Named here rather than chosen by the door: a model
    /// attempt runs only under a bundle the host authorized, and a benchmark's
    /// cells are only comparable to live operation when they run its bundle.
    pub(super) instructions: Digest,
    /// Why this run is being made. Required, as at every operator door.
    pub(super) reason: String,
    /// Who is making it.
    pub(super) operator: String,
}

/// Gate the coordinator's class, then decode and validate the body.
///
/// Split out from the route so the refusal that matters most is provable without
/// a coordinator: a live-classed host answers `409` and reaches neither the
/// fixture nor the control core.
fn parse(store: StoreClass, body: &[u8]) -> Result<BenchmarkRequest, HttpServerResponse> {
    if let Err(refusal) = require_trial_mode(store) {
        return Err(error_response(409, &refusal.to_string()));
    }
    let request: BenchmarkRequest =
        hex::from_slice(body).map_err(|error| error_response(400, &format!("invalid benchmark body: {error}")))?;
    if let Some(refusal) = unstated(&request.reason, &request.operator) {
        return Err(refusal);
    }
    Ok(request)
}

/// `POST /benchmark` — extract the golden-task set, plan the run, and seal it.
pub(super) fn post(state: &ApiCapabilityState, ctx: &NativeCtx<'_, Manual>, request: &HttpServerRequest) -> Routed {
    let request = match parse(state.store_class, &request.body) {
        Ok(request) => request,
        Err(response) => return Routed::Reply(response),
    };

    seal::run(state, ctx, request)
}
