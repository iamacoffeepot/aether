//! ADR-0048 §4 content-addressed handle-id derivation tests
//! (iamacoffeepot/aether#982).
//!
//! Exercises `content_addressed_handle_id`: determinism, slot-order
//! sensitivity, input-count sensitivity, transform-id sensitivity,
//! cross-domain disjointness, and a 10k-tuple collision smoke test.

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;

use aether_data::tagged_id::tag_of;
use aether_data::{HandleId, KindId, Tag, TransformId, content_addressed_handle_id, with_tag};

/// A `TransformId` tagged in the transform space.
fn tx(body: u64) -> TransformId {
    TransformId(with_tag(Tag::Transform, body))
}

/// A `HandleId` tagged in the handle space.
fn hid(body: u64) -> HandleId {
    HandleId(with_tag(Tag::Handle, body))
}

/// Same `(transform_id, inputs)` in the same order produces the same id
/// across 1000 iterations.
#[test]
fn content_addressed_handles_deterministic() {
    let transform = tx(0x1111);
    let inputs = [hid(0xaaaa), hid(0xbbbb), hid(0xcccc)];
    let first = content_addressed_handle_id(transform, &inputs);
    for _ in 0..1000 {
        assert_eq!(content_addressed_handle_id(transform, &inputs), first);
    }
}

/// `compose(a, b)` and `compose(b, a)` produce different ids — the slot
/// byte before each input handle id makes the order load-bearing.
#[test]
fn content_addressed_handles_slot_order_matters() {
    let transform = tx(0x2222);
    let a = hid(0x1234);
    let b = hid(0x5678);
    let ab = content_addressed_handle_id(transform, &[a, b]);
    let ba = content_addressed_handle_id(transform, &[b, a]);
    assert_ne!(ab, ba);
}

/// `foo(a)` and `foo(a, a)` produce different ids — the `input_count`
/// byte distinguishes arities.
#[test]
fn content_addressed_handles_input_count_matters() {
    let transform = tx(0x3333);
    let a = hid(0x9999);
    let one = content_addressed_handle_id(transform, &[a]);
    let two = content_addressed_handle_id(transform, &[a, a]);
    assert_ne!(one, two);
}

/// Same inputs, different `transform_id` produce different ids.
#[test]
fn content_addressed_handles_transform_id_matters() {
    let inputs = [hid(0xdead), hid(0xbeef)];
    let one = content_addressed_handle_id(tx(0x4444), &inputs);
    let two = content_addressed_handle_id(tx(0x5555), &inputs);
    assert_ne!(one, two);
}

/// A fabricated value treated as a `KindId` vs as a `TransformId` /
/// `HandleId` produces a different content-address id — the
/// `HANDLE_DOMAIN` prefix separates the spaces. Cross-domain collision
/// sanity check.
#[test]
fn content_addressed_handles_disjoint_domain() {
    // Same raw 60-bit body interpreted as a transform id vs as a
    // (mis-tagged) kind id. The handle-domain salt + the tag bits both
    // contribute, so the derived ids differ.
    let body = 0x0fed_cba9_8765_4321 & 0x0fff_ffff_ffff_ffff;
    let as_transform = tx(body);
    // A KindId carrying the same body bits but the kind tag. Feeding its
    // raw u64 through the transform slot of the derivation must not
    // collide with the transform-tagged derivation.
    let as_kind = KindId(with_tag(Tag::Kind, body));
    let via_transform = content_addressed_handle_id(as_transform, &[hid(0x1)]);
    let via_kind_bits = content_addressed_handle_id(TransformId(as_kind.0), &[hid(0x1)]);
    assert_ne!(
        via_transform, via_kind_bits,
        "transform-tagged vs kind-tagged input bits must derive distinct handle ids",
    );
    // And the derived id is itself tagged in the handle space.
    assert_eq!(tag_of(via_transform.0), Some(Tag::Handle));
}

/// 10k random `(transform_id, inputs)` tuples — no id collides. 10k vs
/// 64-bit (60-bit body) is well under the birthday bound; a collision
/// would indicate a derivation bug. Uses a deterministic xorshift PRNG
/// so the test is reproducible without a `rand` dep.
#[test]
fn content_addressed_handles_collision_resistance_smoke() {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let mut ids: HashSet<HandleId> = HashSet::with_capacity(10_000);
    for _ in 0..10_000 {
        let transform = tx(next());
        let arity = (next() % 5) as usize; // 0..=4 inputs
        let inputs: Vec<HandleId> = (0..arity).map(|_| hid(next())).collect();
        let id = content_addressed_handle_id(transform, &inputs);
        assert!(
            ids.insert(id),
            "content-address collision in 10k smoke test"
        );
    }
    assert_eq!(ids.len(), 10_000);
}
