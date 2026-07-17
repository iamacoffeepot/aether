//! Lane selection: a `construct.*` order routes to the local arm, a `verify.*`
//! order to the Actions arm, and a config override re-points a verify lane to
//! local — the one routing decision this backend owns.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use aether_bloomery::{
    Budget, Digest, EvidenceRef, ExecutionStatus, ExecutorBackend, NetworkProfile, Nonce, Transformation, WorkHandle,
    WorkOrder,
};
use aether_bloomery_github::ExecutorError;

use super::RoutingExecutor;
use crate::bloomery::local_executor::LocalExecutorError;

/// The shared record of nonces a recorder backend was asked to submit.
type Seen = Arc<Mutex<Vec<String>>>;

// A backend that records the nonces it was asked to submit, so a test can read
// which arm an order routed to. Never errors, so its error type is a phantom.
struct Recorder<E> {
    seen: Seen,
    _marker: PhantomData<fn() -> E>,
}

impl<E> Recorder<E> {
    fn new() -> (Self, Seen) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (Self { seen: Arc::clone(&seen), _marker: PhantomData }, seen)
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

    fn cancel(&self, _handle: &WorkHandle) -> Result<(), E> {
        Ok(())
    }

    fn stream_evidence(&self, _handle: &WorkHandle) -> Result<Vec<EvidenceRef>, E> {
        Ok(Vec::new())
    }
}

fn order(command: &str, nonce: &str) -> WorkOrder {
    WorkOrder {
        transformation: Transformation {
            command: command.to_owned(),
            inputs: vec![Digest::from_bytes([5; 32])],
            checkout: Digest::from_bytes([0xC0; 32]),
            outputs: Vec::new(),
            image: "iama/x:1".to_owned(),
            limits: Budget::default(),
            network: NetworkProfile::None,
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
fn inspect_resolves_a_handle_to_the_lane_its_order_submitted_to() {
    // The router records the lane at submit so a nonce-only inspect re-resolves
    // to the arm the order went to — here the local arm, whose stub reports Unknown.
    let (router, _actions_seen, _local_seen) = router(vec!["construct.".to_owned()]);
    let handle = router.submit(&order("construct.implement", "n-c")).unwrap();
    assert_eq!(router.inspect(&handle).unwrap(), ExecutionStatus::Unknown);
}
