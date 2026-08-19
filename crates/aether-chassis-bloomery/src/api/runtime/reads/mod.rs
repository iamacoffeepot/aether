//! The store reads — `GET /journal`, `GET /artifacts/{digest}`, and
//! `GET /artifacts/{digest}/decoded`. Routes defer to a peer cap; this
//! module pages, ranges, and decodes the replies so every HTTP body is
//! bounded.

mod artifacts;
mod journal;
mod query;

#[cfg(test)]
mod tests;

use aether_http::HttpServerResponse;

use super::hex::digest_from_hex;
use super::response::{bytes_response, error_response, json};
use crate::api::dto::DecodedArtifactView;
use crate::artifacts::{ArtifactsError, GetRangeResult};
use crate::store::PageJournalResult;

pub use artifacts::{ArtifactRange, range_bytes, resolve_kind};
pub use journal::{JournalPageError, page_journal};
pub use query::{ArtifactQuery, JournalQuery};
pub(in crate::api::runtime) use query::{clamp_limit, pairs, parse_u64};

/// Render the store's journal page as one bounded HTTP body.
pub fn journal_response(result: PageJournalResult) -> HttpServerResponse {
    match result {
        PageJournalResult::Ok { records, bloom, from_sequence, limit, descending, notice } => {
            let query = JournalQuery {
                bloom: bloom.as_deref().and_then(digest_from_hex),
                from_sequence,
                limit,
                descending,
                notice,
            };
            match page_journal(&records, &query) {
                Ok(view) => json(200, &view),
                Err(JournalPageError::Event { sequence, error }) => {
                    error_response(500, &format!("journal record {sequence} decode failed: {error}"))
                }
                Err(JournalPageError::Decisions { sequence, error }) => {
                    error_response(500, &format!("journal record {sequence} {error}"))
                }
            }
        }
        PageJournalResult::Err { error } => error_response(500, &error),
    }
}

/// Render an artifact range as bounded bytes, or as decoded JSON.
pub fn artifact_response(result: GetRangeResult) -> HttpServerResponse {
    match result {
        GetRangeResult::Ok { bytes, offset, limit, decoded, notice, .. } if decoded => {
            decoded_response(&bytes, offset, limit, notice)
        }
        GetRangeResult::Ok { bytes, total, offset, notice, truncated, .. } => {
            let mut response = bytes_response(200, bytes);
            let end = offset.saturating_add(u64::try_from(response.body.len()).unwrap_or(0)).saturating_sub(1);
            let range = if total == 0 {
                "bytes */0".to_owned()
            } else {
                format!("bytes {offset}-{end}/{total}")
            };
            response.headers.push(aether_http::HttpHeader { name: "content-range".to_owned(), value: range });
            let _ = truncated;
            if let Some(notice) = notice {
                response.headers.push(aether_http::HttpHeader { name: "x-aether-notice".to_owned(), value: notice });
            }
            response
        }
        GetRangeResult::Unsatisfiable { total, .. } => {
            let mut response = error_response(416, &format!("offset past end of artifact ({total} bytes)"));
            response
                .headers
                .push(aether_http::HttpHeader { name: "content-range".to_owned(), value: format!("bytes */{total}") });
            response
        }
        GetRangeResult::Err { error: ArtifactsError::NotFound, .. } => error_response(404, "no such artifact"),
        GetRangeResult::Err { error, .. } => error_response(500, &format!("artifacts error: {error:?}")),
    }
}

fn decoded_response(bytes: &[u8], offset: u64, limit: u64, notice: Option<String>) -> HttpServerResponse {
    if let Some((kind, value)) = resolve_kind(bytes) {
        return json(
            200,
            &DecodedArtifactView {
                kind: Some(kind.to_owned()),
                value: Some(value),
                bytes: None,
                offset: None,
                total: None,
                truncated: None,
                notice,
            },
        );
    }
    let query = ArtifactQuery { offset, limit, notice };
    // Unknown kind: apply the same range the raw route would have.
    match range_bytes(bytes, &query) {
        ArtifactRange::Ok { bytes, offset, total, truncated, notice } => json(
            200,
            &DecodedArtifactView {
                kind: None,
                value: None,
                bytes: Some(bytes),
                offset: Some(offset),
                total: Some(total),
                truncated: Some(truncated),
                notice,
            },
        ),
        ArtifactRange::Unsatisfiable { total } => {
            error_response(416, &format!("offset past end of artifact ({total} bytes)"))
        }
    }
}
