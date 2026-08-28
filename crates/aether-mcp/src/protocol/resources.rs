//! `resources/list`, `resources/read`, and the URI grammar both depend on.
//!
//! Unlike `tools/call`, a provider-backed read stays a protocol request after
//! dispatch: there is no `isError` convention on this surface, so a
//! not-found is `-32002` and any other provider or adapter failure is
//! `-32603`. A read never returns a host path — an address is the only thing
//! a caller learns about where content lives.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map, Value, json};
use std::error::Error;
use std::fmt;

use crate::kinds::ResourceDescriptor;

use super::remote_procedure_call::{ProtocolError, parse_list_params, reject_unknown_members, required_string};

/// The scheme every resource address uses.
pub const RESOURCE_SCHEME: &str = "aether";

/// The prefix reserved to the server's own ephemeral response store.
///
/// No provider may claim it: an address under it is minted by the server from
/// an unpredictable nonce, and a provider that could claim the prefix could
/// answer for addresses it never issued.
pub const RESPONSE_RESOURCE_PREFIX: &str = "aether://mcp/response/";

/// Why an address is not a valid resource URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UriError {
    pub reason: &'static str,
}

impl fmt::Display for UriError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason)
    }
}

impl Error for UriError {}

fn refuse(reason: &'static str) -> UriError {
    UriError { reason }
}

/// Normalize a resource address.
///
/// The grammar is deliberately small — lowercase `aether://` scheme and
/// authority, an absolute path of non-empty segments, and nothing else. No
/// query, fragment, user information, percent encoding, `.`, or `..`. Every
/// one of those would give the same resource two spellings, and
/// longest-prefix dispatch over a set of alternate spellings is how a request
/// escapes the authority of the provider that was supposed to answer it.
///
/// Parsing happens before prefix matching, always: the server never matches
/// an unchecked raw string.
pub fn normalize_resource_uri(raw: &str) -> Result<String, UriError> {
    let rest = raw.strip_prefix(RESOURCE_SCHEME).ok_or_else(|| refuse("the scheme must be `aether`"))?;
    let rest = rest.strip_prefix("://").ok_or_else(|| refuse("the scheme must be followed by `://`"))?;

    if raw.contains(['?', '#', '%', '@']) {
        return Err(refuse("query, fragment, percent encoding, and user information are not accepted"));
    }

    let (authority, path) = rest.split_once('/').ok_or_else(|| refuse("the path must be absolute"))?;
    if authority.is_empty() || !is_lowercase_host(authority) {
        return Err(refuse("the authority must be a non-empty lowercase host"));
    }

    // A trailing empty segment is how a provider prefix ends, so it is
    // allowed once, at the end, and nowhere else.
    let segments: Vec<&str> = path.split('/').collect();
    let Some((_, leading)) = segments.split_last() else {
        return Err(refuse("the path must be absolute"));
    };
    if leading.iter().any(|segment| segment.is_empty()) {
        return Err(refuse("empty path segments are not accepted"));
    }
    if segments.iter().any(|segment| *segment == "." || *segment == "..") {
        return Err(refuse("`.` and `..` path segments are not accepted"));
    }

    // Nothing above is a rewrite: every alternate spelling is refused rather
    // than folded, so an accepted address already *is* its normal form. That
    // is deliberate — a normalizer that rewrote input would have to be
    // idempotent and agree exactly with every provider's own comparison, and
    // rejecting is the property that is easy to hold.
    Ok(raw.to_string())
}

/// Normalize a provider's prefix claim.
///
/// A prefix must end in `/`, so `aether://bloomery/artifacts/` cannot match
/// `aether://bloomery/artifacts-private/x`. Without that rule one provider's
/// claim would silently reach into a sibling's namespace.
pub fn normalize_provider_prefix(raw: &str) -> Result<String, UriError> {
    let normalized = normalize_resource_uri(raw)?;
    if !normalized.ends_with('/') {
        return Err(refuse("a provider prefix must end in `/`"));
    }
    Ok(normalized)
}

fn is_lowercase_host(authority: &str) -> bool {
    authority.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-'))
}

/// Render a discoverable descriptor in the protocol's spelling.
///
/// The wire kind names its fields `mime_type` and `size_bytes`; the protocol
/// names them `mimeType` and `size`. The translation lives here so the kind
/// stays in Aether's naming and the wire stays in the protocol's.
#[must_use]
pub fn descriptor_to_json(descriptor: &ResourceDescriptor) -> Value {
    let mut rendered = Map::new();
    rendered.insert("uri".to_string(), json!(descriptor.uri));
    rendered.insert("name".to_string(), json!(descriptor.name));
    for (key, value) in [("title", &descriptor.title), ("description", &descriptor.description)] {
        if let Some(value) = value {
            rendered.insert(key.to_string(), json!(value));
        }
    }
    if let Some(mime_type) = &descriptor.mime_type {
        rendered.insert("mimeType".to_string(), json!(mime_type));
    }
    if let Some(size_bytes) = descriptor.size_bytes {
        rendered.insert("size".to_string(), json!(size_bytes));
    }
    Value::Object(rendered)
}

/// Parse `resources/list`.
pub fn parse_list_resources(params: Option<&Value>) -> Result<(), ProtocolError> {
    parse_list_params(params)
}

/// The `resources/list` result: the frozen discoverable catalog, by name.
///
/// Only explicitly registered discoverable entries appear. Ephemeral tool
/// results and content-addressed artifacts are legitimately unlisted — a tool
/// resource link may name a resource that was never listed — so listing them
/// would be an unbounded catalog of addresses nobody asked for.
#[must_use]
pub fn list_resources_result(descriptors: &[ResourceDescriptor]) -> Value {
    let mut sorted: Vec<&ResourceDescriptor> = descriptors.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));

    json!({ "resources": sorted.iter().map(|descriptor| descriptor_to_json(descriptor)).collect::<Vec<Value>>() })
}

/// Parse `resources/read`, normalizing the address before it is matched.
pub fn parse_read_resource(params: Option<&Value>) -> Result<String, ProtocolError> {
    reject_unknown_members(params, &["uri"])?;
    let uri = required_string(params, "uri")?;

    normalize_resource_uri(&uri).map_err(|error| ProtocolError::invalid_params(format!("invalid `uri`: {error}")))
}

/// The `resources/read` result for text content.
#[must_use]
pub fn read_resource_text_result(uri: &str, mime_type: &str, text: &str) -> Value {
    json!({ "contents": [{ "uri": uri, "mimeType": mime_type, "text": text }] })
}

/// The `resources/read` result for binary content.
///
/// The provider hands over raw bytes and this is the only place they are
/// base64-encoded — a provider that pre-encoded would be trusted to have
/// chosen the same alphabet and padding, and a mismatch would be invisible
/// until a client decoded garbage.
#[must_use]
pub fn read_resource_blob_result(uri: &str, mime_type: &str, bytes: &[u8]) -> Value {
    json!({ "contents": [{ "uri": uri, "mimeType": mime_type, "blob": BASE64.encode(bytes) }] })
}

/// How a provider's read failure becomes a protocol error.
///
/// `not_found` is the one category with a protocol code of its own; the rest
/// are the server's problem to describe, not the caller's to act on, so they
/// carry a bounded internal diagnostic.
#[must_use]
pub fn protocol_error_for_read_failure(uri: &str, category: &str, message: &str) -> ProtocolError {
    if category == "not_found" {
        return ProtocolError::resource_not_found(uri);
    }
    ProtocolError::internal_error(format!("{category}: {message}"))
}
