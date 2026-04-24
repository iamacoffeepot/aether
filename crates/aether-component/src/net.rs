//! ADR-0043 substrate HTTP egress, guest side. One helper —
//! [`fetch_blocking`] — wraps the `wait_reply_p32` machinery ADR-0042
//! shipped so a component can round-trip a fetch in a single
//! expression instead of splitting logic across a send + a
//! `#[handler]` reply method.
//!
//! **The async path is the default.** Network latency (tens of ms
//! to tens of seconds depending on the remote) means a blocking
//! fetch stalls the calling component for that full duration — a
//! render-path component misses that many frames; an input-
//! subscribed component misses that many events. Nothing else on
//! the substrate is affected (the ADR-0038 actor-per-component
//! scheduler isolates stalls to the calling component's thread),
//! but the caller's own work is paused.
//!
//! The existing async shape — `ctx.send(&net_sink, &Fetch { .. })`
//! plus `#[handler] fn on_fetch_result(&mut self, ctx, r: FetchResult)`
//! — does not block; the reply arrives on the next dispatch tick.
//! That is the right default for anything render-adjacent.
//!
//! [`fetch_blocking`] exists for one-shot tool components and
//! boot-time initialisation where the simpler control flow is
//! worth the stall cost. The name and doc comment lead with the
//! foot-gun so it's never called by accident.
//!
//! A callback-registration alternative ("when this reply arrives,
//! run this closure, and in the meantime the handler continues")
//! is in flight as a design thread; see the CachedBytes /
//! dependency-scheduler ADR follow-up. Once that lands,
//! `fetch_blocking` stays available for the legitimate blocking
//! cases; the async default gets a nicer ergonomic shape than "send
//! + separate handler."

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use aether_kinds::{Fetch, FetchResult, HttpHeader, HttpMethod, NetError};
use aether_mail::Kind;

use crate::{raw, resolve_sink};

/// Short mailbox name the substrate registers its net sink under
/// (ADR-0043). Exposed so components that want to bypass
/// [`fetch_blocking`] and use `ctx.send(&Sink::<Fetch>, ..)`
/// directly don't have to duplicate the string literal.
pub const NET_MAILBOX_NAME: &str = "net";

/// Default guest-side wait buffer for a `FetchResult`. 16MB matches
/// the substrate's `AETHER_NET_MAX_BODY_BYTES` default (ADR-0043
/// §3) — a response exactly at the cap plus the reply frame's
/// String + headers overhead fits. Responses over the cap come
/// back as `FetchResult::Err { error: BodyTooLarge }` on the
/// substrate side; they don't need this buffer.
const FETCH_REPLY_CAP: usize = 16 * 1024 * 1024 + 64 * 1024;

/// Successful fetch response. Mirrors `FetchResult::Ok` without the
/// echoed `url` (the caller already has it — they sent the
/// request). Error replies collapse into the outer `Err` variant of
/// [`fetch_blocking`]'s return so callers match on one layer, not
/// two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

/// Errors surfaced by [`fetch_blocking`]. Mirrors `SyncIoError` in
/// [`crate::io`]: the first three map to the `wait_reply_p32` host
/// fn's sentinels, `Net` carries an ADR-0043 `NetError` from the
/// substrate adapter (timeout, allowlist denied, body-too-large,
/// …), `Decode` covers the unlikely case where the reply bytes
/// don't postcard-decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncNetError {
    Timeout,
    BufferTooSmall,
    Cancelled,
    Net(NetError),
    Decode(String),
}

/// **⚠️ BLOCKING.** Do not call from a `Tick` handler, an input
/// handler, or any code path that runs per frame — this blocks the
/// calling component's thread for the full request latency (tens
/// of ms to tens of seconds, depending on the remote). A component
/// that draws geometry will miss that many frames; a component
/// subscribed to input will miss that many events. Other
/// components on the same substrate keep running.
///
/// For render-adjacent work, use `ctx.send(&net_sink, &Fetch { .. })`
/// and a `#[handler] fn on_fetch_result(..)` — the reply arrives
/// asynchronously and nothing blocks.
///
/// Legitimate callers: one-shot tool components (asset pipeline
/// stages, CLI-style utilities), boot-time initialisation before
/// the first frame ships, long-running workers that are allowed
/// to stall. If in doubt, use the async path.
///
/// `timeout_ms` is the *wait* timeout (how long the guest parks on
/// the reply). Set `fetch.timeout_ms` separately if you want the
/// substrate-side HTTP request to time out faster than the wait —
/// typically `fetch.timeout_ms < timeout_ms` with some slack so the
/// adapter has room to deliver its `Err::Timeout` reply before the
/// wait itself expires.
///
/// # Example
///
/// ```ignore
/// use aether_component::net::fetch_blocking;
/// use aether_kinds::{Fetch, HttpMethod};
///
/// let resp = fetch_blocking(
///     &Fetch {
///         url: "https://api.example.com/v1/resource".into(),
///         method: HttpMethod::Get,
///         headers: vec![],
///         body: vec![],
///         timeout_ms: Some(10_000),
///     },
///     12_000,
/// )?;
/// assert_eq!(resp.status, 200);
/// ```
pub fn fetch_blocking(fetch: &Fetch, timeout_ms: u32) -> Result<FetchResponse, SyncNetError> {
    resolve_sink::<Fetch>(NET_MAILBOX_NAME).send_postcard(fetch);
    let correlation = unsafe { raw::prev_correlation() };

    let mut buf: Vec<u8> = alloc::vec![0u8; FETCH_REPLY_CAP];
    let rc = unsafe {
        raw::wait_reply(
            <FetchResult as Kind>::ID,
            buf.as_mut_ptr().addr() as u32,
            buf.len() as u32,
            timeout_ms,
            correlation,
        )
    };

    let reply: FetchResult = match rc {
        -1 => return Err(SyncNetError::Timeout),
        -2 => return Err(SyncNetError::BufferTooSmall),
        -3 => return Err(SyncNetError::Cancelled),
        n if n >= 0 => {
            let len = n as usize;
            postcard::from_bytes(&buf[..len]).map_err(|e| SyncNetError::Decode(format!("{e}")))?
        }
        _ => return Err(SyncNetError::Decode(format!("unexpected wait_reply: {rc}"))),
    };

    match reply {
        FetchResult::Ok {
            status,
            headers,
            body,
            ..
        } => Ok(FetchResponse {
            status,
            headers,
            body,
        }),
        FetchResult::Err { error, .. } => Err(SyncNetError::Net(error)),
    }
}

/// Convenience: send a `Fetch` without waiting for the reply. The
/// `FetchResult` arrives on the component's mailbox — wire a
/// `#[handler] fn on_fetch_result(..)` to consume it. Exists as the
/// typed + named counterpart to [`fetch_blocking`]; `ctx.send` on a
/// user-built `Sink<Fetch>` does the same thing.
pub fn fetch(fetch: &Fetch) {
    resolve_sink::<Fetch>(NET_MAILBOX_NAME).send_postcard(fetch);
}

/// Tiny constructor for the common "GET with no headers or body"
/// shape. Saves a bit of literal-struct boilerplate at the call
/// site. Composes: the caller still owns `timeout_ms` and body if
/// they want to adjust.
pub fn get(url: impl Into<String>) -> Fetch {
    Fetch {
        url: url.into(),
        method: HttpMethod::Get,
        headers: alloc::vec::Vec::new(),
        body: alloc::vec::Vec::new(),
        timeout_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    // The helpers' host-fn send path panics off-wasm (raw::send_mail
    // has a host-target stub). What we *can* test on host is the
    // encode step — the bytes the helper would push through the FFI
    // should postcard-roundtrip into the same request kind. That
    // proves the wire shape stays identical to what the ADR-0043
    // substrate dispatcher decodes. Same pattern as `io::tests`.

    fn postcard_bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
        postcard::to_allocvec(value).unwrap()
    }

    #[test]
    fn fetch_encodes_to_postcard_fetch() {
        let req = Fetch {
            url: "https://api.example.com/v1".to_string(),
            method: HttpMethod::Post,
            headers: alloc::vec![HttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            }],
            body: alloc::vec![b'{', b'}'],
            timeout_ms: Some(5000),
        };
        let encoded = postcard_bytes(&req);
        let back: Fetch = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(back.url, "https://api.example.com/v1");
        assert_eq!(back.method, HttpMethod::Post);
        assert_eq!(back.headers.len(), 1);
        assert_eq!(back.body, alloc::vec![b'{', b'}']);
        assert_eq!(back.timeout_ms, Some(5000));
    }

    #[test]
    fn get_constructs_a_fetch_with_get_method_and_empty_body() {
        let req = get("https://api.example.com/health");
        assert_eq!(req.url, "https://api.example.com/health");
        assert_eq!(req.method, HttpMethod::Get);
        assert!(req.headers.is_empty());
        assert!(req.body.is_empty());
        assert_eq!(req.timeout_ms, None);
    }

    #[test]
    fn net_mailbox_name_is_short() {
        // Regression guard for the sink-names-vs-kind-prefixes
        // footgun. Net sink is addressed as "net", not "aether.net".
        assert_eq!(NET_MAILBOX_NAME, "net");
        assert_ne!(NET_MAILBOX_NAME, "aether.net");
    }
}
