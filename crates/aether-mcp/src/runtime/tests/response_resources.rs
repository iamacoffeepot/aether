//! The ephemeral response store's ceilings, expiry, and addressing.
//!
//! The store's contract is unusual enough to be worth pinning: it refuses a new
//! spill rather than evicting a live one. A store that evicted would hand out an
//! address and invalidate it before its advertised lifetime, and the caller
//! following that resource link would see an intermittent `-32002` with nothing
//! in the response to explain it.

use crate::runtime::response_resources::{ResponseStore, ResponseStoreLimits, StoreRefusal, summarize};

const LIFETIME_SECS: u64 = 600;

fn limits() -> ResponseStoreLimits {
    ResponseStoreLimits {
        maximum_bytes: 1_048_576,
        total_bytes: 67_108_864,
        maximum_entries: 128,
        lifetime_secs: LIFETIME_SECS,
    }
}

/// A stored response is readable at its own address and at no other. The
/// guessed address is the point: an address is the only thing between one
/// caller and another caller's output.
#[test]
fn an_address_reads_back_only_itself() {
    let mut store = ResponseStore::new(limits());

    let first = store.store(b"first output".to_vec(), 0).expect("the first spill fits");
    let second = store.store(b"second output".to_vec(), 0).expect("the second spill fits");

    assert_ne!(first, second, "two spills must not share an address");
    assert_eq!(store.read(&first, 0), Some(&b"first output"[..]));
    assert_eq!(store.read(&second, 0), Some(&b"second output"[..]));
    assert_eq!(
        store.read("aether://mcp/response/00000000000000000000000000000000", 0),
        None,
        "a guessed nonce must read nothing",
    );
}

/// The per-resource ceiling binds on the output alone, before the store's other
/// two ceilings are consulted — an oversized single output is refused even into
/// an empty store.
#[test]
fn the_per_resource_ceiling_binds() {
    let mut store = ResponseStore::new(ResponseStoreLimits { maximum_bytes: 16, ..limits() });

    let refusal = store.store(vec![b'x'; 17], 0).expect_err("17 bytes must not fit a 16-byte ceiling");

    assert!(matches!(refusal, StoreRefusal::ResourceTooLarge { bytes: 17, maximum_bytes: 16 }), "got {refusal:?}");
    assert_eq!(store.len(), 0, "a refused spill must leave the store untouched");
    assert!(store.store(vec![b'x'; 16], 0).is_ok(), "exactly the ceiling still fits");
}

/// The total ceiling refuses the *new* spill and keeps every live one. This is
/// the eviction rule stated as a test: the earlier addresses stay readable.
#[test]
fn the_total_ceiling_refuses_rather_than_evicting() {
    let mut store = ResponseStore::new(ResponseStoreLimits { total_bytes: 24, ..limits() });

    let held = store.store(vec![b'a'; 16], 0).expect("the first spill fits the total budget");
    let refusal = store.store(vec![b'b'; 16], 0).expect_err("the second must not fit");

    assert!(matches!(refusal, StoreRefusal::TotalExhausted { bytes: 16, remaining_bytes: 8 }), "got {refusal:?}");
    assert_eq!(store.resident_bytes(), 16, "a refusal must not change what is resident");
    assert_eq!(store.read(&held, 0), Some(&[b'a'; 16][..]), "the live address must survive the refusal");
}

/// The entry ceiling binds independently of the byte ceilings: 128 tiny outputs
/// still fill the store.
#[test]
fn the_entry_ceiling_binds_independently_of_bytes() {
    let mut store = ResponseStore::new(ResponseStoreLimits { maximum_entries: 2, ..limits() });

    store.store(b"a".to_vec(), 0).expect("first");
    store.store(b"b".to_vec(), 0).expect("second");
    let refusal = store.store(b"c".to_vec(), 0).expect_err("a third entry must not be admitted");

    assert!(matches!(refusal, StoreRefusal::EntriesExhausted { maximum_entries: 2 }), "got {refusal:?}");
}

/// An expired address stops reading and gives its space back, so a store at its
/// ceiling recovers on its own rather than needing a caller to prompt it.
#[test]
fn expiry_stops_reads_and_reclaims_space() {
    let lifetime_millis = LIFETIME_SECS * 1_000;
    let mut store = ResponseStore::new(ResponseStoreLimits { maximum_entries: 1, ..limits() });

    let address = store.store(b"aging output".to_vec(), 0).expect("the first spill fits");
    assert!(store.read(&address, lifetime_millis - 1).is_some(), "the address is live up to its lifetime");

    assert_eq!(store.read(&address, lifetime_millis), None, "the lifetime is inclusive of its own end");
    assert_eq!(store.resident_bytes(), 0, "an expired entry must give its bytes back");
    assert!(store.store(b"replacement".to_vec(), lifetime_millis).is_ok(), "the reclaimed slot is reusable");
}

/// A summary describes shape and stays bounded. It rides in the response the
/// address exists to keep small, so a summary that expanded nested content
/// would defeat the addressing it describes.
#[test]
fn a_summary_reports_shape_without_expanding_content() {
    let value = serde_json::json!({
        "commissions": [1, 2, 3],
        "note": "x".repeat(10_000),
    });

    let summary = summarize(&value);

    assert!(summary.contains("object with 2 keys"), "got {summary}");
    assert!(summary.contains("3 entries"), "a container's size is shape, so it is reported: {summary}");
    assert!(!summary.contains(&"x".repeat(100)), "a summary must not expand a leaf's content");
    assert!(summary.len() <= 2_048, "a summary must stay bounded: {} bytes", summary.len());
}
