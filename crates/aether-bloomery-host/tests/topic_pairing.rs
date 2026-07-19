//! Producer/consumer pairing tripwire for the reducer's outbox topics.
//!
//! Every effectful `Decision` the reducer emits projects onto a
//! [`Topic`](aether_bloomery::Topic) (enumerated by `Topic::ALL`), and each
//! topic must be drained by exactly one host driver — otherwise its enqueued
//! rows accumulate in the store's outbox forever, undelivered and silent. That
//! is not hypothetical: `Topic::REDISPATCH` is a live orphan (#3664), the exact
//! failure the shared string consts never checked. This test collects every
//! driver's declared drain set and asserts the 1:1 pairing against `Topic::ALL`,
//! with the still-orphaned topics named in an explicit exception set.

use aether_bloomery::Topic;
use aether_bloomery_host::bloomery::{
    ExecutorDriverCapability, IntegrateDriverCapability, LandDriverCapability, MirrorDriverCapability,
};

/// The reducer topics that still have no draining host driver, each with the
/// issue tracking the missing consumer. A topic listed here must have *zero*
/// drainers; the moment one gains a consumer this list is wrong and the test
/// below fails, forcing the entry's removal — so the exception can never outlive
/// the orphan it documents.
const KNOWN_ORPHANS: &[Topic] = &[
    // #3664 — the parked-question redispatch the reducer enqueues from an
    // adopted answer (ADR-0151), with no host consumer yet.
    Topic::REDISPATCH,
];

/// Every reducer [`Topic`] pairs with exactly one draining host driver — except
/// the still-orphaned ones, which pair with none.
#[test]
fn every_reducer_topic_pairs_with_exactly_one_drainer() {
    // Tripwire: the producer/consumer pairing between the reducer's outbox
    // topics (`Topic::ALL`, minted from the effectful `Decision` variants by
    // `Topic::of_decision`) and the host drivers that drain them. A new
    // effectful decision reaches `ALL` through its `of_decision` arm; if no
    // driver declares it below, its rows would enqueue and never drain — the
    // #3664 orphan bug class — and this fails naming the unpaired topic. A
    // second drainer (double-processing) fails the same way.
    let drained: Vec<Topic> = [
        ExecutorDriverCapability::DRAINED_TOPICS,
        IntegrateDriverCapability::DRAINED_TOPICS,
        LandDriverCapability::DRAINED_TOPICS,
        MirrorDriverCapability::DRAINED_TOPICS,
    ]
    .concat();

    for topic in Topic::ALL {
        let drainers = drained.iter().filter(|declared| **declared == *topic).count();
        if KNOWN_ORPHANS.contains(topic) {
            assert_eq!(
                drainers,
                0,
                "{} is listed as a known orphan but now has {drainers} drainer(s) — remove it from KNOWN_ORPHANS",
                topic.as_str()
            );
        } else {
            assert_eq!(drainers, 1, "{} must be drained by exactly one host driver, found {drainers}", topic.as_str());
        }
    }

    // No driver declares a drain for a topic outside the reducer vocabulary: a
    // host-local topic (the mirror's `view_document`) is intentionally not a
    // `Topic` and must never leak into a driver's declared reducer-topic set.
    for declared in &drained {
        assert!(
            Topic::ALL.contains(declared),
            "{} is declared drained but is not a reducer Topic in Topic::ALL",
            declared.as_str()
        );
    }
}
