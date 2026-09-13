//! Rows a pre-#5278 binary actually wrote, decoded through their pinned
//! upcasts (#5500).
//!
//! `pre-propose-decisions.bin` is the complete pre-fold representative row —
//! every effect family as of the shape stamped `ee7c8fce…` — and
//! `pre-propose-event.bin` is a pre-fold event row stamped `0e738994…`.
//! `pre-study-decisions.bin` is the same thing one shape later: the complete
//! representative row as of `7ed0db5b…`, the shape every decisions row written
//! before the ADR-0216 reader bore. The `pre-precheck-*` pair preserves the last
//! decisions and event rows from before aggregate pre-check vocabulary existed,
//! and the `pre-coordination-*` pair the last rows from before shared
//! verification and eager integration.
//! There is no regen command for any of them: the bytes are history. Each
//! pinned upcast decodes as the current shape
//! because everything since is a tail-appended enum variant, so when one of
//! these stops decoding, a change has moved wire positions those rows still
//! occupy — the remedy is a real frozen decode shape for the pinned digest,
//! never new bytes here.

use aether_bloomery::persisted::{
    DECISIONS_PRE_COORDINATION_DIGEST, DECISIONS_PRE_PRECHECK_DIGEST, DECISIONS_PRE_PROPOSE_DIGEST,
    DECISIONS_PRE_STUDY_DIGEST, EVENT_PRE_COORDINATION_DIGEST, EVENT_PRE_PRECHECK_DIGEST, EVENT_PRE_PROPOSE_DIGEST,
    EVENT_PRE_STUDY_DIGEST, decode_recorded_decisions, decode_recorded_event,
};
use aether_bloomery::testing::{containment_refused_event, surface_overlap_event};

const PRE_PROPOSE_DECISIONS: &[u8] = include_bytes!("fixtures/pre-propose-decisions.bin");
const PRE_PROPOSE_EVENT: &[u8] = include_bytes!("fixtures/pre-propose-event.bin");
const PRE_STUDY_DECISIONS: &[u8] = include_bytes!("fixtures/pre-study-decisions.bin");
const PRE_PRECHECK_DECISIONS: &[u8] = include_bytes!("fixtures/pre-precheck-decisions.bin");
const PRE_PRECHECK_EVENT: &[u8] = include_bytes!("fixtures/pre-precheck-event.bin");
const PRE_COORDINATION_DECISIONS: &[u8] = include_bytes!("fixtures/pre-coordination-decisions.bin");
const PRE_COORDINATION_EVENT: &[u8] = include_bytes!("fixtures/pre-coordination-event.bin");

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
fn a_pre_precheck_decisions_row_decodes_through_its_pinned_upcast() {
    let decoded = decode_recorded_decisions(PRE_PRECHECK_DECISIONS, Some(DECISIONS_PRE_PRECHECK_DIGEST.as_bytes()))
        .expect("a row stamped f44f4243… decodes through the pre-pre-check upcast");
    assert!(!decoded.effects.is_empty(), "the pre-pre-check row retains its complete historical vocabulary");
}

#[test]
fn a_pre_precheck_event_row_decodes_through_its_pinned_upcast() {
    let decoded = decode_recorded_event(PRE_PRECHECK_EVENT, Some(EVENT_PRE_PRECHECK_DIGEST.as_bytes()))
        .expect("a row stamped 485537c3… decodes through the pre-pre-check upcast");
    assert_eq!(decoded, containment_refused_event());
}

#[test]
fn a_pre_coordination_decisions_row_decodes_through_its_pinned_upcast() {
    let decoded =
        decode_recorded_decisions(PRE_COORDINATION_DECISIONS, Some(DECISIONS_PRE_COORDINATION_DIGEST.as_bytes()))
            .expect("a row stamped cd1234d2… decodes through the pre-coordination upcast");
    assert!(!decoded.effects.is_empty(), "the pre-coordination row retains its complete historical vocabulary");
}

#[test]
fn a_pre_coordination_event_row_decodes_through_its_pinned_upcast() {
    let decoded = decode_recorded_event(PRE_COORDINATION_EVENT, Some(EVENT_PRE_COORDINATION_DIGEST.as_bytes()))
        .expect("a row stamped 55af52a6… decodes through the pre-coordination upcast");
    assert_eq!(decoded, containment_refused_event());
}
