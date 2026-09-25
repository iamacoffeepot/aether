//! The Engine API client against byte-level responses and the stub daemon.

use std::io::{self, ErrorKind, Read, Write};
use std::thread;

use super::api::ContainerId;
use super::http::Response;
use super::logs::{self, Demux, Lengths, Output};
use super::{Endpoint, Engine, EngineError};
use crate::ImageRef;
use crate::runtime::testing::{StubDaemon, StubReply, log_stream};

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

#[test]
fn a_chunked_upload_arrives_byte_exact_with_its_query_encoded() {
    // Catches chunk framing bugs on the way out: a size written in decimal, a
    // missing CRLF after a chunk, a buffered tail never sent, or no
    // terminating chunk (the stub would then fail to decode the body). Also
    // catches a `changes` value spliced into the query unencoded, whose space
    // and `=` would cut the instruction short.
    let stub = StubDaemon::bind().expect("bind the stub");
    let engine = stub_engine(&stub);
    let payload: Vec<u8> = (0..=250u8).cycle().take(200 * 1024 + 7).collect();
    let change = ["LABEL aether.workspace.environment=ab".to_owned()];

    let (result, requests) = thread::scope(|scope| {
        let served = scope.spawn(|| stub.serve(vec![StubReply::chunked(200, vec![br#"{"status":"ok"}"#.to_vec()])]));
        let result = engine.import_image("repo", "ab", &change, |out| out.write_all(&payload));
        (result, served.join().expect("the stub thread").expect("the stub serves"))
    });

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        requests[0].line(),
        "POST /v1.44/images/create?fromSrc=-&repo=repo&tag=ab&changes=LABEL%20aether.workspace.environment%3Dab"
    );
    assert!(requests[0].body == payload, "the body arrived altered ({} bytes)", requests[0].body.len());
}

#[test]
fn a_log_stream_demuxes_into_the_exact_stdout_and_stderr_bytes() {
    // Catches a demux that mis-reads a frame header split across reads,
    // stops at an empty frame, mixes the two outputs, or counts a length the
    // writing read then disagrees with. The reader hands out one byte at a
    // time, so every header straddles reads.
    let stream = log_stream(&[(1, b"out-1\n"), (2, b"err-1\n"), (1, b""), (1, &[0xff, 0x00, b'\n']), (2, b"err-2")]);

    let lengths = logs::count(OneByte(&stream)).expect("count");
    let mut stdout = Vec::new();
    Demux::new(OneByte(&stream), Output::Stdout).read_to_end(&mut stdout).expect("stdout");
    let mut stderr = Vec::new();
    Demux::new(OneByte(&stream), Output::Stderr).read_to_end(&mut stderr).expect("stderr");

    assert_eq!(stdout, b"out-1\n\xff\x00\n");
    assert_eq!(stderr, b"err-1\nerr-2");
    assert_eq!(lengths, Lengths { stdout: 9, stderr: 11 });

    let cut = &stream[..stream.len() - 2];
    let mut sink = Vec::new();
    let error = Demux::new(cut, Output::Stderr).read_to_end(&mut sink).expect_err("a cut frame");
    assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
}

#[test]
fn a_hijacked_attach_delivers_the_stdin_bytes_and_their_end() {
    // Catches an attach that is not upgraded (bytes written as a request
    // body the daemon discards), buffered bytes never sent, or a stream never
    // closed, which would leave the process reading stdin forever: the stub
    // records the stream only once the client closes its writing half.
    let stub = StubDaemon::bind().expect("bind the stub");
    let engine = stub_engine(&stub);
    let container = ContainerId::new("c0ffee").expect("hex");
    let stdin: Vec<u8> = (0..=255u8).cycle().take(300 * 1024).collect();

    let requests = thread::scope(|scope| {
        let served = scope.spawn(|| stub.serve(vec![StubReply::upgrade()]));
        let mut connection = engine.attach_stdin(&container).expect("attach");
        connection.write_all(&stdin).expect("write stdin");
        connection.shutdown_write().expect("close stdin");
        served.join().expect("the stub thread").expect("the stub serves")
    });

    assert_eq!(requests[0].line(), "POST /v1.44/containers/c0ffee/attach?stream=1&stdin=1");
    assert!(requests[0].body == stdin, "stdin arrived altered ({} bytes)", requests[0].body.len());
}

/// A reader that yields one byte per read.
struct OneByte<'a>(&'a [u8]);

impl Read for OneByte<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some((&first, rest)) = self.0.split_first() else {
            return Ok(0);
        };
        if buf.is_empty() {
            return Ok(0);
        }
        buf[0] = first;
        self.0 = rest;
        Ok(1)
    }
}
