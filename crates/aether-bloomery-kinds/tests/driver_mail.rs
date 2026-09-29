//! Driver mail: pinned kind ids and the `SetHead` constructor.

use aether_bloomery_kinds::{
    AwaitProcessed, Call, CallInput, CallOutcome, CallProgram, Digest, Head, LEGACY_CALL_PROGRAM_ID, OpaqueBytes,
    Processed, ProgramName, RecordedHeadMove, Ref, SetHead, Tree, Utf8Text, decode_call_program,
};
use aether_data::{Citations, Cites, Kind, KindId, Storage, StorageData, StorageError};

#[aether_data::kind(name = "aether.bloomery.driver.call_program", eq, no_serde)]
struct LegacyCallProgram {
    program: Head<OpaqueBytes>,
    name: ProgramName,
    input: Digest,
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
    // is compared against CallProgram::ID / SetHead::ID, and the mail
    // registry refuses a second schema under an existing name, so drift
    // makes bundles built against the old schema unloadable beside new
    // ones and strands in-flight intents.
    assert_eq!(LEGACY_CALL_PROGRAM_ID, TRIPWIRE_LEGACY_CALL_PROGRAM);
    assert_eq!(LegacyCallProgram::ID, TRIPWIRE_LEGACY_CALL_PROGRAM);
    assert_eq!(CallProgram::ID, TRIPWIRE_CALL_PROGRAM_V2);
    assert_eq!(SetHead::ID, TRIPWIRE_SET_HEAD);
    assert_eq!(Call::ID, TRIPWIRE_CALL);
    assert_eq!(CallOutcome::ID, TRIPWIRE_CALL_OUTCOME);
    assert_eq!(AwaitProcessed::ID, TRIPWIRE_AWAIT_PROCESSED);
    assert_eq!(Processed::ID, TRIPWIRE_PROCESSED);
}

const TRIPWIRE_LEGACY_CALL_PROGRAM: KindId = KindId(0x298e_86d0_91bf_d585);
const TRIPWIRE_CALL_PROGRAM_V2: KindId = KindId(0x2917_43a7_6596_2e0b);
const TRIPWIRE_SET_HEAD: KindId = KindId(0x2842_0dd2_d83a_65e8);
const TRIPWIRE_CALL: KindId = KindId(0x2dcc_bc68_65cc_027a);
const TRIPWIRE_CALL_OUTCOME: KindId = KindId(0x2001_04b9_b5ad_b9c4);
const TRIPWIRE_AWAIT_PROCESSED: KindId = KindId(0x28c8_2171_74e2_f5b0);
const TRIPWIRE_PROCESSED: KindId = KindId(0x2389_3dd7_e7c2_f075);

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
    assert_eq!(MixedInput::decode_storage(input.bytes())?.value, value);
    let input_digest = input.digest();
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
fn set_head_moves_to_the_destination_not_the_expected_binding() {
    // Catches a constructor or `to_move` that swapped the two same-typed
    // digests, which would make every CAS move land on the old binding
    // instead of the new one.
    const HEAD: Head<Tree> = Head::new("main");
    let a = Ref::<Tree>::from_digest(Digest::from_bytes([1; 32]));
    let b = Ref::<Tree>::from_digest(Digest::from_bytes([2; 32]));

    let set_head = SetHead::new(&HEAD, Some(a), b);

    assert_eq!(set_head.from(), Some(a.digest()));
    assert_eq!(set_head.to(), b.digest());
    assert_eq!(set_head.to_move(), RecordedHeadMove::from(&HEAD.move_to(b)));
}
