//! Driver mail: pinned kind ids and the atomic `SetHeads` constructor.

use aether_bloomery_kinds::{
    AwaitProcessed, Call, CallInput, CallOutcome, CallProgram, ClosureArtifact, Digest, Head, HeadChange,
    LEGACY_CALL_PROGRAM_ID, LEGACY_SET_HEAD_ID, OpaqueBytes, Processed, ProgramName, RecordedHead, RecordedHeadMove,
    Ref, SetHeads, Tree, Utf8Text, decode_call_program, decode_set_heads,
};
use aether_data::{Citations, Cites, Kind, KindId, Storage, StorageData, StorageError};

#[aether_data::kind(name = "aether.bloomery.driver.call_program", eq, no_serde)]
struct LegacyCallProgram {
    program: Head<OpaqueBytes>,
    name: ProgramName,
    input: Digest,
}

#[aether_data::kind(name = "aether.bloomery.driver.set_head", eq, no_serde)]
struct LegacySetHead {
    head: RecordedHead,
    from: Option<Digest>,
    to: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.driver.mixed-input")]
struct MixedInput {
    count: u64,
    text: Ref<Utf8Text>,
}

#[derive(Clone)]
struct FailingInput;

impl Kind for FailingInput {
    const NAME: &'static str = "test.bloomery.driver.failing-input";
    const ID: KindId = aether_data::storage_kind_id_from_name(Self::NAME);
}

impl Storage for FailingInput {
    fn decode_storage(_bytes: &[u8]) -> Result<StorageData<Self>, StorageError> {
        Err(StorageError::TrailingBytes)
    }

    fn encode_storage(_data: &StorageData<Self>) -> Result<Vec<u8>, StorageError> {
        Err(StorageError::NestingTooDeep)
    }
}

impl Cites for FailingInput {
    fn cites(&self, _sink: &mut Citations) {}
}

#[test]
fn the_driver_mail_kind_ids_are_pinned() {
    // Tripwire: mail KindIds hash the canonical schema. ReactorIntent.kind
    // is compared against CallProgram::ID / SetHeads::ID, and the mail
    // registry refuses a second schema under an existing name, so drift
    // makes bundles built against the old schema unloadable beside new
    // ones and strands in-flight intents.
    assert_eq!(LEGACY_CALL_PROGRAM_ID, TRIPWIRE_LEGACY_CALL_PROGRAM);
    assert_eq!(LegacyCallProgram::ID, TRIPWIRE_LEGACY_CALL_PROGRAM);
    assert_eq!(CallProgram::ID, TRIPWIRE_CALL_PROGRAM_V2);
    assert_eq!(LEGACY_SET_HEAD_ID, TRIPWIRE_LEGACY_SET_HEAD);
    assert_eq!(LegacySetHead::ID, TRIPWIRE_LEGACY_SET_HEAD);
    assert_eq!(SetHeads::ID, TRIPWIRE_SET_HEADS);
    assert_eq!(Call::ID, TRIPWIRE_CALL);
    assert_eq!(CallOutcome::ID, TRIPWIRE_CALL_OUTCOME);
    assert_eq!(AwaitProcessed::ID, TRIPWIRE_AWAIT_PROCESSED);
    assert_eq!(Processed::ID, TRIPWIRE_PROCESSED);
}

const TRIPWIRE_LEGACY_CALL_PROGRAM: KindId = KindId(0x298e_86d0_91bf_d585);
const TRIPWIRE_CALL_PROGRAM_V2: KindId = KindId(0x2d35_cd93_db36_da13);
const TRIPWIRE_LEGACY_SET_HEAD: KindId = KindId(0x2842_0dd2_d83a_65e8);
const TRIPWIRE_SET_HEADS: KindId = KindId(0x2f03_537c_84d9_dd5d);
const TRIPWIRE_CALL: KindId = KindId(0x2dcc_bc68_65cc_027a);
const TRIPWIRE_CALL_OUTCOME: KindId = KindId(0x2ed5_fa91_cb11_9000);
const TRIPWIRE_AWAIT_PROCESSED: KindId = KindId(0x28c8_2171_74e2_f5b0);
const TRIPWIRE_PROCESSED: KindId = KindId(0x2f9f_49f9_0af8_5287);

#[test]
fn call_program_decodes_both_wire_generations_without_fallback() {
    let program = Head::new("test.program");
    let name = ProgramName::new("summarize").expect("program name");
    let input = Digest::from_bytes([7; 32]);
    let legacy = LegacyCallProgram { program: program.clone(), name: name.clone(), input };
    assert_eq!(
        decode_call_program(LegacyCallProgram::ID, &legacy.encode_into_bytes()),
        Some(CallProgram { program: program.clone(), name: name.clone(), input: CallInput::Stored(input) })
    );

    let stored = CallProgram { program, name, input: CallInput::Stored(input) };
    assert_eq!(decode_call_program(CallProgram::ID, &stored.encode_into_bytes()), Some(stored));

    assert!(decode_call_program(CallProgram::ID, &legacy.encode_into_bytes()).is_none());
    assert!(decode_call_program(LegacyCallProgram::ID, &[1, 2, 3]).is_none());
    assert!(decode_call_program(KindId(9), &[]).is_none());
}

#[test]
fn set_heads_decodes_both_wire_generations_without_fallback() {
    let tree_head = Head::<Tree>::new("tree");
    let text_head = Head::<Utf8Text>::new("text");
    let tree_from = Ref::<Tree>::from_digest(Digest::from_bytes([1; 32]));
    let tree_to = Ref::<Tree>::from_digest(Digest::from_bytes([2; 32]));
    let text_to = Ref::<Utf8Text>::from_digest(Digest::from_bytes([3; 32]));
    let group = SetHeads::new(vec![
        HeadChange::new(&tree_head, Some(tree_from), tree_to),
        HeadChange::new(&text_head, None, text_to),
    ]);
    assert_eq!(decode_set_heads(SetHeads::ID, &group.encode_into_bytes()), Some(group));

    let legacy =
        LegacySetHead { head: RecordedHead::from(&tree_head), from: Some(tree_from.digest()), to: tree_to.digest() };
    let decoded = decode_set_heads(LegacySetHead::ID, &legacy.encode_into_bytes()).expect("legacy singleton");
    assert_eq!(decoded.changes().len(), 1);
    assert_eq!(decoded.changes()[0].head(), &RecordedHead::from(&tree_head));
    assert_eq!(decoded.changes()[0].from(), Some(tree_from.digest()));
    assert_eq!(decoded.changes()[0].to(), tree_to.digest());

    assert!(decode_set_heads(SetHeads::ID, &legacy.encode_into_bytes()).is_none());
    assert!(decode_set_heads(LegacySetHead::ID, &[1, 2, 3]).is_none());
    assert!(decode_set_heads(KindId(9), &[]).is_none());
}

#[test]
fn with_input_preserves_scalar_values_and_reference_citations() -> Result<(), StorageError> {
    let program = Head::new("test.program");
    let name = ProgramName::new("summarize").expect("program name");
    let text = Ref::<Utf8Text>::from_digest(Digest::from_bytes([3; 32]));
    let value = MixedInput { count: 42, text };

    let call = CallProgram::with_input(program.clone(), name.clone(), &value)?;
    let CallInput::Value(input) = &call.input else {
        panic!("with_input must carry an encoded value");
    };
    assert_eq!(input.kind(), MixedInput::ID);
    assert_eq!(input.citations().len(), 1);
    assert_eq!(input.citations()[0].kind(), Utf8Text::ID);
    assert_eq!(input.citations()[0].bytes(), text.digest().as_bytes());
    let input_digest = input.digest();
    let (kind, payload, _) = input.clone().into_parts();
    let payload = ClosureArtifact::new(kind, payload).load(input_digest).expect("the payload reads whole");
    assert_eq!(MixedInput::decode_storage(&payload)?.value, value);
    assert_eq!(CallProgram::decode_from_bytes(&call.encode_into_bytes()), Some(call.clone()));

    let stored = CallProgram { program, name, input: CallInput::Stored(input_digest) };
    assert_eq!(CallProgram::decode_from_bytes(&stored.encode_into_bytes()), Some(stored));
    Ok(())
}

#[test]
fn with_input_returns_the_storage_error() {
    let error = CallProgram::with_input(
        Head::new("test.program"),
        ProgramName::new("summarize").expect("program name"),
        &FailingInput,
    )
    .expect_err("failing storage encode");
    assert_eq!(error, StorageError::NestingTooDeep);
}

#[test]
fn head_change_moves_to_the_destination_not_the_expected_binding() {
    // Catches a constructor or `to_move` that swapped the two same-typed
    // digests, which would make every CAS move land on the old binding
    // instead of the new one.
    const HEAD: Head<Tree> = Head::new("main");
    let a = Ref::<Tree>::from_digest(Digest::from_bytes([1; 32]));
    let b = Ref::<Tree>::from_digest(Digest::from_bytes([2; 32]));

    let change = HeadChange::new(&HEAD, Some(a), b);

    assert_eq!(change.from(), Some(a.digest()));
    assert_eq!(change.to(), b.digest());
    assert_eq!(change.to_move(), RecordedHeadMove::from(&HEAD.move_to(b)));
}
