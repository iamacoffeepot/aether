//! Producer/consumer pairing tripwire for the bloomery outbox topics.
//!
//! Every bloomery outbox topic ([`Topic::ALL`](aether_bloomery::Topic::ALL)) —
//! the reducer-minted ones each effectful `Decision` projects onto, plus the
//! host-minted ones the host both produces and drains — must be drained by
//! exactly one host driver, otherwise its enqueued rows accumulate in the store's
//! outbox forever, undelivered and silent. That is not hypothetical:
//! `Topic::REDISPATCH` is a live orphan (#3664), the exact failure the shared
//! string consts never checked. This test collects every driver's declared drain
//! set and asserts the 1:1 pairing against `Topic::ALL`, with the still-orphaned
//! topics named in an explicit exception set.

use aether_bloomery::Topic;
use aether_bloomery_host::bloomery::{
    ExecutorDriverCapability, IntegrateDriverCapability, LandDriverCapability, MirrorDriverCapability,
};

/// The topics that still have no draining host driver, each with the issue
/// tracking the missing consumer. A topic listed here must have *zero* drainers;
/// the moment one gains a consumer this list is wrong and the test below fails,
/// forcing the entry's removal — so the exception can never outlive the orphan it
/// documents.
const KNOWN_ORPHANS: &[Topic] = &[
    // #3664 — the parked-question redispatch the reducer enqueues from an
    // adopted answer (ADR-0151), with no host consumer yet.
    Topic::REDISPATCH,
];

/// Every bloomery outbox [`Topic`] pairs with exactly one draining host driver —
/// except the still-orphaned ones, which pair with none.
#[test]
fn every_reducer_topic_pairs_with_exactly_one_drainer() {
    // Tripwire: the producer/consumer pairing between the bloomery outbox topics
    // (`Topic::ALL` — reducer-minted via `Topic::of_decision`, plus host-minted)
    // and the host drivers that drain them. A new topic reaches `ALL` through its
    // const (and, when reducer-minted, its `of_decision` arm); if no driver
    // declares it below, its rows would enqueue and never drain — the #3664
    // orphan bug class — and this fails naming the unpaired topic. A second
    // drainer (double-processing) fails the same way.
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

    // Every declared drain is a member of the closed `Topic::ALL` set — a driver
    // cannot declare a topic const that was never added to the enumeration the
    // tripwire walks.
    for declared in &drained {
        assert!(Topic::ALL.contains(declared), "{} is declared drained but is not in Topic::ALL", declared.as_str());
    }
}
