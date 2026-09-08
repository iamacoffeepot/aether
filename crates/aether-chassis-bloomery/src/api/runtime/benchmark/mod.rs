//! `POST /benchmark` and `GET /benchmark/{run}` — the operator doors onto a
//! benchmark run (ADR-0184, issue #4871).
//!
//! Shaped like the operator doors next door (`/blooms/{id}/hold`,
//! `/blooms/{id}/members/{workpiece}/repair`, `/blooms/{id}/supersede`): a JSON
//! body that has to state a `reason` and name an `operator`, refused through the
//! same [`unstated`] gate. No new authentication path is invented here — a
//! benchmark run is an operator decision about the pipeline, which is exactly
//! what those doors already are.
//!
//! What is new is the **trial-mode gate**, and it comes first: a coordinator
//! whose journal is live-classed is refused `409` before its body is even
//! parsed. That is a refusal and never a warning, because a benchmark's blooms
//! are indistinguishable from live ones once they are in a journal — the store's
//! class is the only thing that separates them (ADR-0184, issue #5794).
//!
//! # The door does not wait
//!
//! A run is a *sequence* of blooms, which is minutes to hours of lifetimes, so
//! the start answers `202` with a handle and returns. Both routes are plain
//! ADR-0154 relays onto
//! [`BenchmarkRunnerCapability`](crate::benchmark::BenchmarkRunnerCapability),
//! which owns the sequence; nothing about a run is held here, because nothing
//! about it fits inside one request. The runner is mounted on every build — with
//! a fixture in trial mode and without one everywhere else — so the routes need
//! no build-shape gate of their own: a coordinator that mounts none refuses the
//! run and says why.

#[cfg(test)]
mod tests;

use aether_bloomery::{Digest, StoreClass};
use aether_http::{HttpServerRequest, HttpServerResponse};
use serde::{Deserialize, Serialize};

use super::blooms::unstated;
use super::hex;
use super::response::{error_response, json};
use super::state::{ApiCapabilityState, Routed};
use crate::benchmark::{ReadBenchmarkResult, StartBenchmark, StartBenchmarkResult, require_trial_mode};

/// `POST /benchmark` — replay landed history across profile cells.
#[derive(Deserialize)]
pub(super) struct BenchmarkRequest {
    /// What to call this golden-task set.
    pub(super) set: String,
    /// The base every cell replays over — the set's version (see
    /// [`GoldenTaskSet`](crate::benchmark::GoldenTaskSet)) and the commit
    /// mainline is reset to between cells.
    pub(super) base: Digest,
    /// The landed pull requests to draw tasks from.
    pub(super) pull_requests: Vec<u64>,
    /// One recorded `aether.bloomery.model_override` address per profile cell.
    pub(super) cells: Vec<Digest>,
    /// How many blooms to seal per `(task, cell)`.
    pub(super) samples: u32,
    /// The recorded `aether.bloomery.model_process_instructions` bundle every
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
/// fixture nor the runner.
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

/// `POST /benchmark` — start a run and answer with its handle.
pub(super) fn post(state: &ApiCapabilityState, request: &HttpServerRequest) -> Routed {
    match parse(state.store_class, &request.body) {
        Err(response) => Routed::Reply(response),
        Ok(request) => Routed::StartBenchmark(StartBenchmark {
            set: request.set,
            base: request.base,
            pull_requests: request.pull_requests,
            cells: request.cells,
            samples: request.samples,
            instructions: request.instructions,
        }),
    }
}

/// Render the runner's answer to a start.
pub(super) fn start_response(result: StartBenchmarkResult) -> HttpServerResponse {
    match result {
        // `202`, not `200`: the run is accepted and under way, and what comes
        // back is a handle rather than an answer.
        StartBenchmarkResult::Accepted { run } => json(202, &StartedView { run }),
        StartBenchmarkResult::Refused { error } => error_response(422, &error),
    }
}

/// Render the runner's answer to a read.
pub(super) fn read_response(result: ReadBenchmarkResult) -> HttpServerResponse {
    match result {
        ReadBenchmarkResult::Ok { run } => json(200, &run),
        ReadBenchmarkResult::NotFound => error_response(
            404,
            "no benchmark run under that handle; a run is trial-mode bookkeeping held in memory, so a coordinator \
             restart ends the one in flight and its handle with it",
        ),
    }
}

/// What a started run answers with.
#[derive(Serialize)]
struct StartedView {
    /// The handle `GET /benchmark/{run}` reads.
    run: u64,
}
