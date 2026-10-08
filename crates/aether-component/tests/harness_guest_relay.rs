//! Issue 7653: a guest sends a request through a reference it holds and
//! stores a context for the reply.
//!
//! The `test.context.relay` fixture keeps the party that joined it, holds the
//! reply to each `RelayQuery`, asks the party through that reference with the
//! held reply in the request's context, and answers its caller from the
//! reply handler. `test.context.party` is the party, from the same module.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;

use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::LoadComponent;
use aether_test_fixtures_kinds::{RELAY_PARTY_OFFSET, RelayIntroduce, RelayQuery, RelayQueryResult};

/// The relay's row this file sends. The relay ships only as a cdylib example,
/// so the test casts its `load_any` reference to this instead of naming a type.
#[aether_actor::protocol]
trait Relay {
    fn query(mail: RelayQuery) -> RelayQueryResult;
}

/// The party's row this file sends.
#[aether_actor::protocol]
trait Party {
    fn introduce(mail: RelayIntroduce);
}

fn load(wasm: &[u8], export: &str) -> LoadComponent {
    LoadComponent { wasm: wasm.to_vec(), name: None, config: Vec::new(), export: Some(export.to_owned()) }
}

/// The bug this catches: a guest's request through a held reference loses its
/// context, so the held reply is never answered (the caller gets
/// `Unanswered`) or is answered with a value other than the one the party
/// sent.
#[test]
fn a_guest_asks_its_party_and_answers_its_caller_from_the_reply_handler() {
    let Some(path) = require_wasm("context_relay") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");
    let wasm = fs::read(path).expect("read relay wasm");
    let (relay, _) = harness.load_any(&load(&wasm, "test.context.relay")).expect("load the relay");
    let (party, _) = harness.load_any(&load(&wasm, "test.context.party")).expect("load the party");
    let relay = harness.cast::<Relay>(relay).expect("the relay publishes RelayQuery");
    let party = harness.cast::<Party>(party).expect("the party publishes RelayIntroduce");

    let alone = harness
        .execute(vec![("alone", HarnessOp::send_and_await_reply(&relay, &RelayQuery { question: 7 }))])
        .expect("RelayQuery to the relay")
        .reply::<RelayQueryResult>("alone")
        .expect("decode the relay's reply");
    assert!(matches!(alone, RelayQueryResult::Nobody), "nobody has joined the relay yet");

    harness.execute(vec![("join", HarnessOp::send_and_settle(&party, &RelayIntroduce))]).expect("introduce the party");
    let asked = harness
        .execute(vec![("ask", HarnessOp::send_and_await_reply(&relay, &RelayQuery { question: 7 }))])
        .expect("RelayQuery to the relay")
        .reply::<RelayQueryResult>("ask")
        .expect("decode the relay's reply");

    let RelayQueryResult::Answered { value } = asked else {
        panic!("the relay did not answer its caller from the party's reply: {asked:?}");
    };
    assert_eq!(value, 7 + RELAY_PARTY_OFFSET, "the answer is the party's, carried through the stored context");
}
