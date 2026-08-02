//! The HTTP response constructors every route and reply renderer answers
//! through: a JSON body, a structured JSON error, or raw artifact bytes.

use serde::Serialize;

use aether_http::{HttpHeader, HttpServerResponse};

use crate::api::dto::ErrorView;

/// A `Content-Type: application/json` header set.
fn json_headers() -> Vec<HttpHeader> {
    vec![HttpHeader { name: "content-type".to_owned(), value: "application/json".to_owned() }]
}

/// A JSON response over a serializable value; a `500` if it fails to encode.
pub(super) fn json(status: u16, value: &impl Serialize) -> HttpServerResponse {
    match serde_json::to_vec(value) {
        Ok(body) => HttpServerResponse { status, headers: json_headers(), body },
        Err(error) => error_response(500, &format!("response encode failed: {error}")),
    }
}

/// A structured JSON error body.
pub(super) fn error_response(status: u16, message: &str) -> HttpServerResponse {
    let body = serde_json::to_vec(&ErrorView { error: message.to_owned() }).unwrap_or_else(|_| message.into());
    HttpServerResponse { status, headers: json_headers(), body }
}

/// A raw `application/octet-stream` byte response (artifact bytes).
pub(super) fn bytes_response(status: u16, body: Vec<u8>) -> HttpServerResponse {
    HttpServerResponse {
        status,
        headers: vec![HttpHeader { name: "content-type".to_owned(), value: "application/octet-stream".to_owned() }],
        body,
    }
}
