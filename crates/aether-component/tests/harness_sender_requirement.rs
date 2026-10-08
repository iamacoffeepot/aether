//! Issues 7532 and 7545: a handler whose ctx names a protocol `P` as its
//! sender runs only for a sender the engine casts to `P` first, and reads
//! that proven reference from `ctx.sender()` (ADR-0231 §11), driven through real
//! dispatch against a native receiver and a guest one.
//!
//! Each receiver is a gate: its [`SenderGateTake`] tell and its [`SenderGateDial`]
//! request require [`SenderGateGrantee`] of their sender, count their runs, and mail
//! the sender they were handed a [`SenderGateGranted`]. The native gate and its
//! covering holder are composed here; the guest pair is the bundle's
//! `SenderGate` and `SenderGateHolder`. Both arms come out of `#[actor]`, the
//! native one calling the native ctx's cast helper and the guest one the
//! SDK's, whose refused tell reaches the host as a dispatch code.
//!
//! A covering holder sends with the plain `ctx.send`, which is the only way
//! typed code reaches a gate. The routes the build cannot see are driven the
//! way they arise: [`Relay`] proves the bytes through the boundary
//! (`accept_call`) and forwards them from a turn it sent itself, as
//! `aether.rpc.server` relays a wire `Call`, so the relay is the mail's
//! sender and reply target; and the harness pushes mail with no sender.
//! [`Relay`] declares a handler for the refusal notice, as the RPC server
//! does, and has none for `SenderGateGranted`.
//!
//! No scenario waits on a clock: each sends one mail and waits on its chain's
//! settlement, and a refusal is answered inside that chain.
//!
//! The guest scenarios are skipped when the fixture wasm hasn't been built
//! (`require_wasm`), and only under `AETHER_ALLOW_WASM_SKIP=1`.

use std::fs;
use std::sync::mpsc::{self, Receiver, Sender};

use aether_actor::{
    ActorRef, Addressable, Anyone, HandlesKind, PathRefusal, PathRefused, ProtocolRef, Unchecked, Undeclared, actor,
};
use aether_data::{ErasedActorPath, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DecodeRefused, LoadComponent};
use aether_substrate::BootError;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_test_fixtures_bundle::{SenderGate, SenderGateHolder};
use aether_test_fixtures_kinds::{
    SenderGateDial, SenderGateDialed, SenderGateGranted, SenderGateGrantee, SenderGateHolderQuery,
    SenderGateHolderQueryResult, SenderGateQuery, SenderGateQueryResult, SenderGateTake, SenderGateTrigger,
};

/// The native gate: the bundle's `SenderGate`, handler for handler.
#[derive(Default)]
struct NativeGate {
    takes: u32,
    dials: u32,
}

#[actor(singleton, root)]
impl NativeActor for NativeGate {
    const NAMESPACE: &'static str = "test.sender_requirement.gate";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self::default())
    }

    #[handler::tell]
    fn on_take(&mut self, ctx: &mut NativeCtx<'_, Self, SenderGateGrantee>, take: SenderGateTake) {
        self.takes += 1;
        ctx.send_to(ctx.sender(), &SenderGateGranted { tag: take.tag });
    }

    #[handler::request]
    fn on_dial(&mut self, ctx: &mut NativeCtx<'_, Self, SenderGateGrantee>, dial: SenderGateDial) -> SenderGateDialed {
        self.dials += 1;
        ctx.send_to(ctx.sender(), &SenderGateGranted { tag: dial.tag });

        SenderGateDialed::Ok { tag: dial.tag }
    }

    #[handler::request]
    fn on_query(&mut self, _ctx: &mut NativeCtx<'_>, _query: SenderGateQuery) -> SenderGateQueryResult {
        SenderGateQueryResult { takes: self.takes, dials: self.dials }
    }
}

/// The native holder: it declares the gate and covers [`SenderGateGrantee`], so its
/// plain `ctx.send::<NativeGate>` of either kind builds.
#[derive(Default)]
struct NativeHolder {
    granted: Vec<u32>,
    dialed: Vec<SenderGateDialed>,
}

#[actor(singleton, root, depends(NativeGate))]
impl NativeActor for NativeHolder {
    const NAMESPACE: &'static str = "test.sender_requirement.holder";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self::default())
    }

    #[handler::tell]
    fn on_trigger(&mut self, ctx: &mut NativeCtx<'_>, trigger: SenderGateTrigger) {
        ctx.send::<NativeGate>(&SenderGateTake { tag: trigger.tag });
        ctx.send::<NativeGate>(&SenderGateDial { tag: trigger.tag });
    }

    #[handler::tell]
    fn on_granted(&mut self, _ctx: &mut NativeCtx<'_>, granted: SenderGateGranted) {
        self.granted.push(granted.tag);
    }

    #[handler::response]
    fn on_dialed(&mut self, _ctx: &mut NativeCtx<'_>, dialed: SenderGateDialed) {
        self.dialed.push(dialed);
    }

    #[handler::request]
    fn on_query(&mut self, _ctx: &mut NativeCtx<'_>, _query: SenderGateHolderQuery) -> SenderGateHolderQueryResult {
        SenderGateHolderQueryResult { granted: self.granted.clone(), dialed: self.dialed.clone() }
    }
}

/// Tells the [`Relay`] to relay a take, or a dial, to the gate at `gate`.
#[aether_data::kind(name = "test.sender_requirement.relay", no_serde)]
struct RelayToGate {
    gate: ErasedActorPath,
    dial: bool,
}

/// What the [`Relay`] sends itself, so the turn that relays has the relay as
/// its sender and reply target.
#[aether_data::kind(name = "test.sender_requirement.forward", copy, no_serde)]
struct Forward;

/// The [`Relay`]'s own forwarding row.
#[aether_actor::protocol]
trait Forwarding {
    fn forward(mail: Forward) -> Undeclared;
}

/// What the [`Relay`] hears back from a gate.
#[derive(Debug)]
enum Heard {
    Answer(SenderGateDialed),
    Refused(DecodeRefused),
}

/// The tag every relayed mail carries.
const RELAYED_TAG: u32 = 9;

/// Relays a gate kind the way `aether.rpc.server` relays a wire `Call`, and
/// records every answer it hears. It opts into the refusal notice and does
/// not cover [`SenderGateGrantee`].
struct Relay {
    heard: Sender<Heard>,
    me: Option<ProtocolRef<Forwarding>>,
    relaying: Option<RelayToGate>,
}

#[actor(singleton, root)]
impl NativeActor for Relay {
    const NAMESPACE: &'static str = "test.sender_requirement.relay";
    type Config = ();
    type Params = Sender<Heard>;

    fn init((): (), heard: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { heard, me: None, relaying: None })
    }

    fn wire(state: &mut Self, ctx: &mut NativeCtx<'_>) -> Result<(), BootError> {
        let me = ctx.resolve_path(&ErasedActorPath::new(Self::NAMESPACE).expect("a canonical path"));
        state.me = me.ok().and_then(|me| ctx.cast(me));
        Ok(())
    }

    #[handler::tell]
    fn on_relay(&mut self, ctx: &mut NativeCtx<'_>, relay: RelayToGate) {
        self.relaying = Some(relay);
        ctx.send_to(self.me.expect("the relay cast itself at wire"), &Forward);
    }

    #[handler::unchecked(reason = "test: relays a gate kind, reply target pinned to this relay")]
    fn on_forward(&mut self, ctx: &mut NativeCtx<'_, Self, Anyone, Unchecked>, _forward: Forward) {
        let RelayToGate { gate, dial } = self.relaying.take().expect("a relay was asked for");
        let (kind, bytes) = if dial {
            (<SenderGateDial as Kind>::ID, SenderGateDial { tag: RELAYED_TAG }.encode_into_bytes())
        } else {
            (<SenderGateTake as Kind>::ID, SenderGateTake { tag: RELAYED_TAG }.encode_into_bytes())
        };

        ctx.deliver_forwarded(ctx.accept_call(&gate, kind, bytes).expect("the gate is live"));
    }

    #[handler::response]
    fn on_dialed(&mut self, _ctx: &mut NativeCtx<'_>, dialed: SenderGateDialed) {
        self.heard.send(Heard::Answer(dialed)).expect("heard receiver stays live");
    }

    #[handler::tell]
    fn on_decode_refused(&mut self, _ctx: &mut NativeCtx<'_>, notice: DecodeRefused) {
        self.heard.send(Heard::Refused(notice)).expect("heard receiver stays live");
    }
}

/// A gate `G`, its covering holder `H`, and the [`Relay`], on one harness.
struct Scenario<G, H> {
    harness: SubstrateHarness,
    heard: Receiver<Heard>,
    gate: ActorRef<G>,
    holder: ActorRef<H>,
}

/// The native gate and holder, composed beside the relay.
fn native() -> Scenario<NativeGate, NativeHolder> {
    let (heard_tx, heard) = mpsc::channel();
    let harness = SubstrateHarness::builder()
        .with_actor::<NativeGate>(())
        .with_actor::<NativeHolder>(())
        .with_actor::<Relay>(heard_tx)
        .build()
        .expect("the native gate, its holder, and the relay boot");
    let gate = harness.actor_ref::<NativeGate>();
    let holder = harness.actor_ref::<NativeHolder>();

    Scenario { harness, heard, gate, holder }
}

/// The bundle's gate and holder, loaded beside the relay; the holder loads
/// second, since it declares the gate.
fn guest() -> Option<Scenario<SenderGate, SenderGateHolder>> {
    let wasm = fs::read(require_wasm("aether_test_fixtures_bundle")?).expect("read fixture wasm");
    let (heard_tx, heard) = mpsc::channel();
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<Relay>(heard_tx)
        .build()
        .expect("the component host and the relay boot");
    let load = || LoadComponent { wasm: wasm.clone(), name: None, config: Vec::new(), export: None };
    let gate = harness.load::<SenderGate>(load()).unwrap_or_else(|error| panic!("the gate loads: {error}"));
    let holder = harness.load::<SenderGateHolder>(load()).unwrap_or_else(|error| panic!("the holder loads: {error}"));

    Some(Scenario { harness, heard, gate, holder })
}

impl<G, H> Scenario<G, H>
where
    G: Addressable + HandlesKind<SenderGateTake> + HandlesKind<SenderGateDial> + HandlesKind<SenderGateQuery> + 'static,
    H: HandlesKind<SenderGateTrigger> + HandlesKind<SenderGateHolderQuery> + 'static,
{
    /// How many times each of the gate's two handlers has run.
    fn gate_report(&mut self) -> SenderGateQueryResult {
        self.harness
            .execute(vec![("query", HarnessOp::send_and_await_reply(&self.gate, &SenderGateQuery))])
            .expect("the gate answers its query")
            .reply("query")
            .expect("decode the gate's report")
    }

    /// Have the relay relay a take, or a dial, to the gate, wait for the
    /// chain to settle, and answer everything the relay heard.
    fn relay(&mut self, dial: bool) -> Vec<Heard> {
        let relay = self.harness.actor_ref::<Relay>();
        let mail = RelayToGate { gate: self.harness.actor_path(&self.gate), dial };
        self.harness.execute(vec![("relay", HarnessOp::send_and_settle(&relay, &mail))]).expect("the relay settles");

        self.heard.try_iter().collect()
    }

    /// The covering holder's plain sends run both handlers, and each handler
    /// reaches the holder through the reference it was handed. Fails if the
    /// arm hands over a reference to anyone but the sender, or refuses a
    /// sender that covers the protocol.
    fn a_covering_sender_runs_the_handlers_and_hears_back(mut self) {
        self.harness
            .execute(vec![("trigger", HarnessOp::send_and_settle(&self.holder, &SenderGateTrigger { tag: 7 }))])
            .expect("the holder's sends settle");

        let heard: SenderGateHolderQueryResult = self
            .harness
            .execute(vec![("query", HarnessOp::send_and_await_reply(&self.holder, &SenderGateHolderQuery))])
            .expect("the holder answers its query")
            .reply("query")
            .expect("decode the holder's report");
        let dialed = vec![SenderGateDialed::Ok { tag: 7 }];
        assert_eq!(heard, SenderGateHolderQueryResult { granted: vec![7, 7], dialed });
        assert_eq!(self.gate_report(), SenderGateQueryResult { takes: 1, dials: 1 });
    }

    /// A tell relayed by an actor that does not cover the protocol never
    /// runs the handler, and the relay, which opted in, hears one refusal
    /// notice naming the kind before the chain settles. Fails if the handler
    /// runs anyway, or the refusal is dropped and a relayed caller is told
    /// nothing.
    fn a_relayed_tell_is_refused_and_the_relay_is_told(mut self) {
        let heard = self.relay(false);

        assert!(
            matches!(heard.as_slice(), [Heard::Refused(notice)] if notice.kind == <SenderGateTake as Kind>::ID),
            "one notice naming the take: {heard:?}",
        );
        assert_eq!(self.gate_report(), SenderGateQueryResult { takes: 0, dials: 0 });
    }

    /// A request relayed by an actor that does not cover the protocol never
    /// runs the handler and is answered once, with its own reply naming the
    /// relay and the handler it lacks, and no notice. Fails if the request is
    /// left unanswered, answered twice, or answered with a notice in place of
    /// its reply.
    fn a_relayed_request_is_answered_with_the_typed_refusal(mut self) {
        let heard = self.relay(true);

        let relay = ErasedActorPath::new(Relay::NAMESPACE).expect("a canonical path");
        let lacked = PathRefusal::Uncovered { kind: <SenderGateGranted as Kind>::ID };
        let refused = SenderGateDialed::Err(PathRefused { path: relay, reason: lacked });
        assert!(
            matches!(heard.as_slice(), [Heard::Answer(answer)] if *answer == refused),
            "one reply naming the relay and the row it lacks: {heard:?}",
        );
        assert_eq!(self.gate_report(), SenderGateQueryResult { takes: 0, dials: 0 });
    }

    /// Mail with no sender never runs either handler: the harness pushes a
    /// take and a dial, which carry no actor sender. Fails if the arm hands
    /// the handler a reference minted from nothing.
    fn mail_with_no_sender_never_runs_a_handler(mut self) {
        self.harness
            .execute(vec![
                ("take", HarnessOp::send_and_settle(&self.gate, &SenderGateTake { tag: 3 })),
                ("dial", HarnessOp::send_and_settle(&self.gate, &SenderGateDial { tag: 3 })),
            ])
            .expect("the pushed mail settles");

        assert_eq!(self.gate_report(), SenderGateQueryResult { takes: 0, dials: 0 });
    }
}

#[test]
fn a_native_gate_runs_for_a_covering_sender_and_hands_it_the_reference() {
    native().a_covering_sender_runs_the_handlers_and_hears_back();
}

#[test]
fn a_native_gate_refuses_a_relayed_tell_and_tells_the_relay() {
    native().a_relayed_tell_is_refused_and_the_relay_is_told();
}

#[test]
fn a_native_gate_answers_a_relayed_request_with_the_typed_refusal() {
    native().a_relayed_request_is_answered_with_the_typed_refusal();
}

#[test]
fn a_native_gate_never_runs_for_mail_with_no_sender() {
    native().mail_with_no_sender_never_runs_a_handler();
}

#[test]
fn a_guest_gate_runs_for_a_covering_sender_and_hands_it_the_reference() {
    let Some(scenario) = guest() else {
        return;
    };

    scenario.a_covering_sender_runs_the_handlers_and_hears_back();
}

#[test]
fn a_guest_gate_refuses_a_relayed_tell_and_tells_the_relay() {
    let Some(scenario) = guest() else {
        return;
    };

    scenario.a_relayed_tell_is_refused_and_the_relay_is_told();
}

#[test]
fn a_guest_gate_answers_a_relayed_request_with_the_typed_refusal() {
    let Some(scenario) = guest() else {
        return;
    };

    scenario.a_relayed_request_is_answered_with_the_typed_refusal();
}

#[test]
fn a_guest_gate_never_runs_for_mail_with_no_sender() {
    let Some(scenario) = guest() else {
        return;
    };

    scenario.mail_with_no_sender_never_runs_a_handler();
}
