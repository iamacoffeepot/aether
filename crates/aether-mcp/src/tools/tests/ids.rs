#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;

/// iamacoffeepot/aether#1128: `actor_cost`'s `kind_id` filter
/// accepts a tagged `knd-…` id and a raw decimal, and rejects
/// gibberish.
#[test]
fn parse_kind_id_accepts_tagged_and_decimal() {
    let tagged = tagged_id::encode(with_tag(Tag::Kind, 42)).expect("encodes a kind id");
    assert!(parse_kind_id(&tagged).is_ok(), "tagged knd- id parses");
    assert_eq!(parse_kind_id("12345").expect("decimal parses").0, 12345, "raw decimal u64 parses");
    assert!(parse_kind_id("not-an-id").is_err(), "gibberish rejected");
}

/// iamacoffeepot/aether#1128: `static_kind_name` resolves a known
/// substrate kind's id back to its name and misses on a stranger.
#[test]
fn static_kind_name_resolves_known_substrate_kind() {
    let log_tail = KindId(<aether_kinds::LogTail as Kind>::ID.0);
    assert_eq!(
        static_kind_name(log_tail).as_deref(),
        Some(aether_kinds::LogTail::NAME),
        "a substrate kind resolves to its name",
    );
    assert_eq!(static_kind_name(KindId(0xDEAD_BEEF_DEAD_BEEF)), None, "an unknown id has no static name");
}

fn compact_mail_id(sender: &str, correlation_id: u64) -> MailIdJson {
    MailIdJson { sender: sender.to_owned(), correlation_id }
}

fn compact_node(correlation_id: u64, parent: Option<u64>) -> MailNodeJson {
    MailNodeJson {
        mail_id: compact_mail_id("aether.sender", correlation_id),
        parent: parent.map(|id| compact_mail_id("aether.sender", id)),
        sender: "aether.sender".to_owned(),
        recipient: format!("aether.recipient.{correlation_id}"),
        kind: format!("aether.test.kind.{correlation_id}"),
        t_construct_start: 500,
        t_sent: 1_000,
        t_received: Some(2_000),
        t_finished: Some(5_000),
        thread_name: Some("aether-worker-0".to_owned()),
    }
}

#[test]
fn compact_tree_preserves_root_child_indentation_and_sibling_order() {
    let nodes = vec![compact_node(1, None), compact_node(2, Some(1)), compact_node(3, Some(1))];

    assert_eq!(
        render_compact_tree(&nodes),
        [
            "aether.sender → aether.recipient.1  aether.test.kind.1  +3µs",
            "  aether.sender → aether.recipient.2  aether.test.kind.2  +3µs",
            "  aether.sender → aether.recipient.3  aether.test.kind.3  +3µs",
        ]
    );
}

#[test]
fn compact_tree_marks_in_flight_nodes() {
    let mut node = compact_node(1, None);
    node.t_finished = None;

    assert_eq!(render_compact_tree(&[node]), ["aether.sender → aether.recipient.1  aether.test.kind.1  in-flight"]);
}

#[test]
fn compact_tree_saturates_reversed_handler_timestamps() {
    let mut node = compact_node(1, None);
    node.t_received = Some(9_000);
    node.t_finished = Some(1_000);

    assert_eq!(render_compact_tree(&[node]), ["aether.sender → aether.recipient.1  aether.test.kind.1  +0µs"]);
}

#[test]
fn compact_tree_walks_deep_chains_iteratively() {
    const DEPTH: usize = 10_000;
    let nodes: Vec<_> = (0..DEPTH)
        .map(|index| {
            let correlation_id = u64::try_from(index).expect("test depth fits in u64");
            compact_node(correlation_id, correlation_id.checked_sub(1))
        })
        .collect();

    let lines = render_compact_tree(&nodes);
    assert_eq!(lines.len(), DEPTH);
    assert!(lines.last().is_some_and(|line| line.contains("aether.recipient.9999")));
}

#[test]
fn compact_tree_emits_malformed_cycles_and_orphans_once() {
    let nodes = vec![compact_node(1, Some(2)), compact_node(2, Some(1)), compact_node(3, Some(99))];

    let lines = render_compact_tree(&nodes);
    assert_eq!(lines.len(), nodes.len());
    for correlation_id in 1..=3 {
        assert_eq!(
            lines.iter().filter(|line| line.contains(&format!("aether.recipient.{correlation_id}  "))).count(),
            1,
            "node {correlation_id} should be emitted once"
        );
    }
}
