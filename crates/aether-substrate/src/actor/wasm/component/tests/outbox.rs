//! A candidate guest's held outbox (#7067), driven the way a republish will
//! drive it. A pumped root [`Host`] hosts an old guest and prepares a
//! replacement over it: it previews the candidate module's admission, then
//! builds the candidate with its outbox held, dehydrates the old guest into
//! it, and hands it the reply table. A later turn commits the candidate,
//! flushing what it held, or aborts it, discarding what it held and giving
//! the reply table back.
//!
//! The old guest holds its reply to a [`Hold`] request (ADR-0243 §6) and
//! saves the handle when it dehydrates. The candidate sends a [`Probe`] to
//! its host from `init` and answers the saved handle from `on_rehydrate`,
//! so a candidate that leaked either would reach the host or the session
//! before its commit.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use aether_actor::{Addressable, DISPATCH_HANDLED_HOLD, DISPATCH_HANDLED_RELEASE, HandlesKind, ReplyMode};
use aether_data::{Blob, INPUTS_SECTION, INPUTS_SECTION_VERSION, InputsRecord, Kind, MailId, SessionToken, Uuid, wire};
use wasmtime::{Engine, Linker};

use super::WAT_REALLOC;
use crate::actor::native::BlobCheckIn;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::wasm::component::{Component, ComponentCtx};
use crate::actor::wasm::host_fns;
use crate::actor::wasm::module::{Module, ModuleCache};
use crate::chassis::builder::ReplyTarget;
use crate::mail::outbound::{EgressEvent, HubOutbound};
use crate::mail::registry::AdmissionRefusal;
use crate::store::BlobStore;
use crate::testing::{PumpedDriver, boot_bare_test_chassis, fresh_substrate_and_rx};
use crate::{BootError, NativeActor, NativeInitCtx};

/// A request the old guest keeps its reply to.
#[aether_data::kind(name = "test.outbox.hold")]
struct Hold;

/// A request on which the old guest answers the reply it kept.
#[aether_data::kind(name = "test.outbox.answer")]
struct Answer;

/// The answer every guest gives a kept [`Hold`].
#[aether_data::kind(name = "test.outbox.answered")]
struct Answered;

/// What a candidate sends its host from `init`.
#[aether_data::kind(name = "test.outbox.probe")]
struct Probe;

/// Prepare a candidate over the old guest.
#[aether_data::kind(name = "test.outbox.prepare")]
struct Prepare;

/// Commit the prepared candidate.
#[aether_data::kind(name = "test.outbox.commit")]
struct Commit;

/// Abort the prepared candidate, reinstating the old guest.
#[aether_data::kind(name = "test.outbox.abort")]
struct Abort;

/// The correlation the session's [`Hold`] request carries.
const HOLD_CORRELATION: u64 = 0x5151;

/// The code the host runs its guests from, and the outbound their session
/// answers leave through.
struct Guests {
    engine: Arc<Engine>,
    linker: Linker<ComponentCtx>,
    old: Module,
    candidate: Module,
    outbound: Arc<HubOutbound>,
}

/// A pumped root that hosts an old guest and prepares a candidate over it.
struct Host {
    guests: Guests,
    old: Component,
    candidate: Option<Component>,
    refused: Option<AdmissionRefusal>,
    /// How many [`Hold`]s the old guest has been delivered.
    holds: u32,
    /// The root of each [`Probe`] that reached the host.
    probes: Vec<Option<MailId>>,
}

impl Host {
    fn instantiate(&self, module: &Module, ctx: ComponentCtx) -> Component {
        Component::instantiate(&self.guests.engine, &self.guests.linker, module.compiled(), ctx, &[], None)
            .expect("the guest instantiates")
    }

    /// Deliver the turn's inbound to the old guest, as the trampoline's
    /// fallback does.
    fn deliver<A, S, M: ReplyMode>(&mut self, ctx: &NativeCtx<'_, A, S, M>) {
        self.old.deliver(ctx.inbound().expect("a handler turn has its inbound")).expect("the old guest receives");
    }
}

#[aether_actor::actor(singleton, root)]
impl NativeActor for Host {
    const NAMESPACE: &'static str = "test.outbox.host";
    type Config = ();
    type Params = Guests;

    fn init((): (), guests: Guests, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let old = Component::instantiate(
            &guests.engine,
            &guests.linker,
            guests.old.compiled(),
            ctx.guest_ctx(Arc::clone(&guests.outbound)),
            &[],
            None,
        )
        .expect("the old guest instantiates");

        Ok(Self { guests, old, candidate: None, refused: None, holds: 0, probes: Vec::new() })
    }

    #[handler::tell]
    fn on_hold(&mut self, ctx: &mut NativeCtx<'_>, _hold: Hold) {
        self.deliver(ctx);
        self.holds += 1;
    }

    #[handler::response]
    fn on_answer(&mut self, ctx: &mut NativeCtx<'_>, _answer: Answer) {
        self.deliver(ctx);
    }

    /// A republish's prepare: refused before any guest is touched when the
    /// module would not be admitted, else the candidate is built held and
    /// takes the old guest's state and reply table.
    #[handler::tell]
    fn on_prepare(&mut self, ctx: &mut NativeCtx<'_>, _prepare: Prepare) {
        if let Err(refusal) = ctx.admission_preview(&self.guests.candidate) {
            self.refused = Some(refusal);
            return;
        }

        let mut guest_ctx = ctx.guest_ctx(Arc::clone(&self.guests.outbound));
        guest_ctx.hold_outbox();
        let mut candidate = self.instantiate(&self.guests.candidate, guest_ctx);

        self.old.on_dehydrate().expect("the old guest dehydrates");
        let bundle = self.old.take_saved_state().expect("the old guest saves its held handle");
        candidate.resume_correlations(self.old.correlation_cursor());
        candidate.resume_replies(self.old.take_pending_replies());
        candidate.call_on_rehydrate(&bundle).expect("the candidate rehydrates");

        self.candidate = Some(candidate);
    }

    #[handler::tell]
    fn on_commit(&mut self, ctx: &mut NativeCtx<'_>, _commit: Commit) {
        let mut candidate = self.candidate.take().expect("a candidate is prepared");
        candidate.flush_held_outbox(ctx);
        self.old = candidate;
    }

    #[handler::tell]
    fn on_abort(&mut self, _ctx: &mut NativeCtx<'_>, _abort: Abort) {
        let mut candidate = self.candidate.take().expect("a candidate is prepared");
        candidate.discard_held_outbox();
        self.old.resume_replies(candidate.take_pending_replies());
        self.old.resume_correlations(candidate.correlation_cursor());
    }

    #[handler::tell]
    fn on_probe(&mut self, ctx: &mut NativeCtx<'_>, _probe: Probe) {
        self.probes.push(ctx.in_flight_root());
    }
}

/// The old guest: it keeps its [`Hold`] reply handle at offset 500,
/// registering [`Answered`] as its unanswered reply, answers it with
/// [`Answered`] on [`Answer`], and saves it when it dehydrates.
fn old_guest_wat() -> String {
    let (hold, answered) = (Hold::ID.0, Answered::ID.0);
    format!(
        r#"
        (module
            (import "aether" "reply_mail_p32"
                (func $reply_mail (param i32 i64 i32 i32 i32 i64) (result i32)))
            (import "aether" "held_unanswered_p32"
                (func $held_unanswered (param i32 i64 i32 i32) (result i32)))
            (import "aether" "save_state_p32" (func $save_state (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                (if (i64.eq (local.get 0) (i64.const {hold}))
                    (then
                        (i32.store (i32.const 500) (local.get 4))
                        (drop (call $held_unanswered
                            (local.get 4) (i64.const {answered}) (i32.const 0) (i32.const 0)))
                        (return (i32.const {DISPATCH_HANDLED_HOLD}))))
                (drop (call $reply_mail
                    (i32.load (i32.const 500)) (i64.const {answered}) (i32.const 0) (i32.const 0) (i32.const 1)
                    (i64.const 0)))
                i32.const {DISPATCH_HANDLED_RELEASE})
            (func (export "on_dehydrate") (result i32)
                (drop (call $save_state (i32.const 1) (i32.const 500) (i32.const 4)))
                i32.const 0))
        "#
    )
}

/// The candidate, exporting `namespace`: it sends its host a [`Probe`] from
/// `init` and answers the handle its predecessor saved from `on_rehydrate`.
fn candidate_wat(namespace: &str) -> String {
    let (probe, answered) = (Probe::ID.0, Answered::ID.0);
    let boundary = InputsRecord::ActorBoundary { namespace: namespace.to_owned().into() };
    let section = [vec![INPUTS_SECTION_VERSION], wire::to_vec(&boundary).expect("encode a boundary record")].concat();
    let escaped = section.iter().fold(String::new(), |mut escaped, byte| {
        write!(escaped, "\\{byte:02x}").expect("write to a String");
        escaped
    });
    format!(
        r#"
        (module
            (@custom "{INPUTS_SECTION}" "{escaped}")
            (import "aether" "send_mail_p32"
                (func $send (param i64 i64 i32 i32 i32 i32 i64) (result i32)))
            (import "aether" "reply_mail_p32"
                (func $reply_mail (param i32 i64 i32 i32 i32 i64) (result i32)))
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const {DISPATCH_HANDLED_RELEASE})
            (func (export "init") (param i64) (result i32)
                (drop (call $send
                    (local.get 0) (i64.const {probe}) (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 0)
                    (i64.const 0)))
                i32.const 0)
            (func (export "on_rehydrate_p32") (param i32 i32 i32) (result i32)
                (drop (call $reply_mail
                    (i32.load (local.get 1)) (i64.const {answered}) (i32.const 0) (i32.const 0) (i32.const 1)
                    (i64.const 0)))
                i32.const 0))
        "#
    )
}

/// A [`Host`] booted pumped under its driver, with the egress its guests'
/// session answers reach.
struct Rig {
    driver: PumpedDriver<Host>,
    egress: Receiver<EgressEvent>,
}

impl Rig {
    /// A host whose candidate exports `namespace`.
    fn boot(namespace: &str) -> Self {
        let (registry, mailer, egress) = fresh_substrate_and_rx();
        let engine = Arc::new(Engine::default());
        let mut linker = Linker::new(&engine);
        host_fns::register(&mut linker).expect("register host fns");
        let (modules, blobs) = (
            ModuleCache::new(Arc::clone(&engine)),
            BlobCheckIn::new(BlobStore::new().expect("spawn the reclaim thread")),
        );
        let check_in = |wat: String| {
            let code = Blob::from(wat::parse_str(wat).expect("parse the fixture WAT"));
            modules.check_in(&blobs, &code).expect("check the module in")
        };
        let guests = Guests {
            old: check_in(old_guest_wat()),
            candidate: check_in(candidate_wat(namespace)),
            outbound: Arc::clone(mailer.outbound().expect("the fixture mailer has a loopback outbound")),
            engine,
            linker,
        };

        Self { driver: PumpedDriver::boot(boot_bare_test_chassis(&registry, &mailer), (), guests), egress }
    }

    /// Push a session [`Hold`] the old guest keeps its reply to, and settle
    /// the turn that delivers it; its root stays open until the answer.
    fn hold(&mut self) -> MailId {
        let session =
            ReplyTarget::Session { session: SessionToken(Uuid::from_u128(0x7067)), correlation: HOLD_CORRELATION };
        let host = self.driver.chassis().actor_ref::<Host>();
        let root = self.driver.send_tracked(host, &Hold, Some(session));
        self.driver.pump_until("the old guest keeps its reply", |host| host.holds == 1);
        root
    }

    fn send<K: Kind>(&mut self, mail: &K) -> MailId
    where
        Host: HandlesKind<K>,
    {
        let host = self.driver.chassis().actor_ref::<Host>();
        self.driver.send_and_settle(host, mail, None)
    }

    /// Whether `root`'s chain is still open.
    fn open(&self, root: MailId) -> bool {
        self.driver.chassis().settlement_registry().subscribe_settlement(root).try_recv().is_err()
    }

    /// The correlation of every [`Answered`] the session has been sent.
    fn answers(&self) -> Vec<u64> {
        self.egress
            .try_iter()
            .filter_map(|event| match event {
                EgressEvent::ToSession { kind_name, correlation_id, .. } if kind_name == Answered::NAME => {
                    Some(correlation_id)
                }
                _ => None,
            })
            .collect()
    }

    fn probes(&self) -> Vec<Option<MailId>> {
        self.driver.read_state(|host| host.probes.clone()).expect("the host is live")
    }
}

/// A namespace no module or native actor publishes.
const CANDIDATE: &str = "test.outbox.candidate";

// Catches: a held candidate's `init` send or `on_rehydrate` reply leaving
// before its commit; a flushed send stamped at hold time, or not at all, so
// the commit's chain settles before the mail it caused; and a flushed reply
// that never releases its requester's hold.
#[test]
fn a_held_candidate_sends_nothing_until_commit_flushes_on_its_chain() {
    let mut rig = Rig::boot(CANDIDATE);
    let held = rig.hold();

    rig.send(&Prepare);

    assert!(rig.probes().is_empty(), "the candidate's init send is held");
    assert!(rig.answers().is_empty(), "the candidate's answer is held");
    assert!(rig.open(held));

    let commit = rig.send(&Commit);
    rig.driver.settle(&[held]);

    assert_eq!(rig.probes(), [Some(commit)], "the held send landed on the commit's chain before it settled");
    assert_eq!(rig.answers(), [HOLD_CORRELATION]);
    assert!(!rig.open(held));
}

// Catches: a discard that sends what was held, or that frees or drops the
// reserved slot, so the reinstated old guest's answer is refused as an
// unknown handle and its requester's chain never settles.
#[test]
fn a_discarded_candidate_restores_the_held_slot_to_the_old_guest() {
    let mut rig = Rig::boot(CANDIDATE);
    let held = rig.hold();
    rig.send(&Prepare);

    rig.send(&Abort);

    assert!(rig.probes().is_empty());
    assert!(rig.answers().is_empty());
    assert!(rig.open(held), "the restored slot still holds its requester's chain");

    rig.send(&Answer);
    rig.driver.settle(&[held]);

    assert_eq!(rig.answers(), [HOLD_CORRELATION], "the old guest answers on the restored slot");
    assert!(!rig.open(held));
}

// Catches: a preview that misses a refusal the publish would make, so a
// republish dehydrates and replaces its guests before being refused.
#[test]
fn a_refused_preview_touches_no_guest() {
    let mut rig = Rig::boot(Host::NAMESPACE);
    let held = rig.hold();

    rig.send(&Prepare);

    let refused = rig.driver.read_state(|host| (host.refused.clone(), host.candidate.is_some())).expect("live");
    assert!(
        matches!(refused, (Some(AdmissionRefusal::NativeNamespace { ref namespace }), false) if &**namespace == Host::NAMESPACE),
        "{refused:?}",
    );

    rig.send(&Answer);
    rig.driver.settle(&[held]);

    assert_eq!(rig.answers(), [HOLD_CORRELATION], "the old guest kept its reply table");
}

// Catches: a held outbox dropped with its mail in it passing silently, so
// a republish that forgets to flush or discard loses the candidate's mail
// and strands the reserved slot's requester.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "held guest outbox dropped")]
fn dropping_an_unflushed_outbox_surfaces_the_leak() {
    let mut rig = Rig::boot(CANDIDATE);
    rig.hold();
    rig.send(&Prepare);

    let candidate = rig.driver.host_turn(|host, _| host.candidate.take()).flatten();

    drop(candidate);
}
