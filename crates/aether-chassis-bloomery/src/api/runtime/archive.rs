//! `POST /archive` and `GET /archive` — the operator archive pass (ADR-0211).

use aether_http::{HttpServerRequest, HttpServerResponse};

use super::commissions::authorize;
use super::response::{error_response, json};
use super::state::{ApiCapabilityState, Routed};
use crate::api::dto::{ArchiveFailureView as ArchiveFailureHttp, ArchiveListView, ArchivePassView, ArchiveRecordView};

#[cfg(feature = "github")]
use crate::bloomery::{
    ArchiveFailureView, ArchiveRecords, ArchiveRecordsResult, ArchivedRecordView, ListArchive, ListArchiveResult,
};

#[cfg(not(feature = "github"))]
const UNAVAILABLE: &str = "archive needs the coordinator janitor";

/// `POST /archive` — run the between-blooms archive pass.
pub(super) fn post(state: &ApiCapabilityState, request: &HttpServerRequest) -> Routed {
    if let Err(response) = authorize(request, &state.control_token) {
        return Routed::Reply(response);
    }
    #[cfg(feature = "github")]
    {
        Routed::ArchiveRecords(ArchiveRecords::default())
    }
    #[cfg(not(feature = "github"))]
    Routed::Reply(error_response(503, UNAVAILABLE))
}

/// `GET /archive` — list the tier.
pub(super) fn list(state: &ApiCapabilityState, request: &HttpServerRequest) -> Routed {
    if let Err(response) = authorize(request, &state.control_token) {
        return Routed::Reply(response);
    }
    #[cfg(feature = "github")]
    {
        Routed::ListArchive(ListArchive::default())
    }
    #[cfg(not(feature = "github"))]
    Routed::Reply(error_response(503, UNAVAILABLE))
}

/// Render the pass result. A between-blooms refusal is `409`.
#[cfg(feature = "github")]
pub(super) fn pass_response(result: ArchiveRecordsResult) -> HttpServerResponse {
    match result {
        ArchiveRecordsResult::Archived { records, failures } => json(
            200,
            &ArchivePassView {
                records: records.into_iter().map(http_record).collect(),
                failures: failures.into_iter().map(http_failure).collect(),
            },
        ),
        ArchiveRecordsResult::Refused { reason } => error_response(409, &reason),
        ArchiveRecordsResult::Errored { error } => error_response(500, &error),
    }
}

/// Render the tier listing.
#[cfg(feature = "github")]
pub(super) fn list_response(result: ListArchiveResult) -> HttpServerResponse {
    match result {
        ListArchiveResult::Ok { records } => {
            json(200, &ArchiveListView { records: records.into_iter().map(http_record).collect() })
        }
        ListArchiveResult::Err { error } => error_response(500, &error),
    }
}

#[cfg(feature = "github")]
fn http_record(record: ArchivedRecordView) -> ArchiveRecordView {
    ArchiveRecordView { class: record.class, name: record.name, path: record.path, bytes: record.bytes }
}

#[cfg(feature = "github")]
fn http_failure(failure: ArchiveFailureView) -> ArchiveFailureHttp {
    ArchiveFailureHttp { class: failure.class, name: failure.name, error: failure.error }
}
