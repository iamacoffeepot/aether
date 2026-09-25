//! The Engine API client against byte-level responses and the stub daemon.

use std::io::{self, ErrorKind, Read, Write};
use std::thread;
use std::time::Duration;

use super::api::{ContainerId, Waited};
use super::http::Response;
use super::logs::{self, Demux, Lengths, Output};
use super::{ENDPOINT_KEY, Endpoint, Engine, EngineError, TLS_CA_FILE_KEY, TLS_CERT_FILE_KEY, TLS_KEY_FILE_KEY};
use crate::runtime::testing::{StubDaemon, StubReply, log_stream};
use crate::{ImageRef, WorkspaceConfig};

const IMAGE: &str = "debian@sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn body_of(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    Response::read(raw).map_err(io::Error::other)?.body.read_to_end(&mut body)?;
    Ok(body)
}

fn stub_engine(stub: &StubDaemon) -> Engine {
    Engine::new(Endpoint::from_config(&stub.config()).expect("the stub's config is a usable endpoint"))
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
fn endpoint_config_accepts_unix_and_mutual_tls_and_names_the_refused_key() {
    // Catches a remote endpoint accepted without client auth (a root-equivalent
    // daemon reachable with no certificate), a TLS file beside a Unix socket
    // silently ignored, a named-pipe endpoint accepted before its transport
    // exists, and a refusal that names a key other than the one an operator
    // must fix.
    let stub = StubDaemon::bind_tls().expect("bind the TLS stub");
    let tls = stub.config();
    let unix = |endpoint: &str| WorkspaceConfig { endpoint: Some(endpoint.to_owned()), ..WorkspaceConfig::default() };

    assert!(Endpoint::from_config(&tls).is_ok(), "tcp:// with a port and all three files");
    assert!(Endpoint::from_config(&unix("unix:///var/run/docker.sock")).is_ok());
    assert!(Endpoint::from_config(&WorkspaceConfig::default()).is_ok(), "the unset default");

    let refusals = [
        (WorkspaceConfig { endpoint: Some("tcp://127.0.0.1".to_owned()), ..tls.clone() }, ENDPOINT_KEY),
        (WorkspaceConfig { endpoint: Some("tcp://127.0.0.1:0".to_owned()), ..tls.clone() }, ENDPOINT_KEY),
        (WorkspaceConfig { tls_ca_file: None, ..tls.clone() }, TLS_CA_FILE_KEY),
        (WorkspaceConfig { tls_cert_file: None, ..tls.clone() }, TLS_CERT_FILE_KEY),
        (WorkspaceConfig { tls_key_file: None, ..tls.clone() }, TLS_KEY_FILE_KEY),
        (WorkspaceConfig { tls_key_file: tls.tls_ca_file.clone(), ..tls.clone() }, TLS_KEY_FILE_KEY),
        (WorkspaceConfig { tls_cert_file: tls.tls_cert_file, ..unix("unix:///run/d.sock") }, TLS_CERT_FILE_KEY),
        (unix("npipe:////./pipe/docker_engine"), ENDPOINT_KEY),
        (unix("unix://relative.sock"), ENDPOINT_KEY),
    ];
    for (config, key) in refusals {
        let Err(error) = Endpoint::from_config(&config) else {
            panic!("accepted {config:?}");
        };
        assert_eq!(error.key, key, "{error}");
        assert!(error.to_string().starts_with(key), "the refusal leads with the key: {error}");
    }
}

#[test]
fn a_tls_request_round_trips_with_a_client_certificate() {
    // Catches the TLS variant not wired into `connect`, the TLS files not read
    // from config, or no client certificate presented: the stub refuses a
    // client without one, so the call would fail instead of answering.
    let stub = StubDaemon::bind_tls().expect("bind the TLS stub");
    let engine = stub_engine(&stub);
    let image = ImageRef::new(IMAGE).expect("a valid ref");

    let (result, requests) = thread::scope(|scope| {
        let reply = StubReply::with_length(200, format!(r#"{{"Id":"sha256:1","RepoDigests":["{IMAGE}"]}}"#));
        let served = scope.spawn(|| stub.serve(vec![reply]));
        let result = engine.repo_digests(&image);
        (result, served.join().expect("the stub thread").expect("the stub serves"))
    });

    assert_eq!(result.expect("the inspect answers"), vec![IMAGE.to_owned()]);
    assert_eq!(requests[0].line(), format!("GET /v1.44/images/{IMAGE}/json"));
}

#[test]
fn a_server_certificate_from_another_ca_fails_the_connect() {
    // Catches server verification disabled, or a root store that picks up
    // roots beyond the configured CA: either would let the call through to a
    // daemon the configured CA never vouched for.
    let stub = StubDaemon::bind_tls_untrusted().expect("bind the untrusted TLS stub");
    let engine = stub_engine(&stub);
    let dialed = stub.endpoint();
    let image = ImageRef::new(IMAGE).expect("a valid ref");

    let result = thread::scope(|scope| {
        let served = scope.spawn(|| stub.serve(vec![StubReply::with_length(200, r#"{"RepoDigests":[]}"#)]));
        let result = engine.repo_digests(&image);
        // Ignored: the stub's handshake fails too once the client aborts it.
        let _ = served.join();
        result
    });

    match result {
        Err(EngineError::Connect { endpoint, .. }) => assert_eq!(endpoint, dialed),
        other => panic!("expected a connect failure, got {other:?}"),
    }
}

#[test]
fn a_hijacked_attach_over_tls_delivers_the_stdin_bytes_and_their_end() {
    // Catches a TLS half-close that skips the close_notify or its flush: the
    // stub would then read a truncated stream, an error rather than the end,
    // instead of every stdin byte followed by a clean end.
    assert_an_attach_delivers_the_stdin_bytes_and_their_end(StubDaemon::bind_tls().expect("bind the TLS stub"));
}

#[test]
fn a_wait_over_tls_times_out_on_the_socket_timeout() {
    // Catches the socket's timeout arriving remapped through the TLS stream,
    // which would turn a run's deadline into a failed wait rather than a kill
    // answering `Exhausted(Time)`.
    let stub = StubDaemon::bind_tls().expect("bind the TLS stub");
    let engine = stub_engine(&stub);
    let container = ContainerId::new("c0ffee").expect("hex");

    let waited = thread::scope(|scope| {
        let served = scope.spawn(|| stub.serve(vec![StubReply::hold()]));
        let waited = engine.wait(&container, Duration::from_millis(200));
        served.join().expect("the stub thread").expect("the stub serves");
        waited
    });

    assert_eq!(waited.expect("the wait ends"), Waited::TimedOut);
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
    assert_an_attach_delivers_the_stdin_bytes_and_their_end(StubDaemon::bind().expect("bind the stub"));
}

/// Attach to `stub`, stream 300 KiB of stdin, close it, and require the stub
/// to have read exactly those bytes and then a clean end.
fn assert_an_attach_delivers_the_stdin_bytes_and_their_end(stub: StubDaemon) {
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
