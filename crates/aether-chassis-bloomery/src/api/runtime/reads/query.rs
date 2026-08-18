//! Query-string parse for the journal and artifact read routes.
//!
//! A limit above the clamp is applied and named in `notice`, never refused.

use aether_bloomery::Digest;
use aether_http::{FromRequest, HttpServerRequest, HttpServerResponse};

use super::super::hex::digest_from_hex;
use super::super::response::error_response;

/// Default page size for `GET /journal`.
pub const JOURNAL_DEFAULT_LIMIT: u64 = 100;
/// Hard ceiling for `GET /journal` `limit`.
pub const JOURNAL_MAX_LIMIT: u64 = 1_000;
/// Default byte range for `GET /artifacts/{digest}`.
pub const ARTIFACT_DEFAULT_LIMIT: u64 = 128 * 1024;
/// Hard ceiling for an artifact range.
pub const ARTIFACT_MAX_LIMIT: u64 = 512 * 1024;

/// Parsed `GET /journal` query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalQuery {
    /// Restrict to events that name this bloom. `None` is every record.
    pub bloom: Option<Digest>,
    /// Exclusive cursor: descending pages take `sequence < from`, ascending
    /// pages take `sequence > from`. `None` starts at the newest / oldest.
    pub from_sequence: Option<u64>,
    /// Applied page size, already clamped.
    pub limit: u64,
    /// Newest-first when true (the default).
    pub descending: bool,
    /// Set when the caller named a limit above the clamp.
    pub notice: Option<String>,
}

/// Parsed `GET /artifacts/{digest}` range query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactQuery {
    /// Byte offset into the artifact. `0` when omitted.
    pub offset: u64,
    /// Applied length, already clamped.
    pub limit: u64,
    /// Set when the caller named a limit above the clamp.
    pub notice: Option<String>,
}

impl JournalQuery {
    /// Parse `query`. An unknown `order` or an unparseable number is a `400`.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason when a parameter cannot be applied.
    pub fn parse(query: &str) -> Result<Self, String> {
        let mut bloom = None;
        let mut from_sequence = None;
        let mut requested_limit = None;
        let mut descending = true;
        for (key, value) in pairs(query) {
            match key {
                "bloom" => {
                    bloom = Some(
                        digest_from_hex(value)
                            .ok_or_else(|| format!("bloom is not a 64-character hex digest: {value}"))?,
                    );
                }
                "from_sequence" => from_sequence = Some(parse_u64("from_sequence", value)?),
                "limit" => requested_limit = Some(parse_u64("limit", value)?),
                "order" => descending = parse_order(value)?,
                _ => {}
            }
        }
        let (limit, notice) = clamp_limit(requested_limit, JOURNAL_DEFAULT_LIMIT, JOURNAL_MAX_LIMIT);
        Ok(Self { bloom, from_sequence, limit, descending, notice })
    }
}

impl ArtifactQuery {
    /// Parse `query`. An unparseable number is a `400`.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason when a parameter cannot be applied.
    pub fn parse(query: &str) -> Result<Self, String> {
        let mut offset = 0;
        let mut requested_limit = None;
        for (key, value) in pairs(query) {
            match key {
                "offset" => offset = parse_u64("offset", value)?,
                "limit" => requested_limit = Some(parse_u64("limit", value)?),
                _ => {}
            }
        }
        let (limit, notice) = clamp_limit(requested_limit, ARTIFACT_DEFAULT_LIMIT, ARTIFACT_MAX_LIMIT);
        Ok(Self { offset, limit, notice })
    }
}

impl FromRequest for JournalQuery {
    fn from_request(request: &HttpServerRequest) -> Result<Self, HttpServerResponse> {
        Self::parse(&request.query).map_err(|error| error_response(400, &error))
    }
}

impl FromRequest for ArtifactQuery {
    fn from_request(request: &HttpServerRequest) -> Result<Self, HttpServerResponse> {
        Self::parse(&request.query).map_err(|error| error_response(400, &error))
    }
}

fn pairs(query: &str) -> impl Iterator<Item = (&str, &str)> {
    query.split('&').filter(|pair| !pair.is_empty()).map(|pair| pair.split_once('=').unwrap_or((pair, "")))
}

fn parse_u64(name: &str, value: &str) -> Result<u64, String> {
    value.parse::<u64>().map_err(|_| format!("{name} is not an integer: {value}"))
}

fn parse_order(value: &str) -> Result<bool, String> {
    match value {
        "desc" | "DESC" => Ok(true),
        "asc" | "ASC" => Ok(false),
        other => Err(format!("order must be asc or desc, not {other}")),
    }
}

fn clamp_limit(requested: Option<u64>, default: u64, max: u64) -> (u64, Option<String>) {
    match requested {
        None => (default, None),
        Some(limit) if limit > max => (max, Some(format!("limit clamped from {limit} to {max}"))),
        Some(limit) => (limit, None),
    }
}
