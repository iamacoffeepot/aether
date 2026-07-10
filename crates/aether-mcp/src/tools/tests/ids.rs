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
    assert_eq!(parse_kind_id("12345").expect("decimal parses").0, 12345, "raw decimal u64 parses",);
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
    assert_eq!(static_kind_name(KindId(0xDEAD_BEEF_DEAD_BEEF)), None, "an unknown id has no static name",);
}
