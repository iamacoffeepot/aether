//! The ephemeral response store behind `aether://mcp/response/<nonce>`.
//!
//! A tool output that crosses an inline ceiling is not truncated and not
//! leaf-substituted — the *complete* raw output is stored here and the tool
//! returns its address. Substituting the oversized leaf would change that
//! property's declared type (a `Bytes` field is an array of integers), so the
//! output would stop conforming to the `outputSchema` the tool advertises on
//! exactly the path a client is least likely to check.
//!
//! The store's one unusual rule is that it **never evicts an unexpired entry**.
//! A store that made room by dropping a live resource would hand out an address
//! and then invalidate it before its advertised lifetime, so a caller following
//! a resource link would see an intermittent, unreproducible `-32002`. Refusing
//! the *new* spill instead turns the same pressure into an immediate, local
//! `isError: true` on the call that caused it.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use serde_json::Value;
use uuid::Uuid;

use crate::protocol::remote_procedure_call::bounded_text;
use crate::protocol::resources::RESPONSE_RESOURCE_PREFIX;

/// Bytes of shape summary an addressed output carries.
///
/// Inherited from the outgoing coordinator's `RESPONSE_SUMMARY_MAX_BYTES`: a
/// summary describes structure and a bounded sample, and a summary that could
/// grow with its subject would re-expand the content the address exists to keep
/// out of the response.
pub const RESPONSE_SUMMARY_MAXIMUM_BYTES: usize = 2_048;

/// Milliseconds in one second, for the lifetime arithmetic below.
const MILLIS_PER_SEC: u64 = 1_000;

/// The store's ceilings, resolved from [`McpServerConfiguration`].
///
/// [`McpServerConfiguration`]: crate::McpServerConfiguration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseStoreLimits {
    /// Largest single stored response, in bytes.
    pub maximum_bytes: usize,
    /// Total resident bytes across every live entry.
    pub total_bytes: usize,
    /// Live entries the store will hold.
    pub maximum_entries: usize,
    /// Seconds an address stays readable.
    pub lifetime_secs: u64,
}

/// Why a spill was refused.
///
/// Every arm becomes `isError: true` on the call that produced the output. None
/// of them falls back to an oversized inline response: the ceilings exist to
/// bound what reaches a model context, and a fallback would make them advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreRefusal {
    /// The output alone is larger than one resource may be.
    ResourceTooLarge { bytes: usize, maximum_bytes: usize },
    /// Every entry slot is taken by a live resource.
    EntriesExhausted { maximum_entries: usize },
    /// The output does not fit in the remaining resident budget.
    TotalExhausted { bytes: usize, remaining_bytes: usize },
}

impl fmt::Display for StoreRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResourceTooLarge { bytes, maximum_bytes } => {
                write!(f, "output of {bytes} bytes exceeds the {maximum_bytes}-byte per-resource ceiling")
            }
            Self::EntriesExhausted { maximum_entries } => {
                write!(f, "all {maximum_entries} response-resource slots hold unexpired entries")
            }
            Self::TotalExhausted { bytes, remaining_bytes } => {
                write!(f, "output of {bytes} bytes exceeds the {remaining_bytes} bytes left in the response store")
            }
        }
    }
}

impl Error for StoreRefusal {}

/// One stored response and the instant it stops being readable.
struct StoredResponse {
    bytes: Vec<u8>,
    expires_at_millis: u64,
}

/// The in-memory store. Hub restart discards it, which is why these addresses
/// are for ephemeral results only; durable data uses a domain content hash.
pub struct ResponseStore {
    entries: BTreeMap<String, StoredResponse>,
    resident_bytes: usize,
    limits: ResponseStoreLimits,
}

impl ResponseStore {
    #[must_use]
    pub fn new(limits: ResponseStoreLimits) -> Self {
        Self { entries: BTreeMap::new(), resident_bytes: 0, limits }
    }

    /// Store `bytes` and return the address that reads them back.
    ///
    /// Expired entries are reclaimed first, so a store at its ceiling recovers
    /// on its own as addresses age out rather than needing a caller to prompt
    /// it. The three ceilings are then checked against the *reclaimed* store.
    pub fn store(&mut self, bytes: Vec<u8>, now_millis: u64) -> Result<String, StoreRefusal> {
        self.expire(now_millis);

        if bytes.len() > self.limits.maximum_bytes {
            return Err(StoreRefusal::ResourceTooLarge {
                bytes: bytes.len(),
                maximum_bytes: self.limits.maximum_bytes,
            });
        }
        if self.entries.len() >= self.limits.maximum_entries {
            return Err(StoreRefusal::EntriesExhausted { maximum_entries: self.limits.maximum_entries });
        }
        let remaining_bytes = self.limits.total_bytes.saturating_sub(self.resident_bytes);
        if bytes.len() > remaining_bytes {
            return Err(StoreRefusal::TotalExhausted { bytes: bytes.len(), remaining_bytes });
        }

        let uri = mint_response_uri();
        self.resident_bytes += bytes.len();
        self.entries.insert(
            uri.clone(),
            StoredResponse {
                bytes,
                expires_at_millis: now_millis.saturating_add(self.limits.lifetime_secs.saturating_mul(MILLIS_PER_SEC)),
            },
        );
        Ok(uri)
    }

    /// Read one address back, or `None` when it was never issued or has aged
    /// out. Both answer `-32002`; a caller that could tell them apart would
    /// learn whether a guessed nonce was ever live.
    pub fn read(&mut self, uri: &str, now_millis: u64) -> Option<&[u8]> {
        self.expire(now_millis);
        self.entries.get(uri).map(|stored| stored.bytes.as_slice())
    }

    /// Live entries. Test-facing — the ceilings are the behavior, and a test
    /// that asserts a refusal must be able to see the store it refused from.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Resident bytes across every live entry.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Drop every entry whose lifetime has elapsed.
    ///
    /// Iterative over a collected key list rather than `retain`, because the
    /// resident-byte total has to be decremented per removed entry and a
    /// `retain` predicate that mutated a sibling field would be doing its
    /// accounting inside a borrow of the map it is filtering.
    fn expire(&mut self, now_millis: u64) {
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, stored)| stored.expires_at_millis <= now_millis)
            .map(|(uri, _)| uri.clone())
            .collect();
        for uri in expired {
            if let Some(stored) = self.entries.remove(&uri) {
                self.resident_bytes = self.resident_bytes.saturating_sub(stored.bytes.len());
            }
        }
    }
}

/// Mint one ephemeral address.
///
/// The nonce is unpredictable rather than sequential: an address is the only
/// thing standing between a caller and another caller's output, so a counter
/// would make every live response guessable from one observed address. The
/// rendered form is 32 lowercase hexadecimal characters — a 128-bit address —
/// and the entropy behind it is the 122 random bits of a version-4 UUID, which
/// is the workspace's existing randomness seam and far past guessing.
fn mint_response_uri() -> String {
    format!("{RESPONSE_RESOURCE_PREFIX}{}", Uuid::new_v4().simple())
}

/// Describe a value's shape for an addressed output's `summary`.
///
/// It reports structure and one bounded sample, never expanded content — the
/// summary rides in the response the address exists to keep small, so a summary
/// that grew with its subject would defeat the addressing it describes.
#[must_use]
pub fn summarize(value: &Value) -> String {
    let summary = match value {
        Value::Object(members) => {
            let sample = members
                .iter()
                .take(3)
                .map(|(name, member)| format!("{name} is {}", describe_briefly(member)))
                .collect::<Vec<String>>()
                .join(", ");
            match members.len() {
                0 => "object with no keys".to_string(),
                count => format!("object with {count} keys; {sample}"),
            }
        }
        Value::Array(items) => format!("array with {} entries", items.len()),
        other => describe_briefly(other),
    };

    bounded_text(&summary, RESPONSE_SUMMARY_MAXIMUM_BYTES)
}

/// One clause of a summary: a container's size, or a scalar's type.
fn describe_briefly(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "a boolean".to_string(),
        Value::Number(_) => "a number".to_string(),
        Value::String(text) => format!("a {}-byte string", text.len()),
        Value::Array(items) => format!("{} entries", items.len()),
        Value::Object(members) => format!("{} keys", members.len()),
    }
}
