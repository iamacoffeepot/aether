//! One `muse.turn` driven through the guest invocation seam with a recorded reply.

use std::error::Error;

use aether_bloomery_kinds::{ClosureArtifact, EncodedArtifact, Invoke, Invoked, ProgramName, Ref, Refusal, Utf8Text};
use aether_bloomery_muse::{
    Endpoint, ModelName, MuseTurn, OutputBudget, ReasoningEffort, Role, TurnInput, TurnItem, TurnItems, TurnOutcome,
    TurnResult,
};
use aether_bloomery_program::{AsyncSession, Pending, PendingCall, PollResult, Program, Started, start_async};
use aether_data::{Kind, Storage};
use aether_http::{Fetch, FetchResult, HttpError, HttpHeader};

const COMPLETED: &str = include_str!("../fixtures/completed.json");
const OVERLOADED: &str = include_str!("../fixtures/overloaded.json");
const URL: &str = "https://example.test/v1/responses";

/// Start one turn whose closure carries the input and every cited text, and
/// return the session parked on its first cap send.
fn start_turn() -> Result<(AsyncSession, PendingCall), Box<dyn Error>> {
    let texts = [(Role::Developer, "Be brief."), (Role::User, "What is a bloomery?")];
    let items = texts.iter().map(|&(role, text)| TurnItem::new(role, Ref::of_text(text))).collect();
    let input = TurnInput::new(
        Endpoint::new(URL)?,
        ModelName::new("muse-spark-1.3")?,
        TurnItems::new(items)?,
        OutputBudget::new(512)?,
        ReasoningEffort::Low,
    );
    let encoded = EncodedArtifact::new(&input)?;
    let input_artifact = ClosureArtifact::new(encoded.kind(), encoded.bytes().to_vec());
    let mut closure: Vec<_> =
        texts.iter().map(|(_, text)| ClosureArtifact::new(Utf8Text::ID, text.as_bytes().to_vec())).collect();
    closure.push(input_artifact.clone());

    let invoke = Invoke::new(7, ProgramName::new(MuseTurn::NAME)?, input_artifact.digest(), closure);
    match start_async::<MuseTurn>(invoke) {
        Started::Finished(other) => Err(format!("expected a parked fetch, got Finished {other:?}").into()),
        Started::Live { session, waiting: Some(Pending::Send(pending)) } => Ok((session, pending)),
        Started::Live { waiting, .. } => Err(format!("expected the first poll to send, got {waiting:?}").into()),
    }
}

#[test]
fn a_turn_sends_one_fetch_and_stages_the_reply_it_cites() -> Result<(), Box<dyn Error>> {
    // Catches a result citing an unstaged artifact, texts not read from the injected closure, a fetch to
    // the wrong target or reply kind, and a second fetch per turn.
    let (mut session, pending) = start_turn()?;
    assert_eq!(pending.mailbox, "aether.http");
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
    let recorded = TurnResult::decode_storage(result_artifact.bytes())?.value;
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
    let (mut session, pending) = start_turn()?;
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
    let recorded = TurnResult::decode_storage(result_artifact.bytes())?.value;
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
    let (mut session, pending) = start_turn()?;
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
