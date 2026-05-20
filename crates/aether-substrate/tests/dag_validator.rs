//! ADR-0047 §3 DAG validator phase tests (iamacoffeepot/aether#975).
//!
//! Each test builds a `DagDescriptor`, a routing `Registry` (mailbox
//! existence + consumer kind schemas), and a `CapabilityRegistry`
//! (accept-sets), then asserts the validator returns `Ok(ValidatedDag)`
//! or the first `DagError` the relevant phase produces. The fixtures
//! register consumer kinds via `register_kind_with_descriptor` so the
//! Phase 3 slot-walk resolves a real `Ref<K>` field.

use std::collections::BTreeSet;

use aether_data::{Kind, KindDescriptor, MailboxId, Ref, Schema, TransformId};
use aether_kinds::{Bundle, DagDescriptor, DagError, Edge, Node, NodeId};
use aether_substrate::dag::validator::{self, DEFAULT_MAX_NODES};
use aether_substrate::mail::registry::noop_handler;
use aether_substrate::mail::{CapabilityRegistry, KindId, MailboxCaps, Registry};

use aether_kinds::{ComponentCapabilities, HandlerCapability};

/// A simple source/observer payload kind — no `Ref` fields, registered
/// purely so its id sits in a mailbox accept set.
#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.dag.signal")]
struct Signal {
    seq: u32,
}

/// A consumer kind that declares a single `Ref<Bundle>` input slot —
/// the shape a node downstream of a `Call` must have.
#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.dag.bundle_consumer")]
struct BundleConsumer {
    input: Ref<Bundle>,
}

/// A consumer kind that declares a `Ref<Signal>` input slot — accepts a
/// `Signal`, not a `Bundle`, so an edge out of a `Call` into it is a
/// type mismatch.
#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.dag.signal_consumer")]
struct SignalConsumer {
    input: Ref<Signal>,
}

fn descriptor_of<K: Kind + Schema>() -> KindDescriptor {
    KindDescriptor {
        name: K::NAME.to_owned(),
        schema: K::SCHEMA,
    }
}

/// Register a mailbox by name and return its id. The handler is a no-op;
/// the validator only checks existence, never dispatches.
fn register_mailbox(reg: &Registry, name: &str) -> MailboxId {
    reg.register_inbox(name, noop_handler())
}

/// Register `mailbox` in the capability registry as accepting exactly
/// `accepted` (no fallback).
fn register_caps(caps: &CapabilityRegistry, mailbox: MailboxId, accepted: &[KindId]) {
    register_caps_with_fallback(caps, mailbox, accepted, false);
}

fn register_caps_with_fallback(
    caps: &CapabilityRegistry,
    mailbox: MailboxId,
    accepted: &[KindId],
    fallback: bool,
) {
    let component = ComponentCapabilities {
        handlers: accepted
            .iter()
            .map(|&id| HandlerCapability {
                id,
                name: format!("test.kind.{}", id.0),
                doc: None,
            })
            .collect(),
        fallback: fallback.then_some(aether_kinds::FallbackCapability { doc: None }),
        doc: None,
    };
    caps.register(
        mailbox,
        MailboxCaps::from_component_capabilities(&component),
    );
}

/// A minimal valid DAG: one source feeding one observer over one edge,
/// with both mailboxes registered and accepting their kinds, and the
/// observer declaring a (here unchecked, since the producer is a source)
/// `Ref<Signal>` input.
#[test]
fn validator_accepts_minimal_dag() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();

    let src_mbx = register_mailbox(&reg, "test.source");
    let obs_mbx = register_mailbox(&reg, "test.observer");
    reg.register_kind_with_descriptor(descriptor_of::<Signal>())
        .expect("register Signal");
    reg.register_kind_with_descriptor(descriptor_of::<SignalConsumer>())
        .expect("register SignalConsumer");
    register_caps(&caps, src_mbx, &[Signal::ID]);
    register_caps(&caps, obs_mbx, &[SignalConsumer::ID]);

    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Source {
                id: NodeId(0),
                mailbox: src_mbx,
                kind_id: Signal::ID,
                payload: vec![],
            },
            Node::Observer {
                id: NodeId(1),
                recipient: obs_mbx,
                kind_id: SignalConsumer::ID,
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };

    let validated = validator::validate(&descriptor, &reg, &caps).expect("minimal dag validates");
    assert_eq!(validated.topo_order.len(), 2);
    // Source must precede the observer in topo order.
    let src_pos = validated.topo_order.iter().position(|n| *n == NodeId(0));
    let obs_pos = validated.topo_order.iter().position(|n| *n == NodeId(1));
    assert!(src_pos < obs_pos);
}

#[test]
fn validator_rejects_unsupported_version() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let descriptor = DagDescriptor {
        version: 99,
        nodes: vec![],
        edges: vec![],
    };
    match validator::validate(&descriptor, &reg, &caps) {
        Err(DagError::TooLarge { reason }) => {
            assert!(
                reason.contains("version"),
                "reason should mention version: {reason}"
            );
            assert!(
                reason.contains("99"),
                "reason should mention the bad version: {reason}"
            );
        }
        other => panic!("expected TooLarge version reject, got {other:?}"),
    }
}

#[test]
fn validator_rejects_duplicate_node_id() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let mbx = register_mailbox(&reg, "test.mbx");
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Observer {
                id: NodeId(0),
                recipient: mbx,
                kind_id: Signal::ID,
            },
            Node::Observer {
                id: NodeId(0),
                recipient: mbx,
                kind_id: Signal::ID,
            },
        ],
        edges: vec![],
    };
    assert_eq!(
        validator::validate(&descriptor, &reg, &caps),
        Err(DagError::DuplicateNodeId(NodeId(0)))
    );
}

#[test]
fn validator_rejects_unknown_endpoint() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let mbx = register_mailbox(&reg, "test.mbx");
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![Node::Observer {
            id: NodeId(0),
            recipient: mbx,
            kind_id: Signal::ID,
        }],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(99),
            slot: 0,
        }],
    };
    assert_eq!(
        validator::validate(&descriptor, &reg, &caps),
        Err(DagError::UnknownNodeId(NodeId(99)))
    );
}

#[test]
fn validator_rejects_cycle() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let mbx = register_mailbox(&reg, "test.mbx");
    // Three calls in a loop: 0 -> 1 -> 2 -> 0. Calls are mid-graph so
    // they carry no degree constraint; the cycle is the only violation.
    let make_call = |id| Node::Call {
        id,
        recipient: mbx,
        kind_id: Signal::ID,
    };
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            make_call(NodeId(0)),
            make_call(NodeId(1)),
            make_call(NodeId(2)),
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
            Edge {
                from: NodeId(2),
                to: NodeId(0),
                slot: 0,
            },
        ],
    };
    match validator::validate(&descriptor, &reg, &caps) {
        Err(DagError::Cycle(residual)) => {
            let set: BTreeSet<NodeId> = residual.into_iter().collect();
            assert_eq!(set, [NodeId(0), NodeId(1), NodeId(2)].into_iter().collect());
        }
        other => panic!("expected Cycle, got {other:?}"),
    }
}

#[test]
fn validator_rejects_source_with_incoming_edge() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let mbx = register_mailbox(&reg, "test.mbx");
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Source {
                id: NodeId(0),
                mailbox: mbx,
                kind_id: Signal::ID,
                payload: vec![],
            },
            Node::Source {
                id: NodeId(1),
                mailbox: mbx,
                kind_id: Signal::ID,
                payload: vec![],
            },
        ],
        edges: vec![Edge {
            from: NodeId(1),
            to: NodeId(0),
            slot: 0,
        }],
    };
    assert_eq!(
        validator::validate(&descriptor, &reg, &caps),
        Err(DagError::SourceWithIncomingEdge(NodeId(0)))
    );
}

#[test]
fn validator_rejects_observer_with_outgoing_edge() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let mbx = register_mailbox(&reg, "test.mbx");
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Observer {
                id: NodeId(0),
                recipient: mbx,
                kind_id: Signal::ID,
            },
            Node::Observer {
                id: NodeId(1),
                recipient: mbx,
                kind_id: Signal::ID,
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };
    assert_eq!(
        validator::validate(&descriptor, &reg, &caps),
        Err(DagError::ObserverWithOutgoingEdge(NodeId(0)))
    );
}

#[test]
fn validator_rejects_unknown_sink() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    // Source mailbox id never registered.
    let ghost = MailboxId::from_name("test.ghost.source");
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![Node::Source {
            id: NodeId(0),
            mailbox: ghost,
            kind_id: Signal::ID,
            payload: vec![],
        }],
        edges: vec![],
    };
    match validator::validate(&descriptor, &reg, &caps) {
        Err(DagError::UnknownSink(_)) => {}
        other => panic!("expected UnknownSink, got {other:?}"),
    }
}

#[test]
fn validator_rejects_unknown_recipient() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let ghost = MailboxId::from_name("test.ghost.observer");
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![Node::Observer {
            id: NodeId(0),
            recipient: ghost,
            kind_id: Signal::ID,
        }],
        edges: vec![],
    };
    match validator::validate(&descriptor, &reg, &caps) {
        Err(DagError::UnknownRecipient(_)) => {}
        other => panic!("expected UnknownRecipient, got {other:?}"),
    }
}

#[test]
fn validator_rejects_kind_not_accepted() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let src_mbx = register_mailbox(&reg, "test.source");
    // Mailbox exists but accepts a different kind.
    register_caps(&caps, src_mbx, &[KindId(0xDEAD)]);
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![Node::Source {
            id: NodeId(0),
            mailbox: src_mbx,
            kind_id: Signal::ID,
            payload: vec![],
        }],
        edges: vec![],
    };
    match validator::validate(&descriptor, &reg, &caps) {
        Err(DagError::KindNotAccepted { node, kind_id, .. }) => {
            assert_eq!(node, NodeId(0));
            assert_eq!(kind_id, Signal::ID);
        }
        other => panic!("expected KindNotAccepted, got {other:?}"),
    }
}

#[test]
fn validator_rejects_observer_kind_not_accepted_no_fallback() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let obs_mbx = register_mailbox(&reg, "test.observer");
    register_caps(&caps, obs_mbx, &[KindId(0xBEEF)]);
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![Node::Observer {
            id: NodeId(0),
            recipient: obs_mbx,
            kind_id: Signal::ID,
        }],
        edges: vec![],
    };
    match validator::validate(&descriptor, &reg, &caps) {
        Err(DagError::KindNotAccepted { node, .. }) => assert_eq!(node, NodeId(0)),
        other => panic!("expected KindNotAccepted, got {other:?}"),
    }
}

#[test]
fn validator_accepts_observer_via_fallback() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let obs_mbx = register_mailbox(&reg, "test.observer");
    // Does not handle Signal, but carries a fallback.
    register_caps_with_fallback(&caps, obs_mbx, &[KindId(0xBEEF)], true);
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![Node::Observer {
            id: NodeId(0),
            recipient: obs_mbx,
            kind_id: Signal::ID,
        }],
        edges: vec![],
    };
    assert!(validator::validate(&descriptor, &reg, &caps).is_ok());
}

#[test]
fn validator_rejects_too_large_nodes() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let mbx = register_mailbox(&reg, "test.mbx");
    let nodes: Vec<Node> = (0..=DEFAULT_MAX_NODES)
        .map(|i| Node::Observer {
            id: NodeId(u32::try_from(i).expect("test node count fits u32")),
            recipient: mbx,
            kind_id: Signal::ID,
        })
        .collect();
    let descriptor = DagDescriptor {
        version: 1,
        nodes,
        edges: vec![],
    };
    match validator::validate(&descriptor, &reg, &caps) {
        Err(DagError::TooLarge { reason }) => {
            assert!(reason.contains("node count"), "reason: {reason}");
        }
        other => panic!("expected TooLarge node-count reject, got {other:?}"),
    }
}

#[test]
fn validator_rejects_transform_node() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![Node::Transform {
            id: NodeId(0),
            transform_id: TransformId(0x1234),
            output_kind_id: Signal::ID,
        }],
        edges: vec![],
    };
    match validator::validate(&descriptor, &reg, &caps) {
        Err(DagError::UnknownTransform { node, transform_id }) => {
            assert_eq!(node, NodeId(0));
            assert_eq!(transform_id, TransformId(0x1234));
        }
        other => panic!("expected UnknownTransform, got {other:?}"),
    }
}

#[test]
fn validator_accepts_call_node() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();

    let src_mbx = register_mailbox(&reg, "test.source");
    let call_mbx = register_mailbox(&reg, "test.call_recipient");
    let obs_mbx = register_mailbox(&reg, "test.observer");
    reg.register_kind_with_descriptor(descriptor_of::<Signal>())
        .expect("register Signal");
    reg.register_kind_with_descriptor(descriptor_of::<BundleConsumer>())
        .expect("register BundleConsumer");
    register_caps(&caps, src_mbx, &[Signal::ID]);
    register_caps(&caps, call_mbx, &[Signal::ID]);
    register_caps(&caps, obs_mbx, &[BundleConsumer::ID]);

    // source(0) -> call(1) -> observer(2). The observer consumes a
    // `Bundle` at slot 0, matching the call's `Bundle` output.
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Source {
                id: NodeId(0),
                mailbox: src_mbx,
                kind_id: Signal::ID,
                payload: vec![],
            },
            Node::Call {
                id: NodeId(1),
                recipient: call_mbx,
                kind_id: Signal::ID,
            },
            Node::Observer {
                id: NodeId(2),
                recipient: obs_mbx,
                kind_id: BundleConsumer::ID,
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

    validator::validate(&descriptor, &reg, &caps).expect("call dag validates");
}

#[test]
fn validator_rejects_call_unknown_recipient() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let ghost = MailboxId::from_name("test.ghost.call");
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![Node::Call {
            id: NodeId(0),
            recipient: ghost,
            kind_id: Signal::ID,
        }],
        edges: vec![],
    };
    match validator::validate(&descriptor, &reg, &caps) {
        Err(DagError::UnknownRecipient(_)) => {}
        other => panic!("expected UnknownRecipient, got {other:?}"),
    }
}

#[test]
fn validator_rejects_call_kind_not_accepted() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();
    let call_mbx = register_mailbox(&reg, "test.call_recipient");
    register_caps(&caps, call_mbx, &[KindId(0xFEED)]);
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![Node::Call {
            id: NodeId(0),
            recipient: call_mbx,
            kind_id: Signal::ID,
        }],
        edges: vec![],
    };
    match validator::validate(&descriptor, &reg, &caps) {
        Err(DagError::KindNotAccepted { node, kind_id, .. }) => {
            assert_eq!(node, NodeId(0));
            assert_eq!(kind_id, Signal::ID);
        }
        other => panic!("expected KindNotAccepted, got {other:?}"),
    }
}

#[test]
fn validator_rejects_call_consumer_not_accepting_bundle() {
    let reg = Registry::new();
    let caps = CapabilityRegistry::new();

    let call_mbx = register_mailbox(&reg, "test.call_recipient");
    let obs_mbx = register_mailbox(&reg, "test.observer");
    reg.register_kind_with_descriptor(descriptor_of::<Signal>())
        .expect("register Signal");
    reg.register_kind_with_descriptor(descriptor_of::<SignalConsumer>())
        .expect("register SignalConsumer");
    register_caps(&caps, call_mbx, &[Signal::ID]);
    register_caps(&caps, obs_mbx, &[SignalConsumer::ID]);

    // call(0) -> observer(1), but the observer declares a `Ref<Signal>`
    // input, not a `Ref<Bundle>` — type mismatch on the edge.
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Call {
                id: NodeId(0),
                recipient: call_mbx,
                kind_id: Signal::ID,
            },
            Node::Observer {
                id: NodeId(1),
                recipient: obs_mbx,
                kind_id: SignalConsumer::ID,
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };

    match validator::validate(&descriptor, &reg, &caps) {
        Err(DagError::EdgeTypeMismatch {
            edge_index,
            expected_kind,
            ..
        }) => {
            assert_eq!(edge_index, 0);
            assert_eq!(expected_kind, Bundle::ID);
        }
        other => panic!("expected EdgeTypeMismatch, got {other:?}"),
    }
}
