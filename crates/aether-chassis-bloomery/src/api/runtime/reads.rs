//! The store reads — `GET /journal` and `GET /artifacts/{digest}`. Both routes
//! dispatch to a peer cap and defer, so what lives here is the rendering half:
//! turning the store's and the artifacts cap's replies into HTTP responses.

use aether_bloomery::{Event, ReplayJournalResult};
use aether_data::wire::from_bytes;
use aether_http::HttpServerResponse;

use super::response::{bytes_response, error_response, json};
use crate::api::dto::{JournalEntry, JournalView};
use crate::artifacts::{ArtifactsError, GetResult};

/// Render the store's [`ReplayJournalResult`] into its HTTP response: every
/// journaled event decoded, oldest first.
pub(super) fn journal_response(result: ReplayJournalResult) -> HttpServerResponse {
    match result {
        ReplayJournalResult::Ok { records } => {
            let mut entries = Vec::with_capacity(records.len());
            for record in records {
                match from_bytes::<Event>(&record.event) {
                    Ok(event) => entries.push(JournalEntry {
                        sequence: record.sequence,
                        idempotency_key: record.idempotency_key,
                        event,
                    }),
                    Err(error) => {
                        return error_response(
                            500,
                            &format!("journal record {} decode failed: {error}", record.sequence),
                        );
                    }
                }
            }
            json(200, &JournalView { records: entries })
        }
        ReplayJournalResult::Err { error } => error_response(500, &error),
    }
}

/// Render the artifacts cap's [`GetResult`] into its HTTP response: the raw
/// bytes, a `404`, or the error.
pub(super) fn artifact_response(result: GetResult) -> HttpServerResponse {
    match result {
        GetResult::Ok { bytes, .. } => bytes_response(200, bytes),
        GetResult::Err { error: ArtifactsError::NotFound, .. } => error_response(404, "no such artifact"),
        GetResult::Err { error, .. } => error_response(500, &format!("artifacts error: {error:?}")),
    }
}
