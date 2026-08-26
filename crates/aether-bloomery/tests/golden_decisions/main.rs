//! Pinned wire bytes for a representative [`aether_bloomery::Decisions`] value
//! (ADR-0187 / #4944).
//!
//! The journal's decisions column is a persisted surface. A shape change in the
//! reachable graph — adding a field to [`aether_bloomery::StageProgress`],
//! reordering a [`aether_bloomery::Decision`] variant — must fail this fixture
//! rather than wait for the next boot replay to abort.
//!
//! Coverage is the whole of the fixture's value: a family the representative
//! omits is a family whose payload type can gain a field, encode wider on every
//! new row, and still pass here — while boot replay of the rows written before
//! it fatally aborts. So every effect family appears below, with each payload's
//! `Option` fields populated rather than left `None`, since a `None` encodes
//! its tag and nothing of the shape behind it, and each collection non-empty,
//! since an empty one encodes its length and nothing of its element.
//!
//! That coverage is no longer maintained by eye: the [`completeness`] sibling
//! derives the set of positions reachable from [`aether_bloomery::Decision`]
//! out of the schema and fails naming any this value does not reach.
//!
//! Two surfaces sit outside that walk, and whatever they freeze here they freeze
//! by hand. `Decisions::outcome` is one value, not a sequence, so no single
//! fixture could enumerate it. And [`aether_bloomery::Fact`] is not under
//! [`aether_bloomery::Decision`] at all: it is the journal's *event* column, a
//! second wire-frozen surface the completeness walk never enters.
//!
//! The bytes live under `fixtures/`. These tests only compare; rewriting a file
//! is `cargo xtask fixtures regen <name>`.

mod completeness;
mod schema_digests;

use aether_bloomery::testing::{
    containment_refused_event, representative, surface_overlap_decisions, surface_overlap_event,
};
use aether_bloomery::{Decisions, Event, Fact};
use aether_data::wire::{from_bytes, to_vec};

const GOLDEN_DECISIONS: &[u8] = include_bytes!("fixtures/decisions.bin");
const GOLDEN_SURFACE_OVERLAP_DECISIONS: &[u8] = include_bytes!("fixtures/surface-overlap-decisions.bin");
const GOLDEN_SURFACE_OVERLAP_EVENT: &[u8] = include_bytes!("fixtures/surface-overlap-event.bin");
const GOLDEN_CONTAINMENT_REFUSED_EVENT: &[u8] = include_bytes!("fixtures/containment-refused-event.bin");

#[test]
fn decisions_wire_bytes_match_pinned_golden() {
    let value = representative();
    let encoded = to_vec(&value).expect("representative decisions encode");
    assert_eq!(
        encoded.as_slice(),
        GOLDEN_DECISIONS,
        "decisions wire drifted; run `cargo xtask fixtures regen decisions`"
    );
    let decoded: Decisions = from_bytes(GOLDEN_DECISIONS).expect("pinned bytes decode against HEAD types");
    assert_eq!(decoded, value);
}

#[test]
fn surface_overlap_outcome_wire_bytes_match_pinned_golden() {
    let value = surface_overlap_decisions();
    let encoded = to_vec(&value).expect("surface-overlap decisions encode");
    assert_eq!(
        encoded.as_slice(),
        GOLDEN_SURFACE_OVERLAP_DECISIONS,
        "surface-overlap outcome wire drifted; run `cargo xtask fixtures regen surface-overlap-decisions`"
    );

    let decoded: Decisions = from_bytes(GOLDEN_SURFACE_OVERLAP_DECISIONS).expect("pinned bytes decode against HEAD");
    assert_eq!(decoded, value);
}

#[test]
fn surface_overlap_event_wire_bytes_match_pinned_golden() {
    let value = surface_overlap_event();
    let encoded = to_vec(&value).expect("surface-overlap event encodes");
    assert_eq!(
        encoded.as_slice(),
        GOLDEN_SURFACE_OVERLAP_EVENT,
        "surface-overlap event wire drifted; run `cargo xtask fixtures regen surface-overlap-event`"
    );

    let decoded: Event = from_bytes(GOLDEN_SURFACE_OVERLAP_EVENT).expect("pinned bytes decode against HEAD");
    assert_eq!(decoded, value);
}

#[test]
fn appending_containment_refused_does_not_shift_surface_overlap_discriminant() {
    // Tripwire: `Fact::ContainmentRefused` is appended past `FoldRefused`, so
    // every prior variant — including this already-pinned `SurfaceOverlap`
    // row — keeps its wire discriminant.
    let decoded: Event = from_bytes(GOLDEN_SURFACE_OVERLAP_EVENT).expect("pinned surface-overlap still decodes");
    assert!(matches!(decoded.fact, Fact::SurfaceOverlap { .. }));
    assert_eq!(decoded, surface_overlap_event());
}

#[test]
fn containment_refused_event_wire_bytes_match_pinned_golden() {
    let value = containment_refused_event();
    let encoded = to_vec(&value).expect("containment-refused event encodes");
    assert_eq!(
        encoded.as_slice(),
        GOLDEN_CONTAINMENT_REFUSED_EVENT,
        "containment-refused event wire drifted; run `cargo xtask fixtures regen containment-refused-event`"
    );

    let decoded: Event = from_bytes(GOLDEN_CONTAINMENT_REFUSED_EVENT).expect("pinned bytes decode against HEAD");
    assert_eq!(decoded, value);
}
