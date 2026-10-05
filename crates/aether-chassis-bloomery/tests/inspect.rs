//! End-to-end: the inspect actor reads journal entries and artifacts as JSON on the shipped bloomery composition —
//! a rested Muse session's transcript, a kind no schema source knows, and a fan-out past the resolution budget.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::str;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery_journal::{Batch, Seq};
use aether_bloomery_kinds::{
    Call, CallOutcome, EncodedArtifact, Head, MoveHead, MoveHeadResult, Name, NativeOrigin, Node, Path, ProgramName,
    ReactorSet, RecordedHead, RecordedHeadMove, Tree,
};
use aether_bloomery_muse::{
    Endpoint, InputLimit, MUSE, ModelName, OpenInput, OutputBudget, ReasoningEffort, TurnLimit, TurnSettings, offered,
};
use aether_chassis_bloomery::inspect::{
    InspectArtifact, InspectArtifactResult, InspectEvents, InspectEventsResult, InspectedEvent, MAX_ARTIFACTS,
};
use aether_data::storage::storage_kind;
use aether_data::{KindId, OpaqueBytes, Ref, Utf8Text, storage_kind_id_from_name};
use aether_harness_bloomery::BloomeryHarness;
use aether_harness_substrate::test_helpers::require_wasm;
use serde_json::{Value, json};

/// The recorded responses-API reply the stub server sends.
const ENDED: &[u8] = include_bytes!("../../aether-bloomery-muse/fixtures/ended.json");

/// The user message the session opens with.
const QUESTION: &str = "What is a bloomery?";

/// The summary the `ended.json` fixture's `muse-end` call ends the run with.
const SUMMARY: &str = "Every briefed change is in the tree.";

/// How long the stub waits for the engine to dial.
const STUB_PATIENCE: Duration = Duration::from_secs(45);

/// How many head moves the session scenario follows before it calls the session unrested.
const SESSION_ROUNDS: usize = 16;

/// Accept one connection on `listener` within [`STUB_PATIENCE`], read one request, and answer it `200 OK` with
/// `body` as JSON.
fn serve_once(listener: &TcpListener, body: &[u8]) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + STUB_PATIENCE;
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    };
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(STUB_PATIENCE))?;
    read_request(&stream)?;

    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Read one request: the head up to its blank line, then exactly `Content-Length` body bytes.
fn read_request(stream: &TcpStream) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(ErrorKind::UnexpectedEof.into());
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().map_err(|_| io::Error::from(ErrorKind::InvalidData))?;
        }
    }
    reader.read_exact(&mut vec![0; length])
}

/// Every event the journal holds from `after` on, paging with `next_after`.
fn every_event(harness: &mut BloomeryHarness) -> Vec<InspectedEvent> {
    let mut events = Vec::new();
    let mut after = 0;
    loop {
        let InspectEventsResult::Ok { head, next_after, events: page } =
            harness.inspect_events(&InspectEvents { after, limit: 128, kinds: Vec::new() })
        else {
            panic!("the inspect actor reads the journal");
        };
        events.extend(page);
        if next_after >= head || next_after == after {
            return events;
        }
        after = next_after;
    }
}

/// The recorded `muse.session.record` transition's value, once the session rested.
fn session_record(harness: &mut BloomeryHarness) -> Option<Value> {
    let InspectEventsResult::Ok { events, .. } =
        harness.inspect_events(&InspectEvents { after: 0, limit: 128, kinds: vec!["bloomery.transition".to_owned()] })
    else {
        panic!("the inspect actor reads the journal");
    };
    events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.value).expect("an event value is JSON"))
        .find(|value| value.to_string().contains("\"muse.session.record\""))
}

/// Every string a resolved `aether.artifact.text` carries in `json`, walked without recursion.
fn resolved_texts(json: &Value) -> Vec<String> {
    let mut texts = Vec::new();
    let mut pending = vec![json];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(object) => {
                if object.get("kind") == Some(&json!("aether.artifact.text"))
                    && let Some(Value::String(text)) = object.get("value")
                {
                    texts.push(text.clone());
                }
                pending.extend(object.values());
            }
            Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    texts
}

#[test]
fn a_rested_muse_session_reads_as_json_with_its_transcript_inline() -> Result<(), Box<dyn Error>> {
    // Catches a storage kind missing from the native inventory (the transition renders as hex), the kind filter
    // dropping a matching entry, digests not rendered as hex, and refs not resolved inline. This binary links the
    // muse crate, so its kinds resolve natively here; the next scenario proves the driver's declarations.
    let Some(wasm_path) = require_wasm("aether_bloomery_muse") else {
        return Ok(());
    };
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let endpoint = format!("http://{}/v1/responses", listener.local_addr()?);

    let mut batch = Batch::new();
    let bundle = batch.stage_bytes(&fs::read(&wasm_path)?).digest();
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&MUSE), bundle), None)?;
    let set = batch.stage_encoded(&ReactorSet::new(vec![MUSE])?)?;
    let (tools, artifacts) = offered();
    for artifact in artifacts {
        batch.stage_artifact(artifact);
    }
    let settings = TurnSettings::new(
        Endpoint::new(endpoint)?,
        ModelName::new("muse-spark-1.3")?,
        tools,
        OutputBudget::new(512)?,
        ReasoningEffort::Low,
        InputLimit::new(u64::MAX).expect("limit"),
    );
    let user = batch.stage_text(QUESTION);
    let instructions = batch.stage_text("Work through the tree with only the offered tree tools.");
    let tree = batch.stage_encoded(&Tree::empty())?;
    let open =
        batch.stage_encoded(&OpenInput::new(settings, instructions, user, TurnLimit::new(4)?, tree, Vec::new()))?;
    let call = Call {
        program: MUSE,
        name: ProgramName::new("muse.session.open")?,
        input: open.digest(),
        origin: NativeOrigin::new("test.inspect")?,
        key: 1,
    };
    let mut harness = BloomeryHarness::start_allowing([batch], ["127.0.0.1"]);

    assert_eq!(harness.settle(Seq(1)), Seq(1));
    assert_eq!(harness.move_head(&MoveHead::new(&ReactorSet::ROOT, set, 1)), MoveHeadResult::Committed { seq: 2 });
    harness.settle(Seq(2));

    let record = thread::scope(|scope| {
        let stub = scope.spawn(|| serve_once(&listener, ENDED));
        let outcome = harness.call(&call);
        assert!(matches!(outcome, CallOutcome::Transition { .. }), "the session opens: {outcome:?}");
        let mut record = None;
        for _ in 0..SESSION_ROUNDS {
            let head = harness.settle(harness.head());
            record = session_record(&mut harness);
            if record.is_some() {
                break;
            }
            harness.watch_head(head);
        }
        stub.join().expect("the stub server thread finished")?;
        Ok::<_, io::Error>(record)
    })?;
    let record = record.unwrap_or_else(|| panic!("the session never rested: {:#?}", every_event(&mut harness)));

    let result = record["result"].as_str().expect("the transition's result reads as a hex digest");
    assert_eq!(result.len(), 64, "a digest reads as 64 hex digits: {record}");
    assert_eq!(record["program"]["bundle"], json!(bundle.to_string()), "the bundle reads as its hex digest");

    let mut digest = [0; 32];
    for (slot, pair) in digest.iter_mut().zip(result.as_bytes().chunks(2)) {
        *slot = u8::from_str_radix(str::from_utf8(pair)?, 16)?;
    }
    let InspectArtifactResult::Found { kind, json, truncated, .. } =
        harness.inspect_artifact(&InspectArtifact { digest, depth: 1 })
    else {
        panic!("the session record is stored");
    };
    assert_eq!(kind.as_deref(), Some("muse.session"));
    assert!(!truncated, "one level of a short session fits every bound");
    let session: Value = serde_json::from_str(&json)?;
    assert_eq!(session["rested"], json!("Completed"), "{session:#}");
    let texts = resolved_texts(&session);
    assert!(texts.iter().any(|text| text == QUESTION), "the user question reads inline: {session:#}");
    assert!(texts.iter().any(|text| text == "Ending the run."), "the end call's text reads inline: {session:#}");
    assert!(texts.iter().any(|text| text == SUMMARY), "the summary reads inline: {session:#}");
    Ok(())
}

/// Local mirror of the program fixture's `test.program.summarize.input`: same kind name, same shape, so it encodes
/// to the digest the guest expects. Its result kind has no mirror, so only the driver's declarations name it.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.input")]
struct SummarizeInput {
    text: Ref<Utf8Text>,
}

#[test]
fn a_result_kind_only_a_bundle_declares_resolves_through_the_driver() -> Result<(), Box<dyn Error>> {
    // Catches the driver's declarations not consulted, or consulted before the fixed sources run out (the result
    // renders as hex, unnamed).
    let Some(wasm_path) = require_wasm("aether_test_fixtures_program") else {
        return Ok(());
    };
    let result_kind = storage_kind_id_from_name("test.program.summarize.result");
    assert!(storage_kind(result_kind).is_none(), "the result kind is not linked into this binary");

    let program = Head::<OpaqueBytes>::new("program");
    let mut batch = Batch::new();
    let bundle = batch.stage_bytes(&fs::read(&wasm_path)?).digest();
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&program), bundle), None)?;
    let text = batch.stage_text("hello");
    let input = batch.stage_encoded(&SummarizeInput { text })?.digest();
    let mut harness = BloomeryHarness::start([batch]);
    let call = Call {
        program,
        name: ProgramName::new("test.program.summarize")?,
        input,
        origin: NativeOrigin::new("test.inspect")?,
        key: 1,
    };
    let CallOutcome::Transition { transition, .. } = harness.call(&call) else {
        panic!("the summarize call records a transition");
    };

    let answer = harness.inspect_artifact(&InspectArtifact { digest: *transition.result.as_bytes(), depth: 1 });
    let InspectArtifactResult::Found { kind, json, truncated: false, .. } = answer else {
        panic!("the result is found whole: {answer:?}");
    };
    assert_eq!(kind.as_deref(), Some("test.program.summarize.result"));
    let result: Value = serde_json::from_str(&json)?;
    assert_eq!(result["text"]["kind"], json!("aether.artifact.text"), "{result:#}");
    assert_eq!(result["text"]["value"], json!("summary:hello"), "{result:#}");
    Ok(())
}

#[test]
fn a_kind_no_schema_source_knows_reads_as_hex() {
    // Catches a resolver that errors, panics, or never answers when every schema source misses, the driver's
    // declarations included.
    let kind = KindId(0x0123_4567_89ab_cdef);
    let mut batch = Batch::new();
    let digest = batch.stage_artifact(EncodedArtifact::uncited(kind, b"\x01\x02"));
    let mut harness = BloomeryHarness::start([batch]);

    let answer = harness.inspect_artifact(&InspectArtifact { digest: *digest.as_bytes(), depth: 1 });
    let InspectArtifactResult::Found { kind_id, kind: None, json, truncated: false } = answer else {
        panic!("an unknown kind is found and unnamed: {answer:?}");
    };
    assert_eq!(kind_id, kind.0);
    let json: Value = serde_json::from_str(&json).expect("the rendering is JSON");
    assert_eq!(json, json!({ "kind_id": kind.0, "length": 2, "hex": "0102" }));
}

#[test]
fn a_stored_tree_reads_its_entries_by_name_and_variant() -> Result<(), Box<dyn Error>> {
    // Catches a storage decode that reads a tree's positional map of enum entries as a tagged container, so the
    // artifact renders as hex instead of its entries.
    let mut batch = Batch::new();
    let file = batch.stage_bytes(b"data");
    let script = batch.stage_bytes(b"#!/bin/sh");
    let inner = batch.stage_bytes(b"inner");
    let subtree = batch.stage_encoded(&Tree::new(BTreeMap::from([(Name::new("leaf.txt")?, Node::File(inner))])))?;
    let tree = batch.stage_encoded(&Tree::new(BTreeMap::from([
        (Name::new("a.txt")?, Node::File(file)),
        (Name::new("run")?, Node::Executable(script)),
        (Name::new("link")?, Node::Symlink(Path::new("../bin/run")?)),
        (Name::new("sub")?, Node::Directory(subtree)),
    ])))?;
    let mut harness = BloomeryHarness::start([batch]);

    let answer = harness.inspect_artifact(&InspectArtifact { digest: *tree.digest().as_bytes(), depth: 1 });
    let InspectArtifactResult::Found { kind, json, truncated: false, .. } = answer else {
        panic!("a tree is found whole: {answer:?}");
    };
    assert_eq!(kind.as_deref(), Some("bloomery.tree"));
    let tree: Value = serde_json::from_str(&json)?;
    let entries = &tree["entries"];
    assert!(entries.is_object(), "the entries read as a map, not a fallback: {tree:#}");
    assert_eq!(entries["a.txt"]["File"]["kind"], json!("aether.artifact.bytes"), "{tree:#}");
    assert_eq!(entries["run"]["Executable"]["kind"], json!("aether.artifact.bytes"), "{tree:#}");
    assert_eq!(entries["link"], json!({"Symlink": "../bin/run"}), "{tree:#}");
    let subtree = &entries["sub"]["Directory"];
    assert_eq!(subtree["kind"], json!("bloomery.tree"), "{tree:#}");
    assert!(subtree["value"]["entries"]["leaf.txt"]["File"].is_string(), "{tree:#}");
    Ok(())
}

/// A stored kind citing many texts. Linked into this test binary, so the in-process engine's storage-kind inventory
/// holds its schema as it holds any native kind's.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.inspect.fan_out")]
struct FanOut {
    texts: Vec<Ref<Utf8Text>>,
}

#[test]
fn resolution_stops_at_its_artifact_budget_and_leaves_the_rest_hex() -> Result<(), Box<dyn Error>> {
    // Catches an unbounded fan-out: an artifact citing more texts than one request resolves.
    let mut batch = Batch::new();
    let texts = (0..MAX_ARTIFACTS + 8).map(|index| batch.stage_text(&format!("text {index}"))).collect();
    let fan_out = batch.stage_encoded(&FanOut { texts })?;
    let mut harness = BloomeryHarness::start([batch]);

    let answer = harness.inspect_artifact(&InspectArtifact { digest: *fan_out.digest().as_bytes(), depth: 1 });
    let InspectArtifactResult::Found { kind, json, truncated: true, .. } = answer else {
        panic!("a fan-out past the budget is found and truncated: {answer:?}");
    };
    assert_eq!(kind.as_deref(), Some("test.inspect.fan_out"));
    let fan_out: Value = serde_json::from_str(&json)?;
    let texts = fan_out["texts"].as_array().unwrap_or_else(|| panic!("the texts read as an array: {fan_out:#}"));
    assert_eq!(texts.len(), MAX_ARTIFACTS + 8);
    let resolved = texts.iter().filter(|text| text["value"].is_string()).count();
    let hex = texts.iter().filter(|text| text.as_str().is_some_and(|hex| hex.len() == 64)).count();
    assert_eq!((resolved, hex), (MAX_ARTIFACTS, 8), "{fan_out:#}");
    Ok(())
}
