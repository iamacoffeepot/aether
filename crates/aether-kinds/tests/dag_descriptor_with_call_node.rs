//! ADR-0047 rev 2026-05-20: a descriptor including a `Call { recipient,
//! kind_id }` variant feeding an `Observer` encodes and decodes cleanly.
//! The `Call` variant carries no `output_kind_id` — its output is a
//! self-describing `Bundle`.

use aether_data::{Kind, KindId, MailboxId};
use aether_kinds::dag::{DagDescriptor, Edge, Node, NodeId, Submit};

#[test]
fn descriptor_with_call_node_roundtrips() {
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Source {
                id: NodeId(0),
                mailbox: MailboxId(0x1000),
                kind_id: KindId(0x2000),
                payload: vec![],
            },
            Node::Call {
                id: NodeId(1),
                recipient: MailboxId(0x9000),
                kind_id: KindId(0xA000),
            },
            Node::Observer {
                id: NodeId(2),
                recipient: MailboxId(0xB000),
                kind_id: KindId(0xC000),
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
    let decoded = Submit::decode_from_bytes(&bytes).expect("Submit with Call node decodes cleanly");
    assert_eq!(decoded.descriptor, descriptor);

    // The Call variant declares no output kind — assert its shape so a
    // future addition of an `output_kind_id` field would break here.
    let Node::Call {
        recipient, kind_id, ..
    } = &decoded.descriptor.nodes[1]
    else {
        panic!("expected a Call node at index 1");
    };
    assert_eq!(*recipient, MailboxId(0x9000));
    assert_eq!(*kind_id, KindId(0xA000));
}
