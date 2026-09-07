//! Append-only pin of every persisted kind's schema digest (ADR-0187).
//!
//! Unlike the four byte-fixtures beside this file, this one pins a *history*:
//! a shape change appends a line and registers an upcast. Removing a line or
//! rewriting the file to drop a prior digest is the failure this test exists
//! to catch. The remedy is never a regen command.
//!
//! The first 10 lines are pinned independently of the live fixture: the raw
//! sha256 of that prefix at `449d0f894c533a6a354270544becd8efb18a3753`. Later
//! history may append, and later checkpoints may be added beside this pin;
//! existing pins must not be updated to bless rewritten history.

use std::collections::{BTreeMap, BTreeSet};

use aether_bloomery::Digest;
use aether_bloomery::persisted::{PERSISTED_KINDS, PersistedKind};

const FIXTURE: &str = include_str!("fixtures/schema-digests.txt");

/// How many leading ledger lines [`BASELINE_PREFIX_SHA256`] attests.
const BASELINE_LINE_COUNT: usize = 10;

/// Raw sha256 of the first [`BASELINE_LINE_COUNT`] lines of `schema-digests.txt`
/// (including the trailing newline) at `449d0f894c533a6a354270544becd8efb18a3753`.
///
/// Do not update this pin to bless rewritten history.
const BASELINE_PREFIX_SHA256: Digest =
    Digest::pinned("f5f2be01f6bfa39e41ffb51480f994fa0dd61e639a9bddec3867e36ca2ace86f");

const DECOY_LINE: &str = "decisions 0000000000000000000000000000000000000000000000000000000000000000";

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
    let upcast_digests: BTreeSet<Digest> = kind.upcasts.iter().map(|prior| prior.digest).collect();
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
fn the_checked_in_fixture_preserves_baseline_history() {
    // Tripwire: include_str versus the on-disk path of the same file compared
    // equal, so a committed truncation passed. The candidate is the live
    // fixture text alone, judged against the independent prefix pin.
    validate_baseline_history(FIXTURE).unwrap_or_else(|reason| panic!("{reason}"));
}

#[test]
fn appended_history_is_accepted() {
    let extended = format!("{FIXTURE}{DECOY_LINE}\n");
    validate_baseline_history(&extended)
        .unwrap_or_else(|reason| panic!("appending a line must leave the baseline prefix intact: {reason}"));
}

#[test]
fn committed_style_truncation_is_rejected() {
    // Tripwire: a regen that rewrote this file to fewer lines used to pass
    // because the truncated text was compared with itself. One candidate only.
    let truncated = prefix_of_lines(FIXTURE, BASELINE_LINE_COUNT - 1)
        .expect("the live fixture still has the baseline lines to truncate");
    assert!(
        validate_baseline_history(truncated).is_err(),
        "a truncated ledger must fail against the independent baseline"
    );
}

#[test]
fn removing_a_baseline_line_is_rejected() {
    let mut lines = baseline_lines();
    lines.remove(3);
    lines.push(DECOY_LINE);
    assert!(
        validate_baseline_history(&ledger_from_lines(&lines)).is_err(),
        "dropping a historical line must fail even when a decoy keeps the line count"
    );
}

#[test]
fn altering_a_baseline_line_is_rejected() {
    let prefix = prefix_of_lines(FIXTURE, BASELINE_LINE_COUNT).expect("the live fixture contains the baseline prefix");
    let mut bytes = prefix.as_bytes().to_vec();
    // Flip the first nibble of the first digest (`decisions ` is 10 bytes).
    bytes[10] ^= 1;
    let altered = String::from_utf8(bytes).expect("xor of an ascii hex digit stays utf-8");
    assert!(
        validate_baseline_history(&altered).is_err(),
        "altering a historical digest must fail against the independent baseline"
    );
}

#[test]
fn reordering_baseline_history_is_rejected() {
    let mut lines = baseline_lines();
    lines.swap(0, 1);
    assert!(
        validate_baseline_history(&ledger_from_lines(&lines)).is_err(),
        "reordering historical lines must fail against the independent baseline"
    );
}

fn validate_baseline_history(candidate: &str) -> Result<(), String> {
    let prefix = prefix_of_lines(candidate, BASELINE_LINE_COUNT).ok_or_else(|| {
        format!(
            "schema-digests.txt has fewer than {BASELINE_LINE_COUNT} lines; \
             truncating the ledger erases the record of a shape that wrote stored rows"
        )
    })?;
    let digest = Digest::of_wire_bytes(prefix.as_bytes());
    if digest == BASELINE_PREFIX_SHA256 {
        Ok(())
    } else {
        Err(format!(
            "schema-digests.txt baseline prefix sha256 is {digest}, expected {BASELINE_PREFIX_SHA256}. \
             the first {BASELINE_LINE_COUNT} lines are recorded history and must not be removed, \
             altered, or reordered; append only. do not update the pin to bless rewritten history"
        ))
    }
}

fn prefix_of_lines(candidate: &str, line_count: usize) -> Option<&str> {
    let mut newlines = 0;
    for (index, byte) in candidate.as_bytes().iter().copied().enumerate() {
        if byte == b'\n' {
            newlines += 1;
            if newlines == line_count {
                return Some(&candidate[..=index]);
            }
        }
    }
    None
}

fn baseline_lines() -> Vec<&'static str> {
    prefix_of_lines(FIXTURE, BASELINE_LINE_COUNT)
        .expect("the live fixture contains the baseline prefix")
        .lines()
        .collect()
}

fn ledger_from_lines(lines: &[&str]) -> String {
    let mut body = lines.join("\n");
    body.push('\n');
    body
}
