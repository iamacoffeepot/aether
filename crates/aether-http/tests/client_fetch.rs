//! The `aether.http` egress cap booted in `SubstrateHarness` and driven by
//! mail: each fetch goes through production dispatch, the off-thread worker,
//! and the `#[handler(task)]` completion that answers the caller. A loopback
//! listener on an ephemeral port stands in for the remote.

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_http::{Fetch, FetchResult, HttpCapability, HttpConfig, HttpError, HttpMethod};
use std::collections::HashSet;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::thread;
use std::time::{Duration, Instant};

const OK_JSON: &[u8] = b"HTTP/1.1 200 OK\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 2\r\n\
    Connection: close\r\n\
    \r\n\
    {}";

/// How long the loopback server waits for the cap to connect before giving
/// up, so a fetch that never dials fails the test instead of hanging it.
const ACCEPT_BUDGET: Duration = Duration::from_secs(10);

fn harness() -> SubstrateHarness {
    let config = HttpConfig {
        allowlist: HashSet::from(["127.0.0.1".to_owned()]),
        require_https: false,
        default_timeout: Duration::from_secs(2),
        ..HttpConfig::default()
    };
    SubstrateHarness::builder()
        .with_actor_configured::<HttpCapability>((), config)
        .build()
        .expect("boot harness with the http cap")
}

fn loopback() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let addr = listener.local_addr().expect("loopback listener address");
    (listener, addr)
}

/// Accept one connection, read its request head, wait `delay`, and answer
/// with `response`. Polls a non-blocking accept against [`ACCEPT_BUDGET`] so
/// the scoped thread always ends.
fn serve_once(listener: &TcpListener, delay: Duration, response: &[u8]) {
    listener.set_nonblocking(true).expect("nonblocking listener");
    let deadline = Instant::now() + ACCEPT_BUDGET;
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("the cap never connected to the loopback server: {error}"),
        }
    };
    stream.set_nonblocking(false).expect("blocking accepted stream");

    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buf).expect("read request head");
        assert_ne!(read, 0, "the client closed before finishing its request head");
        head.extend_from_slice(&buf[..read]);
    }

    thread::sleep(delay);
    stream.write_all(response).expect("write loopback response");
}

fn get(request_id: u64, url: String, timeout_ms: Option<u32>) -> Fetch {
    Fetch { request_id, url, method: HttpMethod::Get, headers: vec![], body: vec![], timeout_ms }
}

fn fetch(harness: &mut SubstrateHarness, mail: &Fetch) -> FetchResult {
    let http = harness.actor_ref::<HttpCapability>();
    harness
        .execute(vec![("fetch", HarnessOp::send_and_await_reply(&http, mail))])
        .expect("fetch round trip")
        .reply::<FetchResult>("fetch")
        .expect("decode fetch reply")
}

/// The Ok arm carries the remote's status, headers, and body, and echoes the
/// caller-minted `request_id` and the requested url.
#[test]
fn fetch_ok_replies_with_response_and_echoes_request_id() {
    let mut harness = harness();
    let (listener, addr) = loopback();
    let url = format!("http://{addr}/v1");

    let reply = thread::scope(|scope| {
        scope.spawn(|| serve_once(&listener, Duration::ZERO, OK_JSON));
        fetch(&mut harness, &get(42, url.clone(), Some(5_000)))
    });

    match reply {
        FetchResult::Ok { request_id, url: echoed, status, headers, body } => {
            assert_eq!(request_id, 42, "the Ok arm echoes the caller-minted request_id");
            assert_eq!(echoed, url);
            assert_eq!(status, 200);
            assert!(
                headers.iter().any(|h| h.name.eq_ignore_ascii_case("content-type") && h.value == "application/json"),
                "the remote's headers reach the caller: {headers:?}",
            );
            assert_eq!(body, b"{}");
        }
        FetchResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
    }
}

/// The Err arm echoes the `request_id` and url too: a host off the allowlist
/// is refused, and the refusal still names the request it answers.
#[test]
fn fetch_err_echoes_request_id_and_url() {
    let mut harness = harness();
    let url = "http://denied.example.invalid/".to_owned();

    match fetch(&mut harness, &get(7, url.clone(), None)) {
        FetchResult::Err { request_id, url: echoed, error } => {
            assert_eq!(request_id, 7, "the Err arm echoes the caller-minted request_id");
            assert_eq!(echoed, url);
            assert_eq!(error, HttpError::AllowlistDenied);
        }
        FetchResult::Ok { status, .. } => panic!("expected AllowlistDenied, got status {status}"),
    }
}

/// A fetch with no explicit timeout runs under the configured default: the
/// remote answers after a short delay, well inside the 2 s default, so a
/// missing timeout mapped to zero (or anything shorter than the delay) would
/// reply `Timeout` instead of `Ok`.
#[test]
fn fetch_without_timeout_uses_configured_default() {
    let mut harness = harness();
    let (listener, addr) = loopback();

    let reply = thread::scope(|scope| {
        scope.spawn(|| serve_once(&listener, Duration::from_millis(50), OK_JSON));
        fetch(&mut harness, &get(0, format!("http://{addr}/"), None))
    });

    match reply {
        FetchResult::Ok { status, .. } => assert_eq!(status, 200),
        FetchResult::Err { error, .. } => panic!("expected Ok under the default timeout, got Err({error:?})"),
    }
}
