//! DAG-executor scenario tests (iamacoffeepot/aether#976).
//!
//! Each fixture boots a real chassis (`TraceDispatchCapability` +
//! `HandleCapability` + `DagCapability` + the relevant test caps from
//! [`super::test_support`]) through the same `Builder` the production
//! chassis uses, enqueues an `aether.dag.submit` at the dag cap's
//! mailbox with a session reply target, drains the substrate's egress
//! for the `SubmitResult`, then drives the DAG through the live actor
//! dispatch + parking + settlement path and asserts on the observer's
//! recorded payloads / the DAG's `status`.
//!
//! `TraceDispatchCapability` is load-bearing for every `Call` fixture:
//! it folds substrate-wide trace events into per-root counters and
//! fires `Settled { root }` mail once a root drains — without it the
//! executor's per-`Call` settlement subscription never wakes and the
//! bundle never closes (same dependency the RPC server's `Call` tests
//! carry).

#![allow(clippy::unwrap_used)]

use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, thread};

use aether_actor::Actor;
use aether_data::{DagId, Kind, KindId, MailId, MailboxId, SessionToken, Uuid};
use aether_kinds::descriptors;
use aether_kinds::trace::Nanos;
use aether_kinds::{
    Bundle, Cancel, CancelResult, DagDescriptor, DagReapTick, Edge, Node, NodeId, Status,
    StatusResult, Submit, SubmitResult,
};
use serde::de::DeserializeOwned;

use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::handle_store::HandleStore;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
use aether_substrate::mail::registry::{MailboxEntry, OwnedDispatch, Registry};
use aether_substrate::mail::{MailRef, Source, SourceAddr};

use super::DagCapability;
use super::test_support::{
    Recorder, SLOW_TRANSFORM_GATE, TestBundleObserverActor, TestCallActor, TestCallConfig,
    TestCallReply, TestDeferredCallActor, TestNumber, TestNumberObserved, TestNumberObserverActor,
    TestNumberRequest, TestNumberSourceActor, TestObserved, TestObserved2, TestObserverActor,
    TestParallelObserverActor, TestReadResult, TestSourceActor, big_output_transform_id,
    boom_transform_id, double_transform_id, seed_transform_id, slow_transform_id,
};
use crate::test_chassis::TestChassis;
use crate::trace::TraceDispatchCapability;

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
/// `TraceDispatchCapability` (fires `Settled` so `Call` settlement
/// subscriptions wake), `HandleCapability` (the store the executor
/// resolves into), `DagCapability` (the executor under test), and
/// `TestSourceActor` (the universal source). Each fixture chains its
/// specific observer / call cap before `build_passive`.
fn base_builder(registry: &Arc<Registry>, mailer: &Arc<Mailer>) -> Builder<TestChassis> {
    Builder::<TestChassis>::new(Arc::clone(registry), Arc::clone(mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<crate::HandleCapability>(())
        .with_actor::<DagCapability>(())
        .with_actor::<TestSourceActor>(())
}

/// Boot a `Call`-fixture chassis: the base caps plus a `TestCallActor`
/// (configured by `config`) and a `TestBundleObserverActor` recording
/// into `recorder`. Shared by the source → `Call` → bundle-observer
/// fixtures so each only declares the call config it varies.
fn boot_call_fixture(
    registry: &Arc<Registry>,
    mailer: &Arc<Mailer>,
    config: TestCallConfig,
    recorder: &Recorder<super::test_support::TestBundleObserved>,
) -> PassiveChassis<TestChassis> {
    base_builder(registry, mailer)
        .with_actor::<TestCallActor>(config)
        .with_actor::<TestBundleObserverActor>(Arc::clone(recorder))
        .build_passive()
        .expect("caps boot")
}

/// A distinct session reply target per `corr` so multiple in-flight
/// requests don't collide.
fn session(corr: u64) -> Source {
    Source::with_correlation(
        SourceAddr::Session(SessionToken(Uuid::from_u128(u128::from(corr)))),
        corr,
    )
}

/// Enqueue an already-encoded request kind at `mailbox_name` with a
/// session reply target. Drives the request through the cap's live
/// dispatcher thread.
fn enqueue<K: Kind>(registry: &Registry, mailbox_name: &str, payload: &K, sender: Source) {
    let id = registry.lookup(mailbox_name).expect("mailbox registered");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("entry") else {
        panic!("expected inbox mailbox for {mailbox_name}");
    };
    let bytes = payload.encode_into_bytes();
    handler.enqueue(OwnedDispatch::disarmed(
        K::ID,
        K::NAME.to_owned(),
        None,
        sender,
        MailRef::from(bytes),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));
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
    let chassis = boot_call_fixture(
        &registry,
        &mailer,
        TestCallConfig {
            replies: 1,
            never: false,
        },
        &recorder,
    );

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
    let chassis = boot_call_fixture(
        &registry,
        &mailer,
        TestCallConfig {
            replies: 3,
            never: false,
        },
        &recorder,
    );

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
    let chassis = boot_call_fixture(
        &registry,
        &mailer,
        TestCallConfig {
            replies: 0,
            never: true,
        },
        &recorder,
    );

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
    let chassis = boot_call_fixture(
        &registry,
        &mailer,
        TestCallConfig {
            replies: 1,
            never: false,
        },
        &recorder,
    );

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

/// Base builder plus the number source + number observer the transform
/// fixtures wire (ADR-0048 §3, iamacoffeepot/aether#1012). The
/// `DagCapability` builds its `TransformRegistry` from the link-time
/// inventory at boot, so the `double` / `boom` / `slow` / `big_output` /
/// `seed` transforms from `test_support` are dispatchable.
fn transform_builder(
    registry: &Arc<Registry>,
    mailer: &Arc<Mailer>,
    recorder: &Recorder<TestNumberObserved>,
) -> PassiveChassis<TestChassis> {
    base_builder(registry, mailer)
        .with_actor::<TestNumberSourceActor>(())
        .with_actor::<TestNumberObserverActor>(Arc::clone(recorder))
        .build_passive()
        .expect("caps boot")
}

/// A `TestNumberRequest` payload (cast-shape bytes).
fn number_req(value: u64) -> Vec<u8> {
    <TestNumberRequest as Kind>::encode_into_bytes(&TestNumberRequest { value })
}

/// number-source → transform(`tx`, output `TestNumber`) → number-observer.
fn number_transform_dag(tx: aether_data::TransformId, value: u64) -> DagDescriptor {
    DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Source {
                id: NodeId(0),
                mailbox: mbx::<TestNumberSourceActor>(),
                kind_id: <TestNumberRequest as Kind>::ID,
                payload: number_req(value),
            },
            Node::Transform {
                id: NodeId(1),
                transform_id: tx,
                output_kind_id: <TestNumber as Kind>::ID,
                timeout_ms: None,
            },
            Node::Observer {
                id: NodeId(2),
                recipient: mbx::<TestNumberObserverActor>(),
                kind_id: <TestNumberObserved as Kind>::ID,
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
    }
}

/// source → `double` transform → observer. The observer receives the
/// doubled value resolved inline; the DAG reaches `Complete` (ADR-0048
/// §3 invocation path).
#[test]
fn transform_invoke_resolves_handle() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestNumberObserved> = Arc::new(Mutex::new(Vec::new()));
    let chassis = transform_builder(&registry, &mailer, &recorder);

    let dag_id = submit_ok(
        &registry,
        &rx,
        number_transform_dag(double_transform_id(), 21),
        1,
    );

    assert!(
        poll_until(Duration::from_secs(5), || recorder.lock().unwrap().len()
            == 1),
        "observer never received the transform output",
    );
    let observed = recorder.lock().unwrap()[0].clone();
    assert_eq!(
        observed.input,
        aether_data::Ref::Inline(TestNumber { value: 42, tag: 0 }),
        "double(21) should resolve to 42",
    );
    assert!(poll_until(Duration::from_secs(5), || matches!(
        query_status(&registry, &rx, dag_id, 100),
        StatusResult::Complete { .. }
    )));

    drop(chassis);
}

/// A panicking transform maps to `Failed` with the panic message in the
/// diagnostic; the executor + sibling branches survive (ADR-0048 §6).
fn transform_panic_fails_node() {
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestNumberObserved> = Arc::new(Mutex::new(Vec::new()));
    let chassis = transform_builder(&registry, &mailer, &recorder);

    let dag_id = submit_ok(
        &registry,
        &rx,
        number_transform_dag(boom_transform_id(), 1),
        1,
    );

    let failed = poll_until(Duration::from_secs(5), || {
        matches!(
            query_status(&registry, &rx, dag_id, 100),
            StatusResult::Failed { ref error, .. } if error.contains("panicked")
        )
    });
    assert!(failed, "panicking transform should fail the node");
    assert_eq!(
        recorder.lock().unwrap().len(),
        0,
        "downstream observer must not run on a failed transform",
    );

    // The executor survives: a fresh DAG still resolves.
    let recorder2: Recorder<TestNumberObserved> = recorder;
    let dag2 = submit_ok(
        &registry,
        &rx,
        number_transform_dag(double_transform_id(), 5),
        2,
    );
    assert!(poll_until(Duration::from_secs(5), || matches!(
        query_status(&registry, &rx, dag2, 101),
        StatusResult::Complete { .. }
    )));
    assert_eq!(recorder2.lock().unwrap().len(), 1);

    drop(chassis);
}

/// A transform exceeding its `timeout_ms` marks the node `Failed
/// { error: "timeout: ..." }`; the thread orphans (the executor
/// continues). The fixture releases the gate afterward so the pool
/// joins cleanly (ADR-0048 §6).
#[test]
fn transform_timeout_fails_node() {
    SLOW_TRANSFORM_GATE.store(false, Ordering::Release);
    // SAFETY: nextest runs each test in its own process.
    unsafe {
        env::set_var("AETHER_TRANSFORM_TIMEOUT_MS", "50");
    }
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestNumberObserved> = Arc::new(Mutex::new(Vec::new()));
    let chassis = transform_builder(&registry, &mailer, &recorder);

    let dag_id = submit_ok(
        &registry,
        &rx,
        number_transform_dag(slow_transform_id(), 1),
        1,
    );

    thread::sleep(Duration::from_millis(80));
    let failed = poll_until(Duration::from_secs(5), || {
        enqueue(&registry, dag_mailbox(), &DagReapTick {}, session(300));
        thread::sleep(Duration::from_millis(20));
        matches!(
            query_status(&registry, &rx, dag_id, 301),
            StatusResult::Failed { ref error, .. } if error.contains("timeout")
        )
    });
    assert!(failed, "slow transform should fail on timeout");

    // Release the spinning worker so the pool can join on drop.
    SLOW_TRANSFORM_GATE.store(true, Ordering::Release);
    // SAFETY: nextest process isolation.
    unsafe {
        env::remove_var("AETHER_TRANSFORM_TIMEOUT_MS");
    }
    drop(chassis);
}

/// A transform whose encoded output exceeds
/// `AETHER_TRANSFORM_MAX_OUTPUT_BYTES` hard-fails the node (ADR-0048
/// §6).
#[test]
fn transform_output_overflow_fails_node() {
    // SAFETY: nextest process isolation.
    unsafe {
        env::set_var("AETHER_TRANSFORM_MAX_OUTPUT_BYTES", "8");
    }
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestNumberObserved> = Arc::new(Mutex::new(Vec::new()));
    let chassis = transform_builder(&registry, &mailer, &recorder);

    // `big_output` produces `value` zero bytes; 64 > the 8-byte cap.
    let descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Source {
                id: NodeId(0),
                mailbox: mbx::<TestNumberSourceActor>(),
                kind_id: <TestNumberRequest as Kind>::ID,
                payload: number_req(64),
            },
            Node::Transform {
                id: NodeId(1),
                transform_id: big_output_transform_id(),
                output_kind_id: <super::test_support::TestBytes as Kind>::ID,
                timeout_ms: None,
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };
    let dag_id = submit_ok(&registry, &rx, descriptor, 1);

    let failed = poll_until(Duration::from_secs(5), || {
        matches!(
            query_status(&registry, &rx, dag_id, 100),
            StatusResult::Failed { ref error, .. } if error.contains("exceeded")
        )
    });
    assert!(failed, "oversized transform output should fail the node");

    // SAFETY: nextest process isolation.
    unsafe {
        env::remove_var("AETHER_TRANSFORM_MAX_OUTPUT_BYTES");
    }
    drop(chassis);
}

/// A second DAG with the same `transform_id` + input handle ids (here a
/// zero-input `seed`, whose content-address `f(transform_id, [])` is
/// identical across DAGs) skips the invoke entirely — the cache hit
/// resolves the node. The pool's invoke count reports 1, not 2
/// (ADR-0048 §4, iamacoffeepot/aether#982).
fn transform_skips_invoke_on_cache_hit() {
    super::test_support::SEED_INVOKE_COUNT.store(0, Ordering::Release);
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestNumberObserved> = Arc::new(Mutex::new(Vec::new()));

    // A zero-input transform feeding a number observer.
    let seed_dag = || DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Transform {
                id: NodeId(0),
                transform_id: seed_transform_id(),
                output_kind_id: <TestNumber as Kind>::ID,
                timeout_ms: None,
            },
            Node::Observer {
                id: NodeId(1),
                recipient: mbx::<TestNumberObserverActor>(),
                kind_id: <TestNumberObserved as Kind>::ID,
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };

    let chassis = transform_builder(&registry, &mailer, &recorder);

    let dag1 = submit_ok(&registry, &rx, seed_dag(), 1);
    assert!(poll_until(Duration::from_secs(5), || matches!(
        query_status(&registry, &rx, dag1, 100),
        StatusResult::Complete { .. }
    )));
    // The observer node dispatches as its own causal root (executor.rs),
    // so it is NOT part of the submit chain `Complete` gates on — poll for
    // its async effect rather than asserting it the instant Complete lands.
    assert!(
        poll_until(Duration::from_secs(5), || recorder.lock().unwrap().len()
            == 1),
        "first seed DAG observer should run",
    );

    // Second DAG: same transform, same (empty) inputs -> same
    // content-address -> cache hit -> no second invoke.
    recorder.lock().unwrap().clear();
    let dag2 = submit_ok(&registry, &rx, seed_dag(), 2);
    assert!(poll_until(Duration::from_secs(5), || matches!(
        query_status(&registry, &rx, dag2, 101),
        StatusResult::Complete { .. }
    )));
    assert!(
        poll_until(Duration::from_secs(5), || recorder.lock().unwrap().len()
            == 1),
        "second seed DAG observer should still resolve from cache",
    );
    assert_eq!(
        recorder.lock().unwrap()[0].input,
        aether_data::Ref::Inline(TestNumber { value: 7, tag: 0 }),
    );

    let invoke_count = super::test_support::SEED_INVOKE_COUNT.load(Ordering::Acquire);
    assert_eq!(
        invoke_count, 1,
        "second identical transform must hit the cache, not re-invoke",
    );

    drop(chassis);
}

/// Contention/backoff-sensitive tests live in `mod heavy`: they drive the
/// live actor dispatch / parking / settlement path through a multi-worker
/// pool, so they are serialized into the `serial-heavy` nextest group
/// (`.config/nextest.toml`). Each delegates to the scenario body
/// declared at module scope.
mod heavy {
    #[test]
    fn transform_panic_fails_node() {
        super::transform_panic_fails_node();
    }

    #[test]
    fn transform_skips_invoke_on_cache_hit() {
        super::transform_skips_invoke_on_cache_hit();
    }
}

/// A transform that blocks briefly does not stall the executor's
/// parking / reaping of other DAG branches: a sibling DAG's pure
/// `double` resolves while the `slow` transform spins (ADR-0048 §3 off
/// the executor thread).
#[test]
fn transform_runs_off_executor_thread() {
    SLOW_TRANSFORM_GATE.store(false, Ordering::Release);
    let (registry, mailer, rx) = fresh_substrate_with_rx();
    let recorder: Recorder<TestNumberObserved> = Arc::new(Mutex::new(Vec::new()));
    let chassis = transform_builder(&registry, &mailer, &recorder);

    // DAG 1: a long-blocking `slow` transform (terminal, no observer).
    let slow_descriptor = DagDescriptor {
        version: 1,
        nodes: vec![
            Node::Source {
                id: NodeId(0),
                mailbox: mbx::<TestNumberSourceActor>(),
                kind_id: <TestNumberRequest as Kind>::ID,
                payload: number_req(99),
            },
            Node::Transform {
                id: NodeId(1),
                transform_id: slow_transform_id(),
                output_kind_id: <TestNumber as Kind>::ID,
                timeout_ms: Some(60_000),
            },
        ],
        edges: vec![Edge {
            from: NodeId(0),
            to: NodeId(1),
            slot: 0,
        }],
    };
    let _slow_dag = submit_ok(&registry, &rx, slow_descriptor, 1);

    // DAG 2: a pure `double` that must resolve while DAG 1 is blocked.
    let dag2 = submit_ok(
        &registry,
        &rx,
        number_transform_dag(double_transform_id(), 4),
        2,
    );
    let completed = poll_until(Duration::from_secs(5), || {
        matches!(
            query_status(&registry, &rx, dag2, 101),
            StatusResult::Complete { .. }
        )
    });
    assert!(
        completed,
        "the executor must advance a sibling DAG while a transform blocks off-thread",
    );
    assert_eq!(
        recorder.lock().unwrap()[0].input,
        aether_data::Ref::Inline(TestNumber { value: 8, tag: 0 }),
    );

    // Release the spinning worker so the pool joins cleanly.
    SLOW_TRANSFORM_GATE.store(true, Ordering::Release);
    drop(chassis);
}
