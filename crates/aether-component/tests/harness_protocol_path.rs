//! A guest decodes a `ProtocolPath<P>` and resolves it to a reference
//! (ADR-0231 §3, issue #7501), driven through real loads and mail.
//!
//! The `PathHolder` fixture is told "an actor covering `PathPoking` stands at
//! this path" three ways: in a `PathAttach` request, in its config, and in
//! the config it hands its inline child. Its decode proves each path against
//! the engine's published routes, as a native receiver's does, and its
//! `resolve` proves a live actor still stands there.
//!
//! The target is a `ParentPeerStandIn`, which tells on `Bump` and reports a
//! `TickObserved` to the harness observer for each one. A path that does not
//! prove is written from an actor type too, since nothing attaches a
//! protocol to arbitrary text: [`absent`] names a stand-in key nothing was
//! loaded at, and [`SkewedSidecar`] claims a row the loaded `Sidecar` lacks.

use std::fs;

use aether_actor::{
    ActorInitError, ActorPath, ActorRef, PathRefusal, PathRefused, ProtocolPath, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_component::ComponentHostCapability;
use aether_data::{ErasedActorPath, Kind, LoadName};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::{DropComponent, DropResult, LoadComponent};
use aether_substrate::testing::successor_wasm;
use aether_test_fixtures_bundle::{ParentPeerStandIn, PathHolder, PathHolderChild, Sidecar};
use aether_test_fixtures_kinds::{
    Bump, PATH_HOLDER_CHILD, PathAnswer, PathAttach, PathEcho, PathEchoed, PathHolderConfig, PathPoking, TickObserved,
};

const BUNDLE: &str = "aether_test_fixtures_bundle";

/// `Sidecar`'s name under a contract the loaded `Sidecar` does not publish:
/// a silent `Bump` row. A path written from it is well-typed here and names
/// a live guest that lacks the row, the build skew the decode's check
/// exists for. It is never loaded.
struct SkewedSidecar;

#[actor(instanced, root)]
impl WasmActor for SkewedSidecar {
    const NAMESPACE: &'static str = Sidecar::NAMESPACE;

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(SkewedSidecar)
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {}
}

fn key(name: &str) -> LoadName {
    LoadName::new(name).expect("a valid instance key")
}

/// The path of the stand-in loaded, or never loaded, at `name`.
fn stand_in(name: &str) -> ProtocolPath<PathPoking> {
    ActorPath::<ParentPeerStandIn>::instance(&key(name)).narrow()
}

/// A covering path no route has stood at.
fn absent() -> ProtocolPath<PathPoking> {
    stand_in("nowhere")
}

fn erased(path: &ProtocolPath<PathPoking>) -> ErasedActorPath {
    path.as_erased().clone()
}

/// The harness, the bundle's bytes, and a stand-in live at the key `target`.
struct Scenario {
    harness: SubstrateHarness,
    wasm: Vec<u8>,
}

impl Scenario {
    fn boot() -> Option<Self> {
        let wasm = fs::read(require_wasm(BUNDLE)?).expect("read fixture wasm");
        let harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
        let mut scenario = Self { harness, wasm };
        scenario
            .harness
            .load::<ParentPeerStandIn>(scenario.keyed("target", Vec::new()))
            .unwrap_or_else(|error| panic!("the target loads: {error}"));

        Some(scenario)
    }

    /// A load of the bundle keyed `name` with `config`; the typed load sets
    /// its export.
    fn keyed(&self, name: &str, config: Vec<u8>) -> LoadComponent {
        LoadComponent { wasm: self.wasm.clone(), name: Some(name.to_owned()), config, export: None }
    }

    /// Load a holder keyed `name` with `config`.
    fn holder(&mut self, name: &str, config: &PathHolderConfig) -> Result<ActorRef<PathHolder>, SubstrateHarnessError> {
        let load = self.keyed(name, config.encode_into_bytes());

        self.harness.load::<PathHolder>(load)
    }

    /// A holder keyed `name` whose config names no path.
    fn bare_holder(&mut self, name: &str) -> ActorRef<PathHolder> {
        self.holder(name, &PathHolderConfig::default()).unwrap_or_else(|error| panic!("a bare holder loads: {error}"))
    }

    /// The holder's answer to an attach naming `target`.
    fn attach(&mut self, holder: ActorRef<PathHolder>, target: ProtocolPath<PathPoking>) -> PathAnswer {
        self.harness
            .execute(vec![("attach", HarnessOp::send_and_await_reply(&holder, &PathAttach { target }))])
            .expect("the attach is answered")
            .reply::<PathAnswer>("attach")
            .expect("decode PathAnswer")
    }

    /// The paths the holder says it holds.
    fn echo(&mut self, holder: ActorRef<PathHolder>) -> PathEchoed {
        self.harness
            .execute(vec![("echo", HarnessOp::send_and_await_reply(&holder, &PathEcho))])
            .expect("the echo is answered")
            .reply::<PathEchoed>("echo")
            .expect("decode PathEchoed")
    }

    /// The paths the holder's inline child says it holds. The holder's
    /// `wire` staged the child's alias before the load was answered, so the
    /// barrier proves the registry owner has applied it.
    fn child_echo(&mut self, holder: ActorRef<PathHolder>) -> PathEchoed {
        self.harness.await_registry_applied();
        let child = self
            .harness
            .child::<PathHolder, PathHolderChild>(&holder, key(PATH_HOLDER_CHILD))
            .unwrap_or_else(|error| panic!("the holder's inline child is live: {error}"));

        self.harness
            .execute(vec![("echo", HarnessOp::send_and_await_reply(&child, &PathEcho))])
            .expect("the child's echo is answered")
            .reply::<PathEchoed>("echo")
            .expect("decode PathEchoed")
    }
}

fn refused_load(loaded: Result<ActorRef<PathHolder>, SubstrateHarnessError>) -> String {
    match loaded {
        Err(SubstrateHarnessError::Load(error)) => error,
        other => panic!("the load must be refused; got {other:?}"),
    }
}

/// Catches a mail decode whose context carries no routes: every path would
/// be answered `Unchecked`, whatever it names, and the handler never run.
#[test]
fn a_request_naming_a_live_covering_guest_reaches_the_handler() {
    let Some(mut scenario) = Scenario::boot() else {
        return;
    };
    let holder = scenario.bare_holder("holder");

    let answer = scenario.attach(holder, stand_in("target"));

    assert_eq!(answer, PathAnswer::Ok { path: erased(&stand_in("target")) });
    assert_eq!(scenario.echo(holder).attached, Some(erased(&stand_in("target"))), "the handler kept the path");
}

/// Catches an absent route answered as an empty row set (`Uncovered`), and a
/// handler run on an unproven path: the handler's own answers are `Ok` and
/// `NotLive`, so `Unpublished` is the dispatch's, and the holder kept
/// nothing.
#[test]
fn a_request_naming_a_path_no_route_stood_at_is_answered_unpublished() {
    let Some(mut scenario) = Scenario::boot() else {
        return;
    };
    let holder = scenario.bare_holder("holder");

    let answer = scenario.attach(holder, absent());

    let refused = PathRefused { path: erased(&absent()), reason: PathRefusal::Unpublished };
    assert_eq!(answer, PathAnswer::Err(refused));
    assert_eq!(scenario.echo(holder).attached, None, "the handler did not run");
}

/// Catches coverage skipped once a route is found: a live guest that lacks
/// the protocol's row would be proven and mailed a kind it does not handle.
#[test]
fn a_request_naming_a_live_guest_that_lacks_the_row_is_answered_uncovered() {
    let Some(mut scenario) = Scenario::boot() else {
        return;
    };
    let holder = scenario.bare_holder("holder");
    let bystander = scenario.keyed("bystander", Vec::new());
    scenario.harness.load::<Sidecar>(bystander).unwrap_or_else(|error| panic!("the bystander loads: {error}"));
    let skewed: ProtocolPath<PathPoking> = ActorPath::<SkewedSidecar>::instance(&key("bystander")).narrow();

    let answer = scenario.attach(holder, skewed.clone());

    let refused = PathRefused { path: erased(&skewed), reason: PathRefusal::Uncovered { kind: Bump::ID } };
    assert_eq!(answer, PathAnswer::Err(refused));
}

/// Catches an init shim that decodes the config with no routes (the load
/// would be refused); a `resolve` that mints for a position other than the
/// path's route; and a `send_to` through the reference that does not
/// arrive: the target reports the one `Bump` the holder's `wire` sent.
#[test]
fn a_config_naming_a_live_covering_guest_loads_and_its_wire_mails_the_target() {
    let Some(mut scenario) = Scenario::boot() else {
        return;
    };
    let baseline = scenario.harness.count_observed(TickObserved::NAME);
    let config = PathHolderConfig { target: Some(stand_in("target")), child_target: None };

    let holder = scenario.holder("holder", &config).unwrap_or_else(|error| panic!("the holder loads: {error}"));

    assert_eq!(scenario.echo(holder).config, Some(erased(&stand_in("target"))));
    assert_eq!(
        scenario.harness.count_observed(TickObserved::NAME) - baseline,
        1,
        "the target received the holder's bump; observed kinds: {:?}",
        scenario.harness.observed_kinds(),
    );
}

/// Catches a config refusal with no reason, and a `Starting` route or a
/// tombstone left behind by the refused birth: the second load at the same
/// name stands up.
#[test]
fn a_config_naming_an_absent_path_refuses_the_load_with_the_reason() {
    let Some(mut scenario) = Scenario::boot() else {
        return;
    };
    let config = PathHolderConfig { target: Some(absent()), child_target: None };

    let loaded = scenario.holder("holder", &config);

    let error = refused_load(loaded);
    let names_the_path = error.contains(absent().as_erased().as_str());
    let says_why = error.contains("no route has published");
    assert!(names_the_path && says_why, "the refusal names the path and the reason: {error}");

    let good = PathHolderConfig { target: Some(stand_in("target")), child_target: None };
    let holder = scenario.holder("holder", &good).unwrap_or_else(|error| panic!("the name is free: {error}"));
    assert_eq!(scenario.echo(holder).config, Some(erased(&stand_in("target"))));
}

/// Catches an inline spawn whose config round-trip decodes with no routes:
/// the child would never be built, or the load refused with no reason.
#[test]
fn a_config_hands_its_inline_child_a_path_the_child_decodes() {
    let Some(mut scenario) = Scenario::boot() else {
        return;
    };
    let config = PathHolderConfig { target: None, child_target: Some(stand_in("target")) };

    let holder = scenario.holder("holder", &config).unwrap_or_else(|error| panic!("the holder loads: {error}"));

    let echoed = PathEchoed { config: Some(erased(&stand_in("target"))), attached: None };
    assert_eq!(scenario.child_echo(holder), echoed);

    let absent_child = PathHolderConfig { target: None, child_target: Some(absent()) };
    let error = refused_load(scenario.holder("orphan", &absent_child));
    let names_the_path = error.contains(absent().as_erased().as_str());
    let says_why = error.contains("no route has published");
    assert!(names_the_path && says_why, "the refusal names the child's path and the reason: {error}");
}

/// Catches a `resolve` that mints for a `Dropped` route, so a guest mails a
/// closed actor without a word, and a decode that refuses a closed route,
/// which would answer `Unpublished` and hide the liveness answer from the
/// handler.
///
/// The drop's reply arrives once the guest is released; the close's tail
/// then retires the route on a chain this test never joins, so the attach is
/// re-sent until the holder's `resolve` sees the route gone.
#[test]
fn a_closed_target_proves_at_decode_and_is_refused_not_live_at_resolve() {
    let Some(mut scenario) = Scenario::boot() else {
        return;
    };
    let holder = scenario.bare_holder("holder");
    let host = scenario.harness.actor_ref::<ComponentHostCapability>();
    let target = stand_in("target");
    assert_eq!(scenario.attach(holder, target.clone()), PathAnswer::Ok { path: erased(&target) });

    let not_live = PathAnswer::Err(PathRefused { path: erased(&target), reason: PathRefusal::NotLive });
    let expected = not_live.clone();
    let closed = scenario
        .harness
        .execute(vec![
            ("drop", HarnessOp::send_and_await_reply(&host, &DropComponent { target: erased(&target) })),
            (
                "attach",
                HarnessOp::poll_until(&holder, &PathAttach { target: target.clone() }, move |answer: &PathAnswer| {
                    *answer == expected
                }),
            ),
        ])
        .expect("drop the target, then attach until its route is gone");
    if let DropResult::Err { error } = closed.reply::<DropResult>("drop").expect("decode DropResult") {
        panic!("the target drops: {error}");
    }
    assert_eq!(closed.reply::<PathAnswer>("attach").expect("decode PathAnswer"), not_live);

    let config = PathHolderConfig { target: Some(target.clone()), child_target: None };
    let error = refused_load(scenario.holder("late", &config));
    let names_the_path = error.contains(target.as_erased().as_str());
    let refused_at_resolve = error.contains("no live actor stands at");
    assert!(names_the_path && refused_at_resolve, "the config decoded and `wire` refused the closed path: {error}");
}

/// Catches a republish whose candidate decodes on a context with no routes:
/// its `init` would refuse the republish, its inline child's rebuild would
/// drop the child, or its saved state would drop the attached path, each
/// without a word.
#[test]
fn a_republish_keeps_the_config_path_the_attached_path_and_the_child() {
    let Some(mut scenario) = Scenario::boot() else {
        return;
    };
    let target = stand_in("target");
    let config = PathHolderConfig { target: Some(target.clone()), child_target: Some(target.clone()) };
    let holder = scenario.holder("holder", &config).unwrap_or_else(|error| panic!("the holder loads: {error}"));
    assert_eq!(scenario.attach(holder, target.clone()), PathAnswer::Ok { path: erased(&target) });

    let successor = successor_wasm(&scenario.wasm, 1);
    scenario.harness.publish(successor).unwrap_or_else(|error| panic!("the successor publishes: {error}"));

    let held = PathEchoed { config: Some(erased(&target)), attached: Some(erased(&target)) };
    assert_eq!(scenario.echo(holder), held);
    assert_eq!(scenario.child_echo(holder), PathEchoed { config: Some(erased(&target)), attached: None });
}
