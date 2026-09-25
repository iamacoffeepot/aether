//! The Engine API client against byte-level responses and the stub daemon.

use std::io::{self, ErrorKind, Read};
use std::thread;

use super::http::Response;
use super::{Endpoint, Engine, EngineError};
use crate::ImageRef;
use crate::runtime::testing::{StubDaemon, StubReply};

const IMAGE: &str = "debian@sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn body_of(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    Response::read(raw).map_err(io::Error::other)?.body.read_to_end(&mut body)?;
    Ok(body)
}

fn stub_engine(stub: &StubDaemon) -> Engine {
    Engine::new(Endpoint::parse(&stub.endpoint()).expect("the stub's endpoint parses"))
}

#[test]
fn chunked_and_length_delimited_bodies_decode_byte_exact() {
    // Catches a framing bug that shifts bytes: a chunk's trailing CRLF read as
    // content, a chunk extension kept in the size, the terminating chunk's
    // trailers left in the body, or a length body read past its end.
    let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
        5;ext=1\r\nhello\r\n1\r\n \r\n5\r\nworld\r\n0\r\nX-Trailer: 1\r\n\r\nNEXT";
    assert_eq!(body_of(chunked).expect("chunked body"), b"hello world");

    let length = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloNEXT";
    assert_eq!(body_of(length).expect("length body"), b"hello");
}

#[test]
fn a_body_cut_short_is_an_error_not_a_shorter_body() {
    // Catches a truncated export decoding as a smaller tree: a connection that
    // drops inside a chunk or before a declared length must fail the read.
    let inside_chunk = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\na\r\nhello";
    assert_eq!(body_of(inside_chunk).expect_err("cut inside a chunk").kind(), ErrorKind::UnexpectedEof);

    let before_length = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello";
    assert_eq!(body_of(before_length).expect_err("cut before the length").kind(), ErrorKind::UnexpectedEof);
}

#[test]
fn a_pull_stream_carrying_an_error_under_a_200_fails_the_pull() {
    // Catches a pull judged by its status alone: the daemon reports a missing
    // manifest inside the 200 progress stream, and the import must stop there.
    let stub = StubDaemon::bind().expect("bind the stub");
    let engine = stub_engine(&stub);
    let image = ImageRef::new(IMAGE).expect("a valid ref");
    let stream = br#"{"status":"Pulling from library/debian"}
{"errorDetail":{"message":"manifest unknown"},"error":"manifest unknown"}
"#;

    let result = thread::scope(|scope| {
        let served = scope.spawn(|| stub.serve(vec![StubReply::chunked(200, vec![stream.to_vec()])]));
        let result = engine.pull(&image);
        served.join().expect("the stub thread").expect("the stub serves");
        result
    });

    match result {
        Err(EngineError::Pull(message)) => assert_eq!(message, "manifest unknown"),
        other => panic!("expected a pull error, got {other:?}"),
    }
}

#[test]
fn a_non_2xx_status_carries_the_daemons_message() {
    // Catches a non-2xx answer read as success (an inspect 404 parsed as an
    // empty image) or its message lost from the failure's detail.
    let stub = StubDaemon::bind().expect("bind the stub");
    let engine = stub_engine(&stub);
    let image = ImageRef::new(IMAGE).expect("a valid ref");

    let result = thread::scope(|scope| {
        let reply = StubReply::with_length(404, r#"{"message":"No such image: debian"}"#);
        let served = scope.spawn(|| stub.serve(vec![reply]));
        let result = engine.repo_digests(&image);
        served.join().expect("the stub thread").expect("the stub serves");
        result
    });

    match result {
        Err(EngineError::Status { status: 404, message }) => assert_eq!(message, "No such image: debian"),
        other => panic!("expected a 404, got {other:?}"),
    }
}

#[test]
fn endpoint_parsing_accepts_only_an_absolute_unix_socket() {
    // Catches a remote or named-pipe endpoint accepted before its transport
    // exists (every request would then fail at dial time instead of at boot),
    // and a refusal that does not name the key an operator must fix.
    for refused in ["tcp://127.0.0.1:2375", "npipe:////./pipe/docker_engine", "unix://relative.sock"] {
        let message = Endpoint::parse(refused).expect_err(refused).to_string();
        assert!(message.contains("AETHER_WORKSPACE_ENDPOINT"), "the refusal names the key: {message}");
    }
    assert!(Endpoint::parse("unix:///var/run/docker.sock").is_ok());
}
