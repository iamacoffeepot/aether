//! Wire kinds owned by the two HTTP capabilities (ADR-0121): the egress
//! client (`client.rs`) and the ingress server (`server.rs`). The
//! substrate core dispatches none of them, so they live with the
//! capabilities that own them rather than in `aether-kinds`. The module
//! is always-on and wasm-safe — the types depend only on
//! `aether_data::{Kind, Schema}` and serde, so the
//! `default-features = false` wasm consumers keep compiling.

use serde::{Deserialize, Serialize};

// ADR-0043 substrate HTTP egress. One request kind + one reply
// kind on the `"aether.http"` sink, plus supporting `HttpMethod`,
// `HttpHeader`, and `HttpError` shapes. All structured
// (Strings, Vecs, Option<u32>).
//
// Reply correlation follows the ADR-0041 pattern: the reply
// echoes the originating `url` so callers match reply-to-request
// without threading a pending-op queue. Request `body` is not
// echoed — correlation needs the identity of the request, not
// its contents, and a multi-MB upload should not round-trip its
// bytes. Components needing strict per-op correlation (same URL
// fired back-to-back, non-idempotent POST) lean on ADR-0042's
// per-Source correlation ids via `prev_correlation_p32` rather
// than a per-kind field.

/// HTTP method carried on `Fetch`. Enumerating at the schema
/// layer keeps `"get"` / `"GET"` / `"Get"` from disagreeing
/// across guests; the substrate maps each variant to its
/// canonical uppercase name when calling the HTTP backend.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Head,
    Options,
}

/// One HTTP header on a `Fetch` request or `FetchResult`
/// response. Expressed as a named-field struct because
/// `aether_data::Schema` has no blanket impl for tuples — if
/// that lands later the wire shape here is source-compatible
/// (same two fields in the same order).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

/// Structured failure reason for an HTTP request (ADR-0043 §1).
/// Typed variants cover the branches agents routinely need to
/// match on — `Timeout` → retry, `AllowlistDenied` → config
/// issue, `BodyTooLarge` → chunk the response, `Disabled` →
/// surface to the operator. `InvalidUrl` carries the offending
/// URL text; `AdapterError` is the catchall preserving backend-
/// specific detail (DNS failure, TLS handshake, connection
/// refused, etc.) as free-form text.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    InvalidUrl(String),
    Timeout,
    BodyTooLarge,
    AllowlistDenied,
    Disabled,
    AdapterError(String),
}

/// `aether.http.fetch` — request the substrate perform an HTTP
/// request and reply with the response. Mailed to the
/// `"aether.http"` sink; reply lands via `reply_mail` as
/// `FetchResult`.
/// `timeout_ms` overrides the chassis default
/// (`AETHER_HTTP_TIMEOUT_MS`, default 30000) when set; `None`
/// uses the default.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.fetch")]
pub struct Fetch {
    pub url: String,
    pub method: HttpMethod,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
    pub timeout_ms: Option<u32>,
}

/// Reply to `Fetch`. Both arms echo the originating `url` so the
/// caller correlates reply-to-request without threading a
/// pending-op queue — operation identity comes from the reply
/// kind itself (`aether.http.fetch_result`). Request `body` is
/// deliberately not echoed: correlation needs the identity of
/// the request, not its contents, and a multi-MB upload should
/// not round-trip. `Ok` carries the HTTP status, response
/// headers, and response body (bounded by
/// `AETHER_HTTP_MAX_BODY_BYTES`, default 16MB); `Err` carries an
/// `HttpError` variant.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.fetch_result")]
pub enum FetchResult {
    Ok {
        url: String,
        status: u16,
        headers: Vec<HttpHeader>,
        body: Vec<u8>,
    },
    Err {
        url: String,
        error: HttpError,
    },
}

// ADR-0108 HTTP server kinds. Two public kinds shared by the server
// capability (#1760) and the handler component (#1762): an inbound
// request delivered to the handler, and an outbound response returned
// by the handler. Both reuse `HttpMethod` / `HttpHeader` from ADR-0043
// so the inbound vocabulary is symmetric with the client.

/// Inbound HTTP request delivered to a handler component by the server
/// capability (ADR-0108). `query` is always present — empty string when
/// the URL carries no query component. `body` is raw bytes so binary
/// uploads round-trip without loss.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.request")]
pub struct HttpServerRequest {
    pub method: HttpMethod,
    pub path: String,
    pub query: String,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

/// Outbound HTTP response produced by a handler component and forwarded
/// to the waiting client by the server capability (ADR-0108). `status`
/// is the raw HTTP status code; `body` is raw bytes so binary responses
/// round-trip without loss.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.response")]
pub struct HttpServerResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

/// `aether.http.server.inbound_ready` — accept / reader sidecar →
/// `HttpServerCapability` dispatcher wake (ADR-0108, issue 1760).
/// The HTTP-server analog of `RpcInboundReady`: the sidecar pushes
/// the live work (an accepted `TcpStream`, a parsed request, a close
/// reason) over the cap's internal mpsc and fires this empty-payload
/// mail at the cap's own mailbox so the dispatcher handler drains the
/// queue. A `TcpStream` isn't wire-shaped and a request body may be
/// large, so the mail is only the wakeup signal.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.http.server.inbound_ready")]
pub struct HttpInboundReady {}

#[cfg(test)]
mod tests {
    // ADR-0043 HTTP kind roundtrips. `Fetch` carries String + typed
    // method + Vec<HttpHeader> + Vec<u8> body + Option<u32>;
    // `FetchResult` mirrors `ReadResult`'s Ok/Err split with a
    // typed error arm wrapping `HttpError`. Tests prove the derived
    // Serialize/Deserialize agree on the wire for each shape, with
    // special attention to the `body`-not-echoed invariant and the
    // payload-carrying `HttpError` variants.
    use super::*;
    use aether_data::{Kind, wire};

    fn sample_headers() -> Vec<HttpHeader> {
        vec![
            HttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            HttpHeader {
                name: "user-agent".to_string(),
                value: "aether/0.2".to_string(),
            },
        ]
    }

    #[test]
    fn fetch_request_roundtrip() {
        let f = Fetch {
            url: "https://api.example.com/v1/resource".to_string(),
            method: HttpMethod::Post,
            headers: sample_headers(),
            body: vec![b'{', b'}'],
            timeout_ms: Some(5000),
        };
        let bytes = f.encode_into_bytes();
        let back: Fetch =
            Fetch::decode_from_bytes(&bytes).expect("test setup: kind codec decodes Fetch");
        assert_eq!(back.url, f.url);
        assert_eq!(back.method, HttpMethod::Post);
        assert_eq!(back.headers, f.headers);
        assert_eq!(back.body, vec![b'{', b'}']);
        assert_eq!(back.timeout_ms, Some(5000));
    }

    #[test]
    fn fetch_request_roundtrip_no_timeout() {
        let f = Fetch {
            url: "https://api.example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout_ms: None,
        };
        let bytes = f.encode_into_bytes();
        let back: Fetch = Fetch::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes Fetch (no timeout)");
        assert_eq!(back.timeout_ms, None);
        assert_eq!(back.method, HttpMethod::Get);
    }

    #[test]
    fn fetch_result_ok_roundtrip_echoes_url() {
        let r = FetchResult::Ok {
            url: "https://api.example.com/v1/resource".to_string(),
            status: 200,
            headers: sample_headers(),
            body: vec![0xde, 0xad, 0xbe, 0xef],
        };
        let bytes = r.encode_into_bytes();
        let back: FetchResult = FetchResult::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes FetchResult::Ok");
        match back {
            FetchResult::Ok {
                url,
                status,
                headers,
                body,
            } => {
                assert_eq!(url, "https://api.example.com/v1/resource");
                assert_eq!(status, 200);
                assert_eq!(headers.len(), 2);
                assert_eq!(body, vec![0xde, 0xad, 0xbe, 0xef]);
            }
            FetchResult::Err { .. } => panic!("expected Ok"),
        }
    }

    #[test]
    fn fetch_result_err_roundtrip_echoes_url_and_http_error() {
        let r = FetchResult::Err {
            url: "https://api.example.com/gone".to_string(),
            error: HttpError::Timeout,
        };
        let bytes = r.encode_into_bytes();
        let back: FetchResult = FetchResult::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes FetchResult::Err");
        match back {
            FetchResult::Err { url, error } => {
                assert_eq!(url, "https://api.example.com/gone");
                assert_eq!(error, HttpError::Timeout);
            }
            FetchResult::Ok { .. } => panic!("expected Err"),
        }
    }

    #[test]
    fn http_error_invalid_url_carries_payload() {
        let e = HttpError::InvalidUrl("not a url".to_string());
        let bytes = wire::to_vec(&e).expect("test setup: wire encodes HttpError::InvalidUrl");
        let back: HttpError =
            wire::from_bytes(&bytes).expect("test setup: wire decodes HttpError::InvalidUrl");
        match back {
            HttpError::InvalidUrl(s) => assert_eq!(s, "not a url"),
            other => panic!("expected InvalidUrl, got {other:?}"),
        }
    }

    #[test]
    fn http_error_adapter_carries_detail() {
        let e = HttpError::AdapterError("dns lookup failed".to_string());
        let bytes = wire::to_vec(&e).expect("test setup: wire encodes HttpError::AdapterError");
        let back: HttpError =
            wire::from_bytes(&bytes).expect("test setup: wire decodes HttpError::AdapterError");
        match back {
            HttpError::AdapterError(s) => assert_eq!(s, "dns lookup failed"),
            other => panic!("expected AdapterError, got {other:?}"),
        }
    }

    #[test]
    fn http_error_unit_variants_roundtrip() {
        for e in [
            HttpError::Timeout,
            HttpError::BodyTooLarge,
            HttpError::AllowlistDenied,
            HttpError::Disabled,
        ] {
            let bytes = wire::to_vec(&e).expect("test setup: wire encodes HttpError unit variant");
            let back: HttpError =
                wire::from_bytes(&bytes).expect("test setup: wire decodes HttpError unit variant");
            assert_eq!(back, e);
        }
    }

    #[test]
    fn http_method_roundtrip_all_variants() {
        for m in [
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Delete,
            HttpMethod::Patch,
            HttpMethod::Head,
            HttpMethod::Options,
        ] {
            let bytes = wire::to_vec(&m).expect("test setup: wire encodes HttpMethod variant");
            let back: HttpMethod =
                wire::from_bytes(&bytes).expect("test setup: wire decodes HttpMethod variant");
            assert_eq!(back, m);
        }
    }

    #[test]
    fn http_server_request_roundtrip() {
        assert_eq!(HttpServerRequest::NAME, "aether.http.server.request");
        let r = HttpServerRequest {
            method: HttpMethod::Post,
            path: "/api/v1/things".to_string(),
            query: "foo=bar&baz=1".to_string(),
            headers: sample_headers(),
            body: vec![0x01, 0x02, 0x03],
        };
        let bytes = r.encode_into_bytes();
        let back: HttpServerRequest = HttpServerRequest::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes HttpServerRequest");
        assert_eq!(back.method, HttpMethod::Post);
        assert_eq!(back.path, "/api/v1/things");
        assert_eq!(back.query, "foo=bar&baz=1");
        assert_eq!(back.headers, r.headers);
        assert_eq!(back.body, vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn http_server_request_empty_query_roundtrip() {
        let r = HttpServerRequest {
            method: HttpMethod::Get,
            path: "/health".to_string(),
            query: String::new(),
            headers: vec![],
            body: vec![],
        };
        let bytes = r.encode_into_bytes();
        let back: HttpServerRequest = HttpServerRequest::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes HttpServerRequest (empty query)");
        assert_eq!(back.query, "");
        assert_eq!(back.method, HttpMethod::Get);
    }

    #[test]
    fn http_server_response_roundtrip() {
        assert_eq!(HttpServerResponse::NAME, "aether.http.server.response");
        let r = HttpServerResponse {
            status: 200,
            headers: sample_headers(),
            body: vec![0xde, 0xad, 0xbe, 0xef],
        };
        let bytes = r.encode_into_bytes();
        let back: HttpServerResponse = HttpServerResponse::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes HttpServerResponse");
        assert_eq!(back.status, 200);
        assert_eq!(back.headers, r.headers);
        assert_eq!(back.body, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn http_server_response_error_status_roundtrip() {
        let r = HttpServerResponse {
            status: 404,
            headers: vec![],
            body: b"not found".to_vec(),
        };
        let bytes = r.encode_into_bytes();
        let back: HttpServerResponse = HttpServerResponse::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes HttpServerResponse (404)");
        assert_eq!(back.status, 404);
        assert_eq!(back.body, b"not found");
    }
}
