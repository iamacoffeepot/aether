//! End-to-end: one `muse.turn` Sampled call on the shipped bloomery composition, answered by a loopback stub server
//! when the harness allows its host, and recorded as a refused turn when the deny-by-default allowlist holds.

use std::error::Error;
use std::fs;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery_journal::{Batch, Journal, Seq};
use aether_bloomery_kinds::{
    Call, CallOutcome, Detail, Digest, Fault, FaultReason, Head, NativeOrigin, OpaqueBytes, ProgramName, ProgramRef,
    RecordedHead, RecordedHeadMove, Ref, RequestSource, Requested, artifact_digest,
};
use aether_bloomery_muse::{
    Endpoint, ModelName, OutputBudget, ReasoningEffort, Role, TurnInput, TurnItem, TurnItems, TurnOutcome, TurnResult,
};
use aether_data::Kind;
use aether_harness_bloomery::{BloomeryHarness, Record};
use aether_harness_substrate::test_helpers::require_wasm;

/// The recorded responses-API reply the stub server sends.
const COMPLETED: &[u8] = include_bytes!("../../aether-bloomery-muse/fixtures/completed.json");

/// The `muse` head the seed binds to the bundle.
const MUSE: Head<OpaqueBytes> = Head::new("muse");

/// The conversation the turn resends, in order.
const ITEMS: [(Role, &str); 2] = [(Role::Developer, "Be brief."), (Role::User, "What is a bloomery?")];

/// How long the stub waits for the engine to dial. Longer than the harness's own thirty-second reply bound, so a
/// call that never dials panics with the harness's message rather than the stub's.
const STUB_PATIENCE: Duration = Duration::from_secs(45);

/// A seed holding the `muse` bundle under [`MUSE`], the conversation's texts, and a turn input posting to one
/// endpoint.
struct MuseSeed {
    batch: Batch,
    bundle: Digest,
    input: Digest,
}

/// Seed the bundle and a turn posting to `endpoint`, or `None` when the bundle wasm is not built.
fn seed(endpoint: &str) -> Result<Option<MuseSeed>, Box<dyn Error>> {
    let Some(wasm_path) = require_wasm("aether_bloomery_muse") else {
        return Ok(None);
    };
    let wasm = fs::read(&wasm_path)?;
    let mut batch = Batch::new();
    let bundle = batch.stage_bytes(&wasm).digest();
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&MUSE), bundle), None)?;

    let items = ITEMS.iter().map(|&(role, text)| TurnItem::new(role, batch.stage_text(text))).collect();
    let input = TurnInput::new(
        Endpoint::new(endpoint)?,
        ModelName::new("muse-spark-1.3")?,
        TurnItems::new(items)?,
        OutputBudget::new(512)?,
        ReasoningEffort::Low,
    );
    let input = batch.stage_encoded(&input)?.digest();
    Ok(Some(MuseSeed { batch, bundle, input }))
}

/// The one `muse.turn` call each scenario makes, and the `Requested` it records.
fn turn(seed: &MuseSeed) -> Result<(Call, Requested), Box<dyn Error>> {
    let origin = NativeOrigin::new("test.muse")?;
    let name = ProgramName::new("muse.turn")?;
    let call = Call { program: MUSE, name: name.clone(), input: seed.input, origin: origin.clone(), key: 1 };
    let requested = Requested {
        program: ProgramRef::new(seed.bundle, name),
        input: seed.input,
        source: RequestSource::Native { origin, key: 1 },
    };
    Ok((call, requested))
}

/// One request the stub server read: its head lines and its body.
struct StubRequest {
    head: String,
    body: Vec<u8>,
}

/// Accept one connection on `listener` within [`STUB_PATIENCE`], read one request, and answer it `200 OK` with
/// `body` as JSON.
fn serve_once(listener: &TcpListener, body: &[u8]) -> io::Result<StubRequest> {
    let mut stream = accept_within(listener, STUB_PATIENCE)?;
    stream.set_read_timeout(Some(STUB_PATIENCE))?;
    let request = read_request(&stream)?;

    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(request)
}

/// Accept one connection, giving up after `patience` so a call that never dials cannot hold the scenario open.
fn accept_within(listener: &TcpListener, patience: Duration) -> io::Result<TcpStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + patience;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                return Ok(stream);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Read one request: the head up to its blank line, then exactly `Content-Length` body bytes.
fn read_request(stream: &TcpStream) -> io::Result<StubRequest> {
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(ErrorKind::UnexpectedEof.into());
        }
        if line == "\r\n" {
            break;
        }
        head.push_str(&line);
    }

    let length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map_or(Ok(0), |(_, value)| value.trim().parse().map_err(|_| io::Error::from(ErrorKind::InvalidData)))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(StubRequest { head, body })
}

/// Whether `listener` holds no connection it has not accepted.
fn nothing_dialed(listener: &TcpListener) -> io::Result<bool> {
    listener.set_nonblocking(true)?;
    match listener.accept() {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error),
    }
}

#[test]
fn a_muse_turn_records_the_reply_a_loopback_server_sends() -> Result<(), Box<dyn Error>> {
    // Catches HTTP not composed (the call is never answered), the harness allowlist not reaching the capability,
    // the request body or method lost between program and wire, and the reply body not staged or cited.
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let Some(seed) = seed(&format!("http://{}/v1/responses", listener.local_addr()?))? else {
        return Ok(());
    };
    let (call, requested) = turn(&seed)?;
    let mut harness = BloomeryHarness::start_allowing([seed.batch], ["127.0.0.1"]);

    let (outcome, served) = thread::scope(|scope| {
        let stub = scope.spawn(|| serve_once(&listener, COMPLETED));
        let outcome = harness.call(&call);
        (outcome, stub.join().expect("the stub server thread finished"))
    });
    let CallOutcome::Transition { key: 1, seq: 3, transition } = outcome else {
        panic!("expected the turn's Transition at seq 3, got {outcome:?}");
    };
    harness.assert_appended(Seq(1), &[Record::equal(None, requested), Record::equal(Some(Seq(2)), transition.clone())]);

    let result = Journal::open(harness.journal_path())?
        .get::<TurnResult>(&transition.result)?
        .expect("the transition cites a stored turn result");
    assert_eq!(result.status().get(), 200);
    let TurnOutcome::Completed { text, .. } = result.outcome() else {
        panic!("expected a Completed turn, got {:?}", result.outcome());
    };
    assert_eq!(*text, Ref::of_text("A bloomery is a furnace that smelts iron into a bloom."));
    assert!(harness.stores(&text.digest()), "the answer text is stored");
    assert_eq!(result.body().digest(), artifact_digest(OpaqueBytes::ID, COMPLETED), "the raw reply body is kept");
    assert!(harness.stores(&result.body().digest()), "the raw reply body is stored");

    let served = served?;
    assert!(served.head.starts_with("POST /v1/responses HTTP/1.1\r\n"), "request line: {}", served.head);
    let body: serde_json::Value = serde_json::from_slice(&served.body)?;
    assert_eq!(body["store"], false, "the turn is stateless on the vendor side");
    let sent: Vec<_> = body["input"]
        .as_array()
        .expect("the body carries the conversation")
        .iter()
        .map(|item| (item["role"].as_str(), item["content"][0]["text"].as_str()))
        .collect();
    assert_eq!(sent, [(Some("developer"), Some("Be brief.")), (Some("user"), Some("What is a bloomery?"))]);
    assert!(nothing_dialed(&listener)?, "the turn sends exactly one request");
    Ok(())
}

#[test]
fn an_empty_allowlist_records_a_refused_turn_without_dialing() -> Result<(), Box<dyn Error>> {
    // Catches the silent hang (without `aether.http` the fetch warn-drops and the call is never answered, so the
    // harness panics at its thirty-second bound), egress open by default, and hermetic resolution not in effect.
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let Some(seed) = seed(&format!("http://{}/v1/responses", listener.local_addr()?))? else {
        return Ok(());
    };
    let (call, requested) = turn(&seed)?;
    let mut harness = BloomeryHarness::start([seed.batch]);

    let outcome = harness.call(&call);
    let CallOutcome::Fault { key: 1, seq: 3, fault } = outcome else {
        panic!("expected the turn's Fault at seq 3, got {outcome:?}");
    };
    assert_eq!(fault.reason, FaultReason::Refused { reason: Detail::new("AllowlistDenied") });
    harness.assert_appended(
        Seq(1),
        &[
            Record::equal(None, requested.clone()),
            Record::equal(
                Some(Seq(2)),
                Fault { program: requested.program, input: requested.input, reason: fault.reason },
            ),
        ],
    );
    assert!(nothing_dialed(&listener)?, "a denied fetch opens no connection");
    Ok(())
}
