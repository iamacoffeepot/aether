//! Dispatch evidence, transcript, and coordinator-log reads (war-room slice 9).
//!
//! Rollup + outstanding come from the store. The evidence directory is on the
//! fleet-host filesystem; only this process can tell a swept nonce from one
//! that never existed. Every response is buffered and bounded.

mod header;
mod list;
mod logs;
mod ranged;

#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};

use aether_http::HttpServerResponse;

use super::hex::digest_from_hex;
use super::response::{error_response, json};
use super::state::Routed;
use crate::api::dto::DispatchFilePage;
use crate::artifacts::ArtifactsCapabilityState;
use crate::store::{ListBloomDispatches, ListBloomDispatchesResult, LookupDispatch, LookupDispatchResult};

const EVIDENCE_SUFFIX: &str = "-evidence";
const ARCHIVE_EVIDENCE_DIR: &str = "evidence";
const SWEPT_NOTICE: &str = "evidence directory was reclaimed";

/// `GET /blooms/{id}/dispatches`
pub(in crate::api::runtime) fn list_dispatches(id: &str) -> Result<Routed, String> {
    let bloom = digest_from_hex(id).ok_or_else(|| format!("bloom is not a 64-character hex digest: {id}"))?;
    Ok(Routed::ListBloomDispatches(ListBloomDispatches { bloom: bloom.as_bytes().to_vec() }))
}

/// Render the store's bloom-dispatch list, joining filesystem retention and
/// study cost.
pub(in crate::api::runtime) fn list_response(
    worktree_base: &Path,
    archive_base: &Path,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    result: ListBloomDispatchesResult,
) -> HttpServerResponse {
    match result {
        ListBloomDispatchesResult::Ok { rollup, outstanding } => {
            json(200, &list::assemble(worktree_base, archive_base, artifacts, &rollup, &outstanding))
        }
        ListBloomDispatchesResult::Err { error } => error_response(500, &error),
    }
}

/// `GET /dispatches/{nonce}`
pub(in crate::api::runtime) fn lookup_dispatch(nonce: &str) -> Routed {
    Routed::LookupDispatch(LookupDispatch { nonce: nonce.to_owned() })
}

/// Render one dispatch header. A nonce the journal never named is `404`; a
/// named nonce whose directory is gone is `200` with `retained: false`.
pub(in crate::api::runtime) fn header_response(
    worktree_base: &Path,
    archive_base: &Path,
    result: LookupDispatchResult,
) -> HttpServerResponse {
    match result {
        LookupDispatchResult::Ok { nonce, .. } => json(200, &header::read(worktree_base, archive_base, &nonce)),
        LookupDispatchResult::NotFound => error_response(404, "no such dispatch"),
        LookupDispatchResult::Err { error } => error_response(500, &error),
    }
}

/// `GET /dispatches/{nonce}/transcript` and `/prompt` — ranged file read.
pub(in crate::api::runtime) fn file_page(
    worktree_base: &Path,
    archive_base: &Path,
    nonce: &str,
    file: &str,
    query: &str,
) -> HttpServerResponse {
    let parsed = match ranged::FileQuery::parse(query) {
        Ok(parsed) => parsed,
        Err(error) => return error_response(400, &error),
    };
    match read_named_file(worktree_base, archive_base, nonce, file, parsed.cursor, parsed.limit) {
        Ok(page) => {
            let mut page = page;
            page.notice = parsed.notice;
            json(200, &page)
        }
        Err(FileReadError::Missing) => error_response(404, &format!("{file} is not retained")),
        Err(FileReadError::Io(error)) => error_response(500, &format!("evidence read failed: {error}")),
    }
}

/// `GET /logs/coordinator`
pub(in crate::api::runtime) fn coordinator_logs(query: &str) -> HttpServerResponse {
    match logs::read(query, logs::journalctl) {
        Ok(view) => json(200, &view),
        Err(logs::LogError::Unavailable { reason }) => error_response(501, &reason),
        Err(logs::LogError::BadQuery(error)) => error_response(400, &error),
        Err(logs::LogError::Io(error)) => error_response(500, &error),
    }
}

fn read_named_file(
    worktree_base: &Path,
    archive_base: &Path,
    nonce: &str,
    file: &str,
    cursor: Option<u64>,
    limit: u64,
) -> Result<DispatchFilePage, FileReadError> {
    let Some(dir) = resolve_evidence_dir(worktree_base, archive_base, nonce) else {
        return Err(FileReadError::Missing);
    };
    match ranged::read_ranged(&dir.join(file), cursor, limit) {
        Ok(page) => Ok(page),
        Err(ranged::RangedError::NotFound) => Err(FileReadError::Missing),
        Err(ranged::RangedError::Io(error)) => Err(FileReadError::Io(error)),
    }
}

enum FileReadError {
    Missing,
    Io(io::Error),
}

fn evidence_dir(worktree_base: &Path, nonce: &str) -> PathBuf {
    worktree_base.join(format!("{nonce}{EVIDENCE_SUFFIX}"))
}

fn archived_evidence_dir(archive_base: &Path, nonce: &str) -> PathBuf {
    archive_base.join(ARCHIVE_EVIDENCE_DIR).join(format!("{nonce}{EVIDENCE_SUFFIX}"))
}

/// Working root first, then the tier's evidence subdirectory.
fn resolve_evidence_dir(worktree_base: &Path, archive_base: &Path, nonce: &str) -> Option<PathBuf> {
    for spelling in nonce_spellings(nonce) {
        let working = evidence_dir(worktree_base, &spelling);
        if working.is_dir() {
            return Some(working);
        }
        let archived = archived_evidence_dir(archive_base, &spelling);
        if archived.is_dir() {
            return Some(archived);
        }
    }
    None
}

fn evidence_retained(worktree_base: &Path, archive_base: &Path, nonce: &str) -> bool {
    resolve_evidence_dir(worktree_base, archive_base, nonce).is_some()
}

fn archived_location(worktree_base: &Path, archive_base: &Path, nonce: &str) -> Option<PathBuf> {
    for spelling in nonce_spellings(nonce) {
        if evidence_dir(worktree_base, &spelling).is_dir() {
            return None;
        }
        let archived = archived_evidence_dir(archive_base, &spelling);
        if archived.is_dir() {
            return Some(archived);
        }
    }
    None
}

fn nonce_spellings(nonce: &str) -> Vec<String> {
    let mut spellings = vec![nonce.to_owned()];
    if let Some(rest) = nonce.strip_prefix("dispatch-") {
        spellings.push(format!("redispatch-{rest}"));
    } else if let Some(rest) = nonce.strip_prefix("redispatch-") {
        spellings.push(format!("dispatch-{rest}"));
    }
    spellings
}

fn is_host_nonce(nonce: &str) -> bool {
    nonce.starts_with("dispatch-") || nonce.starts_with("redispatch-")
}
