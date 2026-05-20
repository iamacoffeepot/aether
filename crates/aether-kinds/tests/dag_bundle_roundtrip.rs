//! ADR-0047 §2 rev 2026-05-20: a `Bundle` with two heterogeneous
//! `(KindId, payload)` elements encodes through the canonical `Kind`
//! path and decodes back equal.

use aether_data::{Kind, KindId};
use aether_kinds::dag::{Bundle, BundleElement};

#[test]
fn bundle_with_heterogeneous_elements_roundtrips() {
    let bundle = Bundle {
        elements: vec![
            BundleElement {
                kind_id: KindId(0x1111),
                payload: vec![0xDE, 0xAD],
            },
            BundleElement {
                kind_id: KindId(0x2222),
                payload: vec![0xBE, 0xEF, 0xCA, 0xFE],
            },
        ],
    };

    let bytes = bundle.encode_into_bytes();
    let decoded =
        Bundle::decode_from_bytes(&bytes).expect("Bundle decodes from its own canonical bytes");
    assert_eq!(decoded, bundle);
    assert_eq!(decoded.elements.len(), 2);
    assert_eq!(decoded.elements[0].kind_id, KindId(0x1111));
    assert_eq!(decoded.elements[1].payload, vec![0xBE, 0xEF, 0xCA, 0xFE]);
}

#[test]
fn empty_bundle_roundtrips() {
    let bundle = Bundle { elements: vec![] };
    let bytes = bundle.encode_into_bytes();
    let decoded = Bundle::decode_from_bytes(&bytes).expect("empty Bundle decodes");
    assert_eq!(decoded, bundle);
    assert!(decoded.elements.is_empty());
}
