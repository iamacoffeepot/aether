//! Framing tripwire: pinned bytes identical to the journal's original.

use aether_bloomery_kinds::artifact_digest;
use aether_data::KindId;

const TRIPWIRE_KIND: KindId = KindId(0x0123_4567_89ab_cdef);
const TRIPWIRE_PAYLOAD: &[u8] = b"aether-bloomery-journal";
const TRIPWIRE_DIGEST: [u8; 32] = [
    0x77, 0x86, 0x71, 0x92, 0xab, 0x73, 0xa1, 0xc9, 0xcb, 0x5d, 0x12, 0xef, 0x11, 0xd4, 0x05, 0x58, 0xf3, 0x9e, 0xfd,
    0xec, 0x9c, 0x6f, 0xe2, 0x2f, 0x51, 0xc8, 0x4c, 0x95, 0x2e, 0xe1, 0xcd, 0x17,
];

#[test]
fn the_digest_of_one_fixed_artifact_is_pinned() {
    // Tripwire: prefix byte order, prefix-then-payload order, and the hash
    // preimage. The digest is sha256(kind_id_le_8 || payload). Drifts the
    // moment any of those change — and every stored digest in every journal
    // would move with it. Catches a move that changed the preimage.
    assert_eq!(artifact_digest(TRIPWIRE_KIND, TRIPWIRE_PAYLOAD).as_bytes(), &TRIPWIRE_DIGEST);
}
