//! ADR-0047 §3: every `DagError` variant postcard-roundtrips with
//! structural equality. Catches accidental wire breakage when future
//! variants are appended (postcard discriminants are positional).

use aether_data::{KindId, TransformId};
use aether_kinds::dag::{DagError, NodeId};

fn roundtrip(error: &DagError) {
    let bytes = postcard::to_allocvec(error).expect("DagError encodes via postcard");
    let decoded: DagError =
        postcard::from_bytes(&bytes).expect("DagError decodes from its own bytes");
    assert_eq!(&decoded, error);
}

#[test]
fn every_dag_error_variant_roundtrips() {
    roundtrip(&DagError::DuplicateNodeId(NodeId(3)));
    roundtrip(&DagError::UnknownNodeId(NodeId(99)));
    roundtrip(&DagError::Cycle(vec![NodeId(0), NodeId(1), NodeId(2)]));
    roundtrip(&DagError::SourceWithIncomingEdge(NodeId(0)));
    roundtrip(&DagError::ObserverWithOutgoingEdge(NodeId(4)));
    roundtrip(&DagError::UnknownSink("aether.bogus".into()));
    roundtrip(&DagError::UnknownRecipient("aether.gone".into()));
    roundtrip(&DagError::KindNotAccepted {
        node: NodeId(2),
        kind_id: KindId(0x1234),
        mailbox_or_recipient: "aether.fs".into(),
    });
    roundtrip(&DagError::UnknownTransform {
        node: NodeId(1),
        transform_id: TransformId(0x5555),
    });
    roundtrip(&DagError::TransformOutputMismatch {
        node: NodeId(1),
        declared: KindId(0xAAAA),
        manifest: KindId(0xBBBB),
    });
    roundtrip(&DagError::EdgeTypeMismatch {
        edge_index: 7,
        expected_kind: KindId(0xCCCC),
        got_kind: KindId(0xDDDD),
    });
    roundtrip(&DagError::TooLarge {
        reason: "256 nodes exceeds cap".into(),
    });
}
