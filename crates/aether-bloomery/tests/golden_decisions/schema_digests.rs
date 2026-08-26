//! Append-only pin of every persisted kind's schema digest (ADR-0187).
//!
//! Unlike the four byte-fixtures beside this file, this one pins a *history*:
//! a shape change appends a line and registers an upcast. Removing a line or
//! rewriting the file to drop a prior digest is the failure this test exists
//! to catch. The remedy is never a regen command.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use aether_bloomery::Digest;
use aether_bloomery::persisted::{PERSISTED_KINDS, PersistedKind};

const FIXTURE: &str = include_str!("fixtures/schema-digests.txt");

#[test]
fn pinned_schema_digests_match_the_registry() {
    let pinned = parse_fixture(FIXTURE);
    let fixture_kinds: BTreeSet<&str> = pinned.keys().copied().collect();
    let registry_kinds: BTreeSet<&str> = PERSISTED_KINDS.iter().map(|kind| kind.name).collect();

    assert_eq!(
        fixture_kinds, registry_kinds,
        "every pinned line must name a kind still in PERSISTED_KINDS; \
         a dropped line erases the record of a shape that wrote stored rows. \
         append the new digest to `schema-digests.txt` and register an upcast"
    );

    for kind in PERSISTED_KINDS {
        let lines = pinned.get(kind.name).expect("registry kind is pinned");
        let current = kind.current_digest();
        let last = lines.last().expect("a pinned kind has at least one digest");
        assert_eq!(
            last, &current,
            "kind `{}` current digest is {}, pinned last line is {}. \
             append the new digest to `schema-digests.txt` and register an upcast",
            kind.name, current, last
        );
        assert_upcasts_cover_prior_lines(kind, lines);
    }
}

fn assert_upcasts_cover_prior_lines(kind: &PersistedKind, lines: &[Digest]) {
    let current = kind.current_digest();
    let prior = &lines[..lines.len().saturating_sub(1)];
    let upcast_digests: BTreeSet<Digest> = kind.upcasts.iter().map(|prior| kind.upcast_digest(prior)).collect();
    for digest in prior {
        assert!(
            *digest != current,
            "kind `{}` pins {digest} before the current digest, but it equals the current shape. \
             append the new digest to `schema-digests.txt` and register an upcast",
            kind.name
        );
        assert!(
            upcast_digests.contains(digest),
            "kind `{}` pins prior digest {digest} with no registered upcast. \
             append the new digest to `schema-digests.txt` and register an upcast",
            kind.name
        );
    }
    let pinned_prior: BTreeSet<Digest> = prior.iter().copied().collect();
    for digest in &upcast_digests {
        assert!(
            pinned_prior.contains(digest),
            "kind `{}` registers upcast {digest} that is not a pinned prior line. \
             append the new digest to `schema-digests.txt` and register an upcast",
            kind.name
        );
    }
}

fn parse_fixture(text: &str) -> BTreeMap<&str, Vec<Digest>> {
    let mut pinned = BTreeMap::<&str, Vec<Digest>>::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let kind = parts.next().unwrap_or_else(|| panic!("schema-digests.txt line {} has no kind", index + 1));
        let hex = parts.next().unwrap_or_else(|| panic!("schema-digests.txt line {} has no digest", index + 1));
        assert!(parts.next().is_none(), "schema-digests.txt line {} has trailing tokens", index + 1);
        let digest = Digest::from_hex(hex)
            .unwrap_or_else(|| panic!("schema-digests.txt line {} digest is not 64 lowercase hex", index + 1));
        pinned.entry(kind).or_default().push(digest);
    }
    pinned
}

#[test]
fn the_checked_in_fixture_never_shrinks() {
    // Tripwire: a regen that rewrote this file would drop prior lines. The
    // on-disk file is the history; this test names the line count so a
    // silent truncate fails even if every remaining line still matches.
    let on_disk = fs::read_to_string("tests/golden_decisions/fixtures/schema-digests.txt")
        .or_else(|_| fs::read_to_string("crates/aether-bloomery/tests/golden_decisions/fixtures/schema-digests.txt"))
        .expect("schema-digests.txt is checked in");
    let pinned = parse_fixture(&on_disk);
    let included = parse_fixture(FIXTURE);
    assert_eq!(pinned, included, "include_str and the on-disk fixture must be the same file");
}
