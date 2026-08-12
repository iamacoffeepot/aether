//! Lane selection: a `construct.*` order routes to the local arm, a `verify.*`
//! order to the Actions arm, and a config override re-points a verify lane to
//! local — the one routing decision this backend owns.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use aether_bloomery::{
    Digest, EvidenceRef, ExecutionLimits, ExecutionStatus, ExecutorBackend, NetworkProfile, Nonce, Transformation,
    VerifyFailure, VerifyFailureSet, WorkHandle, WorkOrder,
};
use aether_bloomery_github::ExecutorError;

use super::RoutingExecutor;
use crate::bloomery::executor::local::LocalExecutorError;
use crate::bloomery::executor::{OutstandingDispatch, ReconcileLanes, ReconcileReport};

/// The shared record of nonces a recorder backend was asked to submit.
type Seen = Arc<Mutex<Vec<String>>>;

// A backend that records the nonces it was asked to submit, so a test can read
// which arm an order routed to. Never errors, so its error type is a phantom.
struct Recorder<E> {
    seen: Seen,
    evidence: Vec<EvidenceRef>,
    // What this arm claims to have re-adopted at boot, when the router asks it to
    // reconcile — the local arm's observed footprint, which is what the router
    // rebuilds its routing map from.
    readopted: Vec<Nonce>,
    _marker: PhantomData<fn() -> E>,
}

impl<E> Recorder<E> {
    fn new() -> (Self, Seen) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (Self { seen: Arc::clone(&seen), evidence: Vec::new(), readopted: Vec::new(), _marker: PhantomData }, seen)
    }

    fn returning(evidence: Vec<EvidenceRef>) -> (Self, Seen) {
        let (mut recorder, seen) = Self::new();
        recorder.evidence = evidence;
        (recorder, seen)
    }

    fn readopting(readopted: Vec<Nonce>) -> (Self, Seen) {
        let (mut recorder, seen) = Self::new();
        recorder.readopted = readopted;
        (recorder, seen)
    }
}

impl<E: Send + Sync> ReconcileLanes for Recorder<E> {
    fn reconcile(&self, _live: &[OutstandingDispatch]) -> ReconcileReport {
        ReconcileReport { readopted: self.readopted.clone(), reclaimed: 0 }
    }
}

impl<E: Send + Sync> ExecutorBackend for Recorder<E> {
    type Error = E;

    fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, E> {
        self.seen.lock().unwrap().push(order.nonce.0.clone());
        Ok(WorkHandle::new(order.nonce.clone()))
    }

    fn inspect(&self, _handle: &WorkHandle) -> Result<ExecutionStatus, E> {
        Ok(ExecutionStatus::Unknown)
    }

    fn cancel(&self, handle: &WorkHandle) -> Result<(), E> {
        self.seen.lock().unwrap().push(handle.nonce.0.clone());
        Ok(())
    }

    fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, E> {
        self.seen.lock().unwrap().push(handle.nonce.0.clone());
        Ok(self.evidence.clone())
    }
}

fn order(command: &str, nonce: &str) -> WorkOrder {
    WorkOrder {
        transformation: Transformation {
            command: command.to_owned(),
            inputs: vec![Digest::from_bytes([5; 32])],
            checkout: Digest::from_bytes([0xC0; 32]),
            diff_base: None,
            outputs: Vec::new(),
            image: "iama/x:1".to_owned(),
            limits: ExecutionLimits { wall_clock_secs: 3_600 },
            network: NetworkProfile::None,
            description: None,
            model: None,
        },
        nonce: Nonce(nonce.to_owned()),
    }
}

fn router(local_prefixes: Vec<String>) -> (RoutingExecutor, Seen, Seen) {
    let (actions, actions_seen) = Recorder::<ExecutorError>::new();
    let (local, local_seen) = Recorder::<LocalExecutorError>::new();
    let router = RoutingExecutor::new(Arc::new(actions), Arc::new(local), local_prefixes);
    (router, actions_seen, local_seen)
}

#[test]
fn construct_routes_local_and_verify_routes_actions_by_default() {
    let (router, actions_seen, local_seen) = router(vec!["construct.".to_owned()]);

    router.submit(&order("construct.implement", "n-c")).unwrap();
    router.submit(&order("verify.clippy", "n-v")).unwrap();

    assert_eq!(*local_seen.lock().unwrap(), vec!["n-c".to_owned()], "the model lane routes to the local backend");
    assert_eq!(*actions_seen.lock().unwrap(), vec!["n-v".to_owned()], "the verify lane routes to the Actions backend");
}

#[test]
fn a_config_override_repoints_a_verify_lane_to_local() {
    // The release valve: adding `verify.` to the local prefix set flips the verify
    // lane to the local backend without touching the routing code.
    let (router, actions_seen, local_seen) = router(vec!["construct.".to_owned(), "verify.".to_owned()]);

    router.submit(&order("verify.clippy", "n-v")).unwrap();

    assert_eq!(*local_seen.lock().unwrap(), vec!["n-v".to_owned()], "the override routes verify to local");
    assert!(actions_seen.lock().unwrap().is_empty(), "nothing reached the Actions backend");
}

#[test]
fn a_terminal_message_evicts_the_routing_record() {
    // `routed` must track in-flight orders, not the process's lifetime total: the
    // last message for an order drops its lane record. `stream_evidence` is that
    // last message here, so the following cancel — finding no record — falls back
    // to the Actions arm instead of re-resolving to the local arm the submit used.
    let (router, actions_seen, local_seen) = router(vec!["construct.".to_owned()]);
    let handle = router.submit(&order("construct.implement", "n-c")).unwrap();

    router.stream_evidence(&handle).unwrap();
    router.cancel(&handle).unwrap();

    assert_eq!(*local_seen.lock().unwrap(), vec!["n-c", "n-c"], "submit + stream both routed to the local arm");
    assert_eq!(*actions_seen.lock().unwrap(), vec!["n-c"], "after eviction the cancel falls back to the Actions arm");
}

#[test]
fn inspect_resolves_a_handle_to_the_lane_its_order_submitted_to() {
    // The router records the lane at submit so a nonce-only inspect re-resolves
    // to the arm the order went to — here the local arm, whose stub reports Unknown.
    let (router, _actions_seen, _local_seen) = router(vec!["construct.".to_owned()]);
    let handle = router.submit(&order("construct.implement", "n-c")).unwrap();
    assert_eq!(router.inspect(&handle).unwrap(), ExecutionStatus::Unknown);
}

#[test]
fn stream_preserves_the_backend_failure_set_unchanged() {
    let failures = [VerifyFailure::Fmt, VerifyFailure::Test].into_iter().collect::<VerifyFailureSet>();
    let reference = EvidenceRef {
        name: "attempt.fail.12.subject.detail.n-v".to_owned(),
        nonce: Nonce("n-v".to_owned()),
        artifact_id: 1,
        size_bytes: 10,
        candidate: None,
        findings: None,
        failed_verifiers: failures,
        cost: None,
    };
    let (actions, _actions_seen) = Recorder::<ExecutorError>::new();
    let (local, _local_seen) = Recorder::<LocalExecutorError>::returning(vec![reference.clone()]);
    let router = RoutingExecutor::new(Arc::new(actions), Arc::new(local), vec!["verify.".to_owned()]);
    let handle = router.submit(&order("verify.check", "n-v")).unwrap();

    assert_eq!(router.stream_evidence(&handle).unwrap(), vec![reference]);
}

#[test]
fn reconcile_re_routes_a_readopted_nonce_to_the_local_arm() {
    // Issue #4847's compounding gap: the routing map is process memory, so after a
    // restart every outstanding nonce misses and takes the Actions fallback — a
    // local-lane order's cancel goes to GitHub, probes both run wrappers, and
    // returns Ok without ever reaching the arm that holds the run. The lane is
    // recovered from what the local arm can still see of the dispatch, not by
    // re-deriving it from the prefix set (which is config, and may have been
    // flipped since the dispatch it would be re-deciding).
    let (actions, actions_seen) = Recorder::<ExecutorError>::new();
    let (local, local_seen) = Recorder::<LocalExecutorError>::readopting(vec![Nonce("n-c".to_owned())]);
    let router = RoutingExecutor::new(Arc::new(actions), Arc::new(local), vec!["construct.".to_owned()]);

    let report = router.reconcile(&[]);

    assert_eq!(report.readopted, vec![Nonce("n-c".to_owned())], "the router reports what the local arm recovered");
    router.cancel(&WorkHandle::new(Nonce("n-c".to_owned()))).unwrap();
    router.cancel(&WorkHandle::new(Nonce("n-untouched".to_owned()))).unwrap();

    assert_eq!(*local_seen.lock().unwrap(), vec!["n-c".to_owned()], "the recovered nonce cancels on the local arm");
    assert_eq!(
        *actions_seen.lock().unwrap(),
        vec!["n-untouched".to_owned()],
        "a nonce with no local footprint keeps the Actions fallback",
    );
}
