//! DAG-executor scenario tests (iamacoffeepot/aether#976).
//!
//! Each fixture boots a real chassis (`TraceObserverCapability` +
//! `HandleCapability` + `DagCapability` + the relevant test caps from
//! [`super::test_support`]) through the same `Builder` the production
//! chassis uses, enqueues an `aether.dag.submit` at the dag cap's
//! mailbox with a session reply target, drains the substrate's egress
//! for the `SubmitResult`, then drives the DAG through the live actor
//! dispatch + parking + settlement path and asserts on the observer's
//! recorded payloads / the DAG's `status`.
//!
//! `TraceObserverCapability` is load-bearing for every `Call` fixture:
//! it folds substrate-wide trace events into per-root counters and
//! fires `Settled { root }` mail once a root drains — without it the
//! executor's per-`Call` settlement subscription never wakes and the
//! bundle never closes (same dependency the RPC server's `Call` tests
//! carry).

#![allow(clippy::unwrap_used)]

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, thread};

use aether_actor::Actor;
use aether_data::{DagId, Kind, KindId, MailId, MailboxId, SessionToken, Uuid};
use aether_kinds::descriptors;
use aether_kinds::{
    Bundle, Cancel, CancelResult, DagDescriptor, DagReapTick, Edge, Node, NodeId, Status,
    StatusResult, Submit, SubmitResult,
};
use serde::de::DeserializeOwned;

use aether_substrate::chassis::builder::Builder;
use aether_substrate::handle_store::HandleStore;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
use aether_substrate::mail::registry::{MailboxEntry, OwnedDispatch, Registry};
use aether_substrate::mail::{ReplyTarget, ReplyTo};

use super::DagCapability;
use super::test_support::{
    Recorder, TestBundleObserverActor, TestCallActor, TestCallConfig, TestCallReply,
    TestDeferredCallActor, TestObserved, TestObserved2, TestObserverActor,
    TestParallelObserverActor, TestReadResult, TestSourceActor,
};
use crate::test_chassis::TestChassis;
use crate::trace::TraceObserverCapability;

/// Build a substrate seed (registry pre-loaded with `descriptors::all()`,
/// mailer wired to a drainable loopback egress). Like the crate's
/// `fresh_substrate` but exposes the egress receiver so a test can read
/// the `SubmitResult` / `CancelResult` / `StatusResult` reply.
fn fresh_substrate_with_rx() -> (Arc<Registry>, Arc<Mailer>, Receiver<EgressEvent>) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
    }
    let (outbound, rx) = HubOutbound::attached_loopback();
    let store = Arc::new(HandleStore::new(1024 * 1024));
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
    (registry, mailer, rx)
}

/// A `Builder` carrying the base every DAG fixture shares —
/// `TraceObserverCapability` (fires `Settled` so `Call` settlement
/// subscriptions wake), `HandleCapability` (the store the executor
/// resolves into), `DagCapability` (the executor under test), and
/// `TestSourceActor` (the universal source). Each fixture chains its
/// specific observer / call cap before `build_passive`.
fn base_builder(registry: &Arc<Registry>, mailer: &Arc<Mailer>) -> Builder<TestChassis> {
    Builder::<TestChassis>::new(Arc::clone(registry), Arc::clone(mailer))
        .with_actor::<TraceObserverCapability>(())
        .with_actor::<crate::HandleCapability>(())
        .with_actor::<DagCapability>(())
        .with_actor::<TestSourceActor>(())
}

/// A distinct session reply target per `corr` so multiple in-flight
/// requests don't collide.
fn session(corr: u64) -> ReplyTo {
    ReplyTo::with_correlation(
        ReplyTarget::Session(SessionToken(Uuid::from_u128(u128::from(corr)))),
        corr,
    )
}

/// Enqueue an already-encoded request kind at `mailbox_name` with a
/// session reply target. Drives the request through the cap's live
/// dispatcher thread.
fn enqueue<K: Kind + serde::Serialize>(
    registry: &Registry,
    mailbox_name: &str,
    payload: &K,
    sender: ReplyTo,
) {
    let id = registry.lookup(mailbox_name).expect("mailbox registered");
    let MailboxEntry::Inbox(handler) = registry.entry(id).expect("entry") else {
        panic!("expected inbox mailbox for {mailbox_name}");
    };
    let bytes = postcard::to_allocvec(payload).expect("encode request");
    handler.enqueue(OwnedDispatch {
        kind: K::ID,
        kind_name: K::NAME.to_owned(),
        origin: None,
        sender,
        payload: bytes,
        count: 1,
        mail_id: MailId::NONE,
        root: MailId::NONE,
        parent_mail: None,
    });
}

/// Drain egress until a `ToSession` reply of kind `K` arrives, decoding
/// it via postcard. Panics on timeout.
fn await_session_reply<K: Kind + DeserializeOwned>(
    rx: &Receiver<EgressEvent>,
    timeout: Duration,
) -> K {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = rx
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("no {} reply within deadline", K::NAME));
        if let EgressEvent::ToSession {
            kind_name, payload, ..
        } = event
            && kind_name == K::NAME
        {
            return postcard::from_bytes(&payload).expect("decode session reply");
        }
    }
}

/// The dag cap's mailbox name.
fn dag_mailbox() -> &'static str {
    <DagCapability as Actor>::NAMESPACE
}

/// Poll `f` until it returns `true` or the deadline passes; returns the
/// final value of `f`.
fn poll_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return f();
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Submit `descriptor` through the dag cap and return the minted
/// `DagId` (asserting the submit succeeded).
fn submit_ok(
    registry: &Registry,
    rx: &Receiver<EgressEvent>,
    descriptor: DagDescriptor,
    corr: u64,
) -> DagId {
    enqueue(
        registry,
        dag_mailbox(),
        &Submit { descriptor },
        session(corr),
    );
    match await_session_reply::<SubmitResult>(rx, Duration::from_secs(5)) {
        SubmitResult::Ok { dag_id, .. } => dag_id,
        SubmitResult::Err { error } => panic!("submit failed: {error:?}"),
    }
}

/// Query the DAG's status through the cap.
fn query_status(
    registry: &Registry,
    rx: &Receiver<EgressEvent>,
    dag_id: DagId,
    corr: u64,
) -> StatusResult {
    enqueue(registry, dag_mailbox(), &Status { dag_id }, session(corr));
    await_session_reply::<StatusResult>(rx, Duration::from_secs(5))
}

/// A `Source` node feeding kind `K` to `mailbox` with `payload`.
fn source_node(id: u32, mailbox: MailboxId, kind: KindId, payload: Vec<u8>) -> Node {
    Node::Source {
        id: NodeId(id),
        mailbox,
        kind_id: kind,
        payload,
    }
}

/// Mailbox id for an actor by its namespace.
fn mbx<A: Actor>() -> MailboxId {
    MailboxId(aether_data::mailbox_id_from_name(A::NAMESPACE).0)
}

/// Postcard-encode a `TestSourceRequest`.
fn source_req(value: u64, fail: bool) -> Vec<u8> {
    postcard::to_allocvec(&super::test_support::TestSourceRequest { value, fail }).unwrap()
}

/// The `TestSourceRequest` kind id.
fn source_kind() -> KindId {
    <super::test_support::TestSourceRequest as Kind>::ID
}

/// DAG: read-source → observer. The observer receives the source's
/// resolved reply inline; status reaches `Complete`.
#[test]
fn dag_executor_runs_two_node_dag() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestObserved> = Arc::new(Mutex::new(Vec::new()));
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            source_node(
                0,
                mbx::<TestSourceActor>(),
                source_kind(),
                source_req(42, false),
            ),
            Node::Observer {
                id: NodeId(1),
                recipient: mbx::<TestObserverActor>(),
                kind_id: <TestObserved as Kind>::ID,
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };

    let dag_id = submit_ok(&registry, &rx, descriptor, 1);

    assert!(
        poll_until(Duration::from_secs(5), || recorder.lock().unwrap().len()
            == 1),
        "observer never received the source reply",
    );
    let observed = recorder.lock().unwrap()[0].clone();
    assert_eq!(
        observed.input,
        aether_data::Ref::Inline(TestReadResult::Ok { value: 42 })
    );

    assert!(poll_until(Duration::from_secs(5), || matches!(
        query_status(&registry, &rx, dag_id, 100),
        StatusResult::Complete { .. }
    )));

    drop(chassis);
}

/// Same DAG: after the source resolves, no parked mail remains on the
/// source handle (the eagerly-dispatched observer parked there until the
/// reply landed, then un-parked).
#[test]
fn dag_executor_parks_observer_until_source_resolves() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestObserved> = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::clone(mailer.handle_store());
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            source_node(
                0,
                mbx::<TestSourceActor>(),
                source_kind(),
                source_req(7, false),
            ),
            Node::Observer {
                id: NodeId(1),
                recipient: mbx::<TestObserverActor>(),
                kind_id: <TestObserved as Kind>::ID,
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };

    enqueue(&registry, dag_mailbox(), &Submit { descriptor }, session(1));
    let source_handle = match await_session_reply::<SubmitResult>(&rx, Duration::from_secs(5)) {
        SubmitResult::Ok { output_handles, .. } => {
            output_handles
                .iter()
                .find(|h| h.node_id == NodeId(0))
                .expect("source has an output handle")
                .handle_id
        }
        SubmitResult::Err { error } => panic!("submit failed: {error:?}"),
    };

    assert!(
        poll_until(Duration::from_secs(5), || recorder.lock().unwrap().len()
            == 1),
        "observer never ran",
    );
    assert_eq!(store.parked_count(source_handle), 0);

    drop(chassis);
}

/// DAG: two reads → one observer with two input slots. The observer
/// fires once both sources resolve, both slots filled.
#[test]
fn dag_executor_runs_parallel_sources() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestObserved2> = Arc::new(Mutex::new(Vec::new()));
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestParallelObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            source_node(
                0,
                mbx::<TestSourceActor>(),
                source_kind(),
                source_req(10, false),
            ),
            source_node(
                1,
                mbx::<TestSourceActor>(),
                source_kind(),
                source_req(20, false),
            ),
            Node::Observer {
                id: NodeId(2),
                recipient: mbx::<TestParallelObserverActor>(),
                kind_id: <TestObserved2 as Kind>::ID,
            },
        ],
        edges: vec![
            Edge {
                from: NodeId(0),
                to: NodeId(2),
                slot: 0,
            },
            Edge {
                from: NodeId(1),
                to: NodeId(2),
                slot: 1,
            },
        ],
    };

    let _dag = submit_ok(&registry, &rx, descriptor, 1);

    assert!(
        poll_until(Duration::from_secs(5), || recorder.lock().unwrap().len()
            == 1),
        "two-slot observer never ran",
    );
    let observed = recorder.lock().unwrap()[0].clone();
    assert_eq!(
        observed.a,
        aether_data::Ref::Inline(TestReadResult::Ok { value: 10 })
    );
    assert_eq!(
        observed.b,
        aether_data::Ref::Inline(TestReadResult::Ok { value: 20 })
    );

    drop(chassis);
}

/// A read source whose reply is an `Err` variant: the observer's
/// `Ref<TestReadResult>` slot resolves to the `Err` inline, and the
/// observer is still dispatched.
#[test]
fn dag_executor_propagates_source_err_as_observer_input() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestObserved> = Arc::new(Mutex::new(Vec::new()));
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            source_node(
                0,
                mbx::<TestSourceActor>(),
                source_kind(),
                source_req(9, true),
            ),
            Node::Observer {
                id: NodeId(1),
                recipient: mbx::<TestObserverActor>(),
                kind_id: <TestObserved as Kind>::ID,
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };

    let _dag = submit_ok(&registry, &rx, descriptor, 1);

    assert!(
        poll_until(Duration::from_secs(5), || recorder.lock().unwrap().len()
            == 1),
        "observer never received the Err source reply",
    );
    let observed = recorder.lock().unwrap()[0].clone();
    match observed.input {
        aether_data::Ref::Inline(TestReadResult::Err { message }) => {
            assert!(message.contains("failed"), "got {message}");
        }
        other => panic!("expected inline Err, got {other:?}"),
    }

    drop(chassis);
}

/// Cancel a DAG: `Ok { cancelled: true }` for a still-running DAG (status
/// then reads `Failed { error: "cancelled" }`), or `Ok { cancelled:
/// false }` if it had already completed (a tolerated race).
#[test]
fn dag_executor_cancels_running_dag() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestObserved> = Arc::new(Mutex::new(Vec::new()));
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            source_node(
                0,
                mbx::<TestSourceActor>(),
                source_kind(),
                source_req(1, false),
            ),
            Node::Observer {
                id: NodeId(1),
                recipient: mbx::<TestObserverActor>(),
                kind_id: <TestObserved as Kind>::ID,
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };

    let dag_id = submit_ok(&registry, &rx, descriptor, 1);
    enqueue(&registry, dag_mailbox(), &Cancel { dag_id }, session(2));
    match await_session_reply::<CancelResult>(&rx, Duration::from_secs(5)) {
        CancelResult::Ok { cancelled: true } => {
            assert!(poll_until(Duration::from_secs(5), || matches!(
                query_status(&registry, &rx, dag_id, 100),
                StatusResult::Failed { ref error, .. } if error == "cancelled"
            )));
            assert_eq!(recorder.lock().unwrap().len(), 0);
        }
        CancelResult::Ok { cancelled: false } => {
            assert!(matches!(
                query_status(&registry, &rx, dag_id, 100),
                StatusResult::Complete { .. }
            ));
        }
        CancelResult::Err { error } => panic!("cancel errored: {error}"),
    }

    drop(chassis);
}

/// A DAG polled mid-flight reports `Running` (with a per-node progress
/// list naming both nodes), `Pending`, or `Complete`; it always reaches
/// `Complete`.
#[test]
fn dag_executor_status_reports_running() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestObserved> = Arc::new(Mutex::new(Vec::new()));
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            source_node(
                0,
                mbx::<TestSourceActor>(),
                source_kind(),
                source_req(5, false),
            ),
            Node::Observer {
                id: NodeId(1),
                recipient: mbx::<TestObserverActor>(),
                kind_id: <TestObserved as Kind>::ID,
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };

    let dag_id = submit_ok(&registry, &rx, descriptor, 1);
    match query_status(&registry, &rx, dag_id, 2) {
        StatusResult::Running { progress } => {
            assert_eq!(progress.len(), 2);
            assert!(progress.iter().any(|p| p.node_id == NodeId(0)));
            assert!(progress.iter().any(|p| p.node_id == NodeId(1)));
        }
        StatusResult::Pending | StatusResult::Complete { .. } => {}
        StatusResult::Failed { node_id, error } => {
            panic!("unexpected Failed({node_id:?}, {error})")
        }
    }
    assert!(poll_until(Duration::from_secs(5), || matches!(
        query_status(&registry, &rx, dag_id, 100),
        StatusResult::Complete { .. }
    )));

    drop(chassis);
}

/// With tiny retention windows, a completed DAG is observable as
/// `Complete`, then reaped — after which `status` reports the unknown-dag
/// shape (`Failed { error: "unknown dag .." }`).
#[test]
fn dag_executor_status_reports_complete_then_reaps() {
    // SAFETY: nextest runs each test in its own process, so the env set
    // here doesn't race sibling tests.
    unsafe {
        env::set_var("AETHER_DAG_RETENTION_COMPLETE_MS", "50");
        env::set_var("AETHER_DAG_RETENTION_FAILED_MS", "50");
    }
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestObserved> = Arc::new(Mutex::new(Vec::new()));
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            source_node(
                0,
                mbx::<TestSourceActor>(),
                source_kind(),
                source_req(3, false),
            ),
            Node::Observer {
                id: NodeId(1),
                recipient: mbx::<TestObserverActor>(),
                kind_id: <TestObserved as Kind>::ID,
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };

    let dag_id = submit_ok(&registry, &rx, descriptor, 1);
    assert!(poll_until(Duration::from_secs(5), || matches!(
        query_status(&registry, &rx, dag_id, 100),
        StatusResult::Complete { .. }
    )));

    thread::sleep(Duration::from_millis(120));
    let unknown = poll_until(Duration::from_secs(5), || {
        enqueue(&registry, dag_mailbox(), &DagReapTick {}, session(200));
        thread::sleep(Duration::from_millis(20));
        matches!(
            query_status(&registry, &rx, dag_id, 201),
            StatusResult::Failed { ref error, .. } if error.starts_with("unknown dag")
        )
    });
    assert!(unknown, "reaped DAG should report unknown-dag");

    // SAFETY: nextest runs each test in its own process; no sibling
    // thread reads these env vars concurrently.
    unsafe {
        env::remove_var("AETHER_DAG_RETENTION_COMPLETE_MS");
        env::remove_var("AETHER_DAG_RETENTION_FAILED_MS");
    }
    drop(chassis);
}

/// source → Call (single-reply cap) → observer consuming the Bundle.
/// The observer receives a 1-element Bundle whose element is the cap's
/// reply.
#[test]
fn dag_executor_call_collects_single_reply_bundle() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestCallActor>(TestCallConfig {
            replies: 1,
            never: false,
        })
        .with_actor::<TestBundleObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let bundle = run_call_dag_to(&registry, &rx, &recorder, mbx::<TestCallActor>());
    assert_eq!(bundle.elements.len(), 1);
    assert_eq!(bundle.elements[0].kind_id, <TestCallReply as Kind>::ID);

    drop(chassis);
}

/// Call to a cap that emits N correlated replies before settling. The
/// resolved Bundle has N elements in emission order.
#[test]
fn dag_executor_call_collects_multi_reply_bundle() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestCallActor>(TestCallConfig {
            replies: 3,
            never: false,
        })
        .with_actor::<TestBundleObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let bundle = run_call_dag_to(&registry, &rx, &recorder, mbx::<TestCallActor>());
    assert_eq!(bundle.elements.len(), 3);
    for (i, el) in bundle.elements.iter().enumerate() {
        let reply: TestCallReply = postcard::from_bytes(&el.payload).unwrap();
        assert_eq!(reply.index, i as u64);
    }

    drop(chassis);
}

/// A spawn-and-die-worker `Call` cap: the bundle must not close until the
/// worker's deferred reply lands (the `SettlementHold` keeps the chain
/// open). The element is the worker's reply, proving it wasn't dropped
/// into an already-settled bundle.
#[test]
fn dag_executor_call_inherited_worker_counts_toward_settlement() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestDeferredCallActor>(())
        .with_actor::<TestBundleObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let bundle = run_call_dag_to(&registry, &rx, &recorder, mbx::<TestDeferredCallActor>());
    assert_eq!(
        bundle.elements.len(),
        1,
        "the deferred worker's reply must land in the bundle, not an already-closed one",
    );
    let reply: TestCallReply = postcard::from_bytes(&bundle.elements[0].payload).unwrap();
    assert_eq!(reply.index, 1);

    drop(chassis);
}

/// Call to a cap that never settles (holds the chain open forever): with
/// a tiny per-`Call` timeout the node fails rather than buffering forever
/// or truncating to a partial bundle.
#[test]
fn dag_executor_call_times_out_nonsettling_cap() {
    // SAFETY: nextest runs each test in its own process.
    unsafe {
        env::set_var("AETHER_DAG_CALL_TIMEOUT_MS", "100");
    }
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestCallActor>(TestCallConfig {
            replies: 0,
            never: true,
        })
        .with_actor::<TestBundleObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let dag_id = submit_call_dag(&registry, &rx, mbx::<TestCallActor>());

    thread::sleep(Duration::from_millis(150));
    let failed = poll_until(Duration::from_secs(5), || {
        enqueue(&registry, dag_mailbox(), &DagReapTick {}, session(300));
        thread::sleep(Duration::from_millis(20));
        matches!(
            query_status(&registry, &rx, dag_id, 301),
            StatusResult::Failed { ref error, .. } if error.contains("timed out")
        )
    });
    assert!(failed, "non-settling Call should fail on timeout");
    assert_eq!(recorder.lock().unwrap().len(), 0);

    // SAFETY: nextest runs each test in its own process; no sibling
    // thread reads this env var concurrently.
    unsafe {
        env::remove_var("AETHER_DAG_CALL_TIMEOUT_MS");
    }
    drop(chassis);
}

/// The call dispatch mints a fresh `call_root` distinct from the DAG
/// root: a non-empty bundle can only close if the executor subscribed to
/// the call's own root (a DAG-root subscription would never fire
/// mid-graph).
#[test]
fn dag_executor_call_dispatches_as_own_root() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let chassis = base_builder(&registry, &mailer)
        .with_actor::<TestCallActor>(TestCallConfig {
            replies: 1,
            never: false,
        })
        .with_actor::<TestBundleObserverActor>(Arc::clone(&recorder))
        .build_passive()
        .expect("caps boot");

    let bundle = run_call_dag_to(&registry, &rx, &recorder, mbx::<TestCallActor>());
    assert_eq!(
        bundle.elements.len(),
        1,
        "the per-call settlement subscription must fire to close a non-empty bundle",
    );

    drop(chassis);
}

/// Build + submit a source → Call → bundle-observer DAG with the Call
/// addressed at `call_mbx`, and return the `DagId`.
fn submit_call_dag(registry: &Registry, rx: &Receiver<EgressEvent>, call_mbx: MailboxId) -> DagId {
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            source_node(
                0,
                mbx::<TestSourceActor>(),
                source_kind(),
                source_req(1, false),
            ),
            Node::Call {
                id: NodeId(1),
                recipient: call_mbx,
                kind_id: <super::test_support::TestCallRequest as Kind>::ID,
            },
            Node::Observer {
                id: NodeId(2),
                recipient: mbx::<TestBundleObserverActor>(),
                kind_id: <super::test_support::TestBundleObserved as Kind>::ID,
            },
        ],
        edges: vec![
            // source gates the Call's dispatch (TestCallRequest has no
            // Ref slots, so the edge contributes no substitution).
            Edge {
                from: NodeId(0),
                to: NodeId(1),
                slot: 0,
            },
            // Call's Bundle output feeds the observer's slot 0.
            Edge {
                from: NodeId(1),
                to: NodeId(2),
                slot: 0,
            },
        ],
    };
    submit_ok(registry, rx, descriptor, 1)
}

/// Run the Call DAG against a specific Call mailbox + return the Bundle
/// the observer received.
fn run_call_dag_to(
    registry: &Registry,
    rx: &Receiver<EgressEvent>,
    recorder: &Recorder<super::test_support::TestBundleObserved>,
    call_mbx: MailboxId,
) -> Bundle {
    let _dag = submit_call_dag(registry, rx, call_mbx);
    assert!(
        poll_until(Duration::from_secs(8), || recorder.lock().unwrap().len()
            == 1),
        "bundle observer never ran",
    );
    let observed = recorder.lock().unwrap()[0].clone();
    match observed.input {
        aether_data::Ref::Inline(bundle) => bundle,
        aether_data::Ref::Handle { .. } => panic!("bundle slot should be resolved inline"),
    }
}
