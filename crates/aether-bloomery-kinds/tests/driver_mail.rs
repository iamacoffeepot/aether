//! Driver mail: pinned kind ids and the `SetHead` constructor.

use aether_bloomery_kinds::{
    AwaitProcessed, Call, CallOutcome, CallProgram, Digest, Head, Processed, RecordedHeadMove, Ref, SetHead, Tree,
};
use aether_data::{Kind, KindId};

#[test]
fn the_driver_mail_kind_ids_are_pinned() {
    // Tripwire: mail KindIds hash the canonical schema. ReactorIntent.kind
    // is compared against CallProgram::ID / SetHead::ID, and the mail
    // registry refuses a second schema under an existing name, so drift
    // makes bundles built against the old schema unloadable beside new
    // ones and strands in-flight intents.
    assert_eq!(CallProgram::ID, TRIPWIRE_CALL_PROGRAM);
    assert_eq!(SetHead::ID, TRIPWIRE_SET_HEAD);
    assert_eq!(Call::ID, TRIPWIRE_CALL);
    assert_eq!(CallOutcome::ID, TRIPWIRE_CALL_OUTCOME);
    assert_eq!(AwaitProcessed::ID, TRIPWIRE_AWAIT_PROCESSED);
    assert_eq!(Processed::ID, TRIPWIRE_PROCESSED);
}

const TRIPWIRE_CALL_PROGRAM: KindId = KindId(0x298e_86d0_91bf_d585);
const TRIPWIRE_SET_HEAD: KindId = KindId(0x2842_0dd2_d83a_65e8);
const TRIPWIRE_CALL: KindId = KindId(0x2dcc_bc68_65cc_027a);
const TRIPWIRE_CALL_OUTCOME: KindId = KindId(0x2001_04b9_b5ad_b9c4);
const TRIPWIRE_AWAIT_PROCESSED: KindId = KindId(0x28c8_2171_74e2_f5b0);
const TRIPWIRE_PROCESSED: KindId = KindId(0x2389_3dd7_e7c2_f075);

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
