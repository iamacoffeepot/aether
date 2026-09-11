//! Frozen queued view rows from before the coordination field was added.

use aether_bloomery::{BloomStatus, Digest, ViewDocument, decode_row, encode_row};
use aether_data::Kind;

#[test]
fn previous_views_preserve_multiple_blooms_and_precheck_state() {
    let stored = include_bytes!("fixtures/pre-coordination-view-storage.bin");
    let positional = include_bytes!("fixtures/pre-coordination-view-positional.bin");
    for (bytes, schema) in [(stored.as_slice(), Some(ViewDocument::NAME)), (positional.as_slice(), None)] {
        assert!(decode_row::<ViewDocument>(bytes, schema).is_err(), "this row needs its previous-shape decoder");
        let view = ViewDocument::decode_row(bytes, schema).expect("previous view upcasts");
        assert_eq!(view.blooms.len(), 2);
        assert!(view.blooms.iter().all(|bloom| bloom.coordination.is_none()));
        assert!(view.blooms.iter().all(|bloom| bloom.status == BloomStatus::Sealed));
        assert_eq!(view.blooms[1].id.0, Digest::from_bytes([45; 32]));
        assert!(view.blooms[1].precheck.is_none());
        let node = view.blooms[0]
            .precheck
            .as_ref()
            .and_then(|state| state.prepared.as_ref())
            .expect("prepared precheck retained");
        assert_eq!(node.tree, Digest::from_bytes([42; 32]));
        assert_eq!(node.head, Digest::from_bytes([43; 32]));
        for bloom in &view.blooms {
            assert_eq!(
                bloom.members.iter().map(|member| member.workpiece.0.as_str()).collect::<Vec<_>>(),
                ["alpha", "beta"]
            );
        }
        let current = encode_row(&view, schema).expect("upcast view encodes");
        assert_eq!(ViewDocument::decode_row(&current, schema).expect("current view decodes"), view);
        assert!(ViewDocument::decode_row(&bytes[..bytes.len() / 2], schema).is_err());
    }
}
