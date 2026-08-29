//! Rows a pre-#5278 binary actually wrote, decoded through their pinned
//! upcasts (#5500).
//!
//! `pre-propose-decisions.bin` is the complete pre-fold representative row —
//! every effect family as of the shape stamped `ee7c8fce…` — and
//! `pre-propose-event.bin` is a pre-fold event row stamped `0e738994…`. There
//! is no regen command for either file: the bytes are history. The pre-#5278
//! upcasts decode as the current shape because everything since is a
//! tail-appended enum variant, so when one of these stops decoding, a change
//! has moved wire positions those rows still occupy — the remedy is a real
//! frozen decode shape for the pinned digest, never new bytes here.

use aether_bloomery::persisted::{
    DECISIONS_PRE_PROPOSE_DIGEST, EVENT_PRE_PROPOSE_DIGEST, decode_recorded_decisions, decode_recorded_event,
};
use aether_bloomery::testing::surface_overlap_event;

const PRE_PROPOSE_DECISIONS: &[u8] = include_bytes!("fixtures/pre-propose-decisions.bin");
const PRE_PROPOSE_EVENT: &[u8] = include_bytes!("fixtures/pre-propose-event.bin");

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
