//! One `muse.turn` driven through the guest invocation seam with a recorded reply.

use std::error::Error;

use aether_bloomery_kinds::{
    ClosureArtifact, DigestMismatch, EncodedArtifact, Invoke, Invoked, ProgramApi, ProgramName, Ref, Refusal, Utf8Text,
};
use aether_bloomery_muse::{
    CallId, Endpoint, ModelName, MuseTurn, OfferedTool, OfferedTools, OutputBudget, ReasoningEffort, Role, ToolCall,
    ToolInput, ToolOutput, TurnInput, TurnItem, TurnItems, TurnOutcome, TurnResult,
};
use aether_bloomery_program::{
    AsyncSession, Pending, PendingCall, PollResult, Program, Started, ToolSchema, start_async, tool_definition,
};
use aether_data::{Cites, Kind, Storage, StorageData};
use aether_http::{Fetch, FetchResult, HttpError, HttpHeader};

const COMPLETED: &str = include_str!("../fixtures/completed.json");
const OVERLOADED: &str = include_str!("../fixtures/overloaded.json");
const CALLED: &str = include_str!("../fixtures/called.json");
const URL: &str = "https://example.test/v1/responses";

/// Stands in for `workspace.read`'s input and result, so the turn decodes and renders a type it does not link.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.muse.read_input")]
struct ReadInput {
    path: String,
}

/// One offered tool: the program, the definition sent for it, and its input and result schemas.
struct Tool {
    program: &'static str,
    definition: String,
    input: ToolSchema,
    result: ToolSchema,
}

/// A staged artifact's payload, read whole and verified against its digest.
fn payload(artifact: &EncodedArtifact) -> Result<Vec<u8>, DigestMismatch> {
    let (kind, bytes, _) = artifact.clone().into_parts();
    ClosureArtifact::new(kind, bytes).load(artifact.digest())
}

/// `value` as the closure member that stores it.
fn stored<K: Storage + Clone + Cites>(value: &K) -> Result<ClosureArtifact, Box<dyn Error>> {
    let (kind, payload, _) = EncodedArtifact::new(value)?.into_parts();
    Ok(ClosureArtifact::new(kind, payload))
}

/// `text` as the closure member that stores it.
fn text(text: &str) -> ClosureArtifact {
    ClosureArtifact::new(Utf8Text::ID, text.as_bytes().to_vec())
}

/// The conversation every turn opens with, and the closure members its texts are.
fn opening() -> (Vec<TurnItem>, Vec<ClosureArtifact>) {
    let texts = [(Role::Developer, "Be brief."), (Role::User, "What is a bloomery?")];
    let items = texts.iter().map(|&(role, said)| TurnItem::message(role, Ref::of_text(said))).collect();
    (items, texts.iter().map(|(_, said)| text(said)).collect())
}

/// Start one turn offering `tools` over `items`, whose closure carries the input, `closure`, and every offered
/// definition and schema, and return the session parked on its first cap send.
fn start_turn(
    tools: &[Tool],
    items: Vec<TurnItem>,
    mut closure: Vec<ClosureArtifact>,
) -> Result<(AsyncSession, PendingCall), Box<dyn Error>> {
    let mut offered = Vec::with_capacity(tools.len());
    for tool in tools {
        let (input, result) = (Ref::of_encoded(&tool.input)?, Ref::of_encoded(&tool.result)?);
        offered.push(OfferedTool::new(ProgramName::new(tool.program)?, Ref::of_text(&tool.definition), input, result));
        closure.extend([text(&tool.definition), stored(&tool.input)?, stored(&tool.result)?]);
    }
    let input = TurnInput::new(
        Endpoint::new(URL)?,
        ModelName::new("muse-spark-1.3")?,
        OfferedTools::new(offered)?,
        TurnItems::new(items)?,
        OutputBudget::new(512)?,
        ReasoningEffort::Low,
    );
    let input_artifact = stored(&input)?;
    closure.push(input_artifact.clone());

    let invoke = Invoke::new(7, ProgramName::new(MuseTurn::NAME)?, input_artifact.claimed().unverified(), closure);
    match start_async::<MuseTurn>(invoke) {
        Started::Finished(other) => Err(format!("expected a parked fetch, got Finished {other:?}").into()),
        Started::Live { session, waiting: Some(Pending::Send(pending)) } => Ok((session, pending)),
        Started::Live { waiting, .. } => Err(format!("expected the first poll to send, got {waiting:?}").into()),
    }
}

/// The opening conversation with no tools offered.
fn start_plain_turn() -> Result<(AsyncSession, PendingCall), Box<dyn Error>> {
    let (items, closure) = opening();
    start_turn(&[], items, closure)
}

#[test]
fn a_turn_sends_one_fetch_and_stages_the_reply_it_cites() -> Result<(), Box<dyn Error>> {
    // Catches a result citing an unstaged artifact, texts not read from the injected closure, a fetch to
    // the wrong target or reply kind, and a second fetch per turn.
    let (mut session, pending) = start_plain_turn()?;
    assert_eq!(pending.api, ProgramApi::Http);
    assert_eq!(pending.kind_id, Fetch::ID);
    assert_eq!(pending.expected_reply, FetchResult::ID);

    let reply = FetchResult::Ok {
        request_id: 1,
        url: URL.into(),
        status: 200,
        headers: Vec::new(),
        body: COMPLETED.as_bytes().to_vec(),
    };
    session.fulfill_send(&pending, FetchResult::ID, reply.encode_into_bytes());
    let PollResult::Finished(Invoked::Completed { seq: 7, result, staged }) = session.poll() else {
        panic!("expected the turn to complete after its one fetch");
    };

    let result_artifact = staged.iter().find(|artifact| artifact.digest() == result).ok_or("result is staged")?;
    let recorded = TurnResult::decode_storage(&payload(result_artifact)?)?.value;
    let TurnOutcome::Completed { text, usage } = recorded.outcome() else {
        panic!("expected Completed, got {:?}", recorded.outcome());
    };
    assert_eq!(recorded.status().get(), 200);
    assert_eq!(usage.cached_input_tokens(), 1024);
    assert_eq!(*text, Ref::of_text("A bloomery is a furnace that smelts iron into a bloom."));
    assert_eq!(
        staged,
        vec![
            EncodedArtifact::opaque_bytes(COMPLETED.as_bytes()),
            EncodedArtifact::text("A bloomery is a furnace that smelts iron into a bloom."),
            EncodedArtifact::new(&recorded)?,
        ],
        "the body and the text are staged, and the result cites them"
    );
    assert_eq!(recorded.body(), Ref::of_bytes(COMPLETED.as_bytes()));
    Ok(())
}

#[test]
fn a_transient_refusal_is_recorded_once_with_its_retry_after() -> Result<(), Box<dyn Error>> {
    // Catches the `Retry-After` header not threaded from the fetch reply into the record, an overload
    // recorded as terminal, the refusal body dropped, and a hidden in-run retry.
    let (mut session, pending) = start_plain_turn()?;
    let reply = FetchResult::Ok {
        request_id: 1,
        url: URL.into(),
        status: 503,
        headers: vec![HttpHeader { name: "Retry-After".into(), value: "7".into() }],
        body: OVERLOADED.as_bytes().to_vec(),
    };
    session.fulfill_send(&pending, FetchResult::ID, reply.encode_into_bytes());
    let (result, staged) = match session.poll() {
        PollResult::Finished(Invoked::Completed { seq: 7, result, staged }) => (result, staged),
        other => panic!("expected the turn to complete after its one fetch, with no second send; got {other:?}"),
    };

    let result_artifact = staged.iter().find(|artifact| artifact.digest() == result).ok_or("result is staged")?;
    let recorded = TurnResult::decode_storage(&payload(result_artifact)?)?.value;
    assert_eq!(*recorded.outcome(), TurnOutcome::Transient { retry_after_secs: Some(7) });
    assert_eq!(recorded.status().get(), 503);
    assert_eq!(recorded.body(), Ref::of_bytes(OVERLOADED.as_bytes()));
    assert_eq!(
        staged,
        vec![EncodedArtifact::opaque_bytes(OVERLOADED.as_bytes()), EncodedArtifact::new(&recorded)?],
        "the body is staged, and the result cites it"
    );
    Ok(())
}

#[test]
fn a_turn_with_no_reply_refuses_with_the_http_error() -> Result<(), Box<dyn Error>> {
    // Catches a turn that never reached the vendor being recorded as answered.
    let (mut session, pending) = start_plain_turn()?;
    let reply = FetchResult::Err { request_id: 1, url: URL.into(), error: HttpError::Timeout };
    session.fulfill_send(&pending, FetchResult::ID, reply.encode_into_bytes());

    match session.poll() {
        PollResult::Finished(Invoked::Refused { seq: 7, refusal: Refusal::Refused { reason } }) => {
            assert!(reason.as_str().contains("Timeout"), "{}", reason.as_str());
            Ok(())
        }
        other => panic!("expected Refused naming the HTTP error, got {other:?}"),
    }
}

#[test]
fn a_turn_offers_its_tools_and_records_the_calls_it_is_asked_for() -> Result<(), Box<dyn Error>> {
    // Catches a definition or schema read from outside the injected closure or a definition left out of the
    // request, a result citing unstaged arguments, arguments decoded against the wrong schema or into bytes the
    // input's own `Storage` impl does not read, an unstaged decode, and a refused decode recorded as a decode.
    let read_definition = serde_json::json!({
        "type": "function",
        "name": "workspace-read",
        "description": "Read one workspace file.",
        "parameters": { "type": "object" },
        "strict": false,
    });
    let tools = [
        Tool {
            program: "muse.turn",
            definition: tool_definition::<MuseTurn>()?.to_string(),
            input: ToolSchema::of::<TurnInput>(),
            result: ToolSchema::of::<TurnResult>(),
        },
        Tool {
            program: "workspace.read",
            definition: read_definition.to_string(),
            input: ToolSchema::of::<ReadInput>(),
            result: ToolSchema::of::<ReadInput>(),
        },
    ];
    let (items, closure) = opening();
    let (mut session, pending) = start_turn(&tools, items, closure)?;

    let fetch = Fetch::decode_from_bytes(&pending.api_call(1).payload).ok_or("the pending call is a fetch")?;
    let sent: serde_json::Value = serde_json::from_slice(&fetch.body)?;
    let sent_names: Vec<_> =
        sent["tools"].as_array().ok_or("tools are sent")?.iter().map(|tool| &tool["name"]).collect();
    assert_eq!(sent_names, ["muse-turn", "workspace-read"], "both offered definitions are sent, in order");

    let reply = FetchResult::Ok {
        request_id: 1,
        url: URL.into(),
        status: 200,
        headers: Vec::new(),
        body: CALLED.as_bytes().to_vec(),
    };
    session.fulfill_send(&pending, FetchResult::ID, reply.encode_into_bytes());
    let PollResult::Finished(Invoked::Completed { seq: 7, result, staged }) = session.poll() else {
        panic!("expected the turn to complete after its one fetch");
    };

    let staged_payload = |digest| -> Result<Vec<u8>, Box<dyn Error>> {
        let artifact = staged.iter().find(|artifact| artifact.digest() == digest).ok_or("the artifact is staged")?;
        Ok(payload(artifact)?)
    };
    let recorded = TurnResult::decode_storage(&staged_payload(result)?)?.value;
    let TurnOutcome::Called { calls, text, .. } = recorded.outcome() else {
        panic!("expected Called, got {:?}", recorded.outcome());
    };
    let programs: Vec<_> = calls.as_slice().iter().map(|call| call.program().as_str()).collect();
    assert_eq!(programs, ["workspace.read", "muse.turn"]);
    for cited in calls.as_slice().iter().map(ToolCall::arguments).chain([*text]) {
        staged_payload(cited.digest())?;
    }

    let [read, turn] = calls.as_slice() else {
        panic!("expected two calls, got {calls:?}");
    };
    let ToolInput::Decoded(decoded) = read.input() else {
        panic!("expected the read's arguments to decode, got {:?}", read.input());
    };
    let expected = ReadInput { path: "notes/bloomery.md".into() };
    assert_eq!(decoded.cast::<ReadInput>(), Some(Ref::of_encoded(&expected)?), "stored under the input's kind");
    assert_eq!(staged_payload(decoded.digest())?, ReadInput::encode_storage(&StorageData::from_value(expected))?);
    let ToolInput::Refused(refusal) = turn.input() else {
        panic!("expected the partial turn input to refuse, got {:?}", turn.input());
    };
    let refusal = String::from_utf8(staged_payload(refusal.digest())?)?;
    assert!(refusal.starts_with("The arguments do not decode as muse.turn.input: "), "{refusal}");
    Ok(())
}

#[test]
fn a_turn_replays_a_rendered_result_and_a_refusal_as_call_outputs() -> Result<(), Box<dyn Error>> {
    // Catches a result read from outside the injected closure, a rendering that is not the pinned bytes, and a
    // refusal rendered instead of sent as its stored text.
    let read_arguments = r#"{"path": "notes/bloomery.md"}"#;
    let turn_arguments = r#"{"endpoint":"https://example.test/v1/responses"}"#;
    let refusal = "The arguments do not decode as muse.turn.input.";
    let read_result = ReadInput { path: "notes/bloomery.md".into() };
    let result_schema = ToolSchema::of::<ReadInput>();
    let result_digest = Ref::of_encoded(&read_result)?.digest();

    let call = |id: &str, program: &str, arguments: &str| -> Result<TurnItem, Box<dyn Error>> {
        let input = ToolInput::Refused(Ref::of_text(refusal));
        Ok(TurnItem::Call(ToolCall::new(CallId::new(id)?, ProgramName::new(program)?, Ref::of_text(arguments), input)))
    };
    let (mut items, mut closure) = opening();
    items.extend([
        call("call_read", "workspace.read", read_arguments)?,
        call("call_turn", "muse.turn", turn_arguments)?,
        TurnItem::CallOutput {
            call_id: CallId::new("call_read")?,
            output: ToolOutput::result(Ref::of_encoded(&result_schema)?, &result_schema, result_digest),
        },
        TurnItem::CallOutput { call_id: CallId::new("call_turn")?, output: ToolOutput::Refused(Ref::of_text(refusal)) },
    ]);
    closure.extend([text(read_arguments), text(turn_arguments), text(refusal)]);
    closure.extend([stored(&result_schema)?, stored(&read_result)?]);
    let (_, pending) = start_turn(&[], items, closure)?;

    let fetch = Fetch::decode_from_bytes(&pending.api_call(1).payload).ok_or("the pending call is a fetch")?;
    let sent: serde_json::Value = serde_json::from_slice(&fetch.body)?;
    let outputs: Vec<_> = sent["input"]
        .as_array()
        .ok_or("items are sent")?
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| (item["call_id"].as_str(), item["output"].as_str()))
        .collect();
    // Tripwire: the rendered result bytes are the prompt prefix a later turn resends, which must stay byte-stable.
    assert_eq!(
        outputs,
        [(Some("call_read"), Some(r#"{"path":"notes/bloomery.md"}"#)), (Some("call_turn"), Some(refusal))]
    );
    Ok(())
}
