//! ADR-0047 §2 Phase 2 happy path: a 3-node source → observer
//! descriptor with a small inline payload encodes through the
//! canonical `Kind` path and decodes back equal.

use aether_data::{Kind, KindId, MailboxId};
use aether_kinds::dag::{DagDescriptor, Edge, Node, NodeId, Submit};

#[test]
fn source_observer_descriptor_roundtrips() {
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Source {
                id: NodeId(0),
                mailbox: MailboxId(0x1000),
                kind_id: KindId(0x2000),
                payload: vec![1, 2, 3, 4],
            },
            Node::Observer {
                id: NodeId(1),
                recipient: MailboxId(0x3000),
                kind_id: KindId(0x4000),
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };

    let submit = Submit {
        descriptor: descriptor.clone(),
    };
    let bytes = submit.encode_into_bytes();
    let decoded =
        Submit::decode_from_bytes(&bytes).expect("Submit decodes from its own canonical bytes");
    assert_eq!(decoded.descriptor, descriptor);
}
