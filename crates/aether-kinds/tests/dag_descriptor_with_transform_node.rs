//! ADR-0047 §10 / ADR-0048 forward-compat: a descriptor including a
//! `Transform { transform_id, .. }` variant encodes and decodes
//! cleanly even though Phase 2 won't dispatch it.

use aether_data::{Kind, KindId, MailboxId, TransformId};
use aether_kinds::dag::{DagDescriptor, Edge, Node, NodeId, Submit};

#[test]
fn descriptor_with_transform_node_roundtrips() {
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Source {
                id: NodeId(0),
                mailbox: MailboxId(0x1000),
                kind_id: KindId(0x2000),
                payload: vec![9],
            },
            Node::Transform {
                id: NodeId(1),
                transform_id: TransformId(0x5000),
                output_kind_id: KindId(0x6000),
            },
            Node::Observer {
                id: NodeId(2),
                recipient: MailboxId(0x7000),
                kind_id: KindId(0x8000),
            },
        ],
        edges: vec![
            Edge {
                from: NodeId(0),
                to: NodeId(1),
                slot: 0,
            },
            Edge {
                from: NodeId(1),
                to: NodeId(2),
                slot: 0,
            },
        ],
    };

    let submit = Submit {
        descriptor: descriptor.clone(),
    };
    let bytes = submit.encode_into_bytes();
    let decoded =
        Submit::decode_from_bytes(&bytes).expect("Submit with Transform node decodes cleanly");
    assert_eq!(decoded.descriptor, descriptor);
}
