//! Rows a pre-#5278 binary actually wrote, decoded through their pinned
//! upcasts (#5500).
//!
//! `pre-propose-decisions.bin` is the complete pre-fold representative row —
//! every effect family as of the shape stamped `ee7c8fce…` — and
//! `pre-propose-event.bin` is a pre-fold event row stamped `0e738994…`.
//! `pre-study-decisions.bin` is the same thing one shape later: the complete
//! representative row as of `7ed0db5b…`, the shape every decisions row written
//! before the ADR-0216 reader bore. There is no regen command for any of them:
//! the bytes are history. Each pinned upcast decodes as the current shape
//! because everything since is a tail-appended enum variant, so when one of
//! these stops decoding, a change has moved wire positions those rows still
//! occupy — the remedy is a real frozen decode shape for the pinned digest,
//! never new bytes here.

use aether_bloomery::persisted::{
    DECISIONS_PRE_PROPOSE_DIGEST, DECISIONS_PRE_STUDY_DIGEST, EVENT_PRE_PROPOSE_DIGEST, EVENT_PRE_STUDY_DIGEST,
    SCOPE_RUN, SCOPE_RUN_PRE_PIN_DIGEST, decode_recorded_decisions, decode_recorded_event, decode_reshaped,
};
use aether_bloomery::testing::surface_overlap_event;
use aether_bloomery::{Digest, ScopeRun, ScopeRunPrePin, WorkpieceId};
use aether_data::wire::to_vec;

const PRE_PROPOSE_DECISIONS: &[u8] = include_bytes!("fixtures/pre-propose-decisions.bin");
const PRE_PROPOSE_EVENT: &[u8] = include_bytes!("fixtures/pre-propose-event.bin");
const PRE_STUDY_DECISIONS: &[u8] = include_bytes!("fixtures/pre-study-decisions.bin");

#[test]
fn a_pre_propose_decisions_row_decodes_through_its_pinned_upcast() {
    let decoded = decode_recorded_decisions(PRE_PROPOSE_DECISIONS, Some(DECISIONS_PRE_PROPOSE_DIGEST.as_bytes()))
        .expect("a row stamped ee7c8fce… decodes through the pre-propose upcast");
    assert!(!decoded.effects.is_empty(), "the pre-fold representative row carries every effect family");
}

#[test]
fn a_pre_propose_event_row_decodes_through_its_pinned_upcast() {
    let decoded = decode_recorded_event(PRE_PROPOSE_EVENT, Some(EVENT_PRE_PROPOSE_DIGEST.as_bytes()))
        .expect("a row stamped 0e738994… decodes through the pre-propose upcast");
    assert_eq!(decoded, surface_overlap_event());
}

#[test]
fn a_pre_study_decisions_row_decodes_through_its_pinned_upcast() {
    // Tripwire: ADR-0216 appended `Decision::DispatchStudy` and two `Outcome`
    // variants. These are the bytes the previous binary actually wrote for the
    // complete representative row, so a variant *inserted* rather than appended
    // shifts a discriminant this row still occupies and fails here — which is
    // the boot-replay abort, moved forward to the change that causes it.
    let decoded = decode_recorded_decisions(PRE_STUDY_DECISIONS, Some(DECISIONS_PRE_STUDY_DIGEST.as_bytes()))
        .expect("a row stamped 7ed0db5b… decodes through the pre-study upcast");
    assert!(!decoded.effects.is_empty(), "the pre-reader representative row carries every effect family");
}

#[test]
fn a_pre_study_event_row_decodes_through_its_pinned_upcast() {
    // The event column's half of the same append: `Fact::StudyCompleted` sits
    // past every discriminant a pre-reader row could hold, so a row stamped
    // with the pre-reader event identity still decodes as today's shape. The
    // pre-propose bytes are a real row of that era — the shape did not move
    // between the two stamps, only the identity did.
    let decoded = decode_recorded_event(PRE_PROPOSE_EVENT, Some(EVENT_PRE_STUDY_DIGEST.as_bytes()))
        .expect("a row stamped a12b797c… decodes through the pre-study upcast");
    assert_eq!(decoded, surface_overlap_event());
}

#[test]
fn a_pre_pin_scope_run_decodes_with_the_pin_absent() {
    // The plausible bug: appending `instructions` leaves old run-record bytes
    // unreadable, so a scoping run opened before the pin cannot be replayed
    // and looks like a store fault rather than an unpinned run. The upcast
    // fills the new field as absent — the honest reading of a run that named
    // no bundle.
    let prior = ScopeRunPrePin {
        commission: WorkpieceId("wp-scope".to_owned()),
        ordinal: 1,
        intent: Digest::from_bytes([1; 32]),
        base: Digest::from_bytes([2; 32]),
        subject: Digest::from_bytes([3; 32]),
    };
    let bytes = to_vec(&prior).expect("a pre-pin run record encodes");
    let decoded: ScopeRun = decode_reshaped(&SCOPE_RUN, Some(SCOPE_RUN_PRE_PIN_DIGEST.as_bytes()), &bytes)
        .expect("a pre-pin run record decodes through its pinned upcast");

    assert!(decoded.instructions.is_none(), "a run opened before the pin names no bundle");
    assert_eq!(decoded.commission, prior.commission);
    assert_eq!(decoded.ordinal, prior.ordinal);
    assert_eq!(decoded.intent, prior.intent);
    assert_eq!(decoded.base, prior.base);
    assert_eq!(decoded.subject, prior.subject);
}
