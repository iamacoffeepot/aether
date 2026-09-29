//! Guest births and the held-reply hand-off (ADR-0241 §3, §5, §6;
//! ADR-0243 §9), driven through a booted test chassis: the host is a real
//! root actor dispatched through its slot, it publishes checked-in modules
//! through a staged registry batch, and every birth it stages is decided by
//! the registry owner and completes through the host's `#[handler(task)]`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use aether_actor::{Addressable, HandlesKind, HeldReply, ProtocolRef};
use aether_data::{Blob, BlobHash, INPUTS_SECTION, INPUTS_SECTION_VERSION, InputsRecord, Kind, wire};
use wasmtime::Engine;

use crate::actor::native::spawn::{GuestBirth, GuestOutcome, SpawnError, Subname};
use crate::actor::native::{BlobCheckIn, Held, NativeActor, NativeCtx, NativeInitCtx, Pending, TaskDone};
use crate::actor::wasm::module::{Module, ModuleCache};
use crate::chassis::builder::{PassiveChassis, ReplyTarget};
use crate::chassis::error::BootError;
use crate::mail::MailId;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{RegistryBatch, RegistryBatchResult};
use crate::mail::registry::{OwnedDispatch, Registry, lineage_mailbox_id};
use crate::store::BlobStore;
use crate::testing::{TestChassis, bare_substrate, boot_test_chassis_with, registered_ref};

/// How long a wait that must succeed may take before the test fails.
const PATIENCE: Duration = Duration::from_secs(5);

/// How long a test watches for something that must not happen.
const QUIET: Duration = Duration::from_millis(200);

/// The namespace the fixture module publishes.
const HATCHED: &str = "test.guest.hatched";

/// The namespace a second module publishes, which the first does not hold.
const OTHER: &str = "test.guest.other";

/// What the host saw.
enum Seen {
    Published(RegistryBatchResult),
    Staged(u32),
    Refused { birth: u32, error: SpawnError },
    Born { birth: u32, outcome: GuestOutcome<TestControl> },
    Asked,
}

/// Where and as what one birth lands, by index into the host's list.
#[derive(Clone)]
struct BirthSpec {
    namespace: &'static str,
    module: BlobHash,
    key: Option<&'static str>,
    /// The earlier birth, by index, the guest is born beneath.
    parent: Option<u32>,
}

#[derive(Clone)]
struct HostParams {
    seen: Sender<Seen>,
    modules: Vec<Module>,
    births: Vec<BirthSpec>,
}

/// Publish the host's module at this index.
#[aether_data::kind(name = "test.guest.publish", copy)]
struct Publish {
    module: u32,
}

/// Stage the host's birth at this index.
#[aether_data::kind(name = "test.guest.hatch", copy)]
struct Hatch {
    birth: u32,
}

/// A birth's completion context: which birth it is.
#[aether_data::kind(name = "test.guest.birth_note", copy, partial_eq)]
struct BirthNote {
    birth: u32,
}

/// A publish's completion context.
#[aether_data::kind(name = "test.guest.publish_note", copy)]
struct PublishNote;

/// A request the host holds and hands off to the guest at `birth`.
#[aether_data::kind(name = "test.guest.relay", copy)]
struct Relay {
    birth: u32,
    value: u32,
}

/// Tell the guest at `birth`, through the host, to answer what it parked.
#[aether_data::kind(name = "test.guest.go", copy)]
struct Go {
    birth: u32,
}

/// What a guest is asked through its control rows.
#[aether_data::kind(name = "test.guest.ask", copy)]
struct Ask {
    value: u32,
}

/// Answer the parked ask.
#[aether_data::kind(name = "test.guest.answer", copy)]
struct Answer;

/// The reply a guest answers an [`Ask`] with.
#[aether_data::kind(name = "test.guest.reply", copy, partial_eq)]
struct Reply {
    value: u32,
}

// A sentinel: no test here closes an actor while it still owes a `Reply`.
impl HeldReply for Reply {
    fn unanswered() -> Self {
        Self { value: u32::MAX }
    }
}

/// The rows of [`GuestHosted`] its host controls the guest through.
#[aether_actor::protocol]
trait TestControl {
    fn ask(mail: Ask) -> Reply;
    fn answer(mail: Answer);
}

/// The root that publishes modules and stages guest births, and relays a
/// held request to a guest.
struct GuestHost {
    params: HostParams,
    born: HashMap<u32, ProtocolRef<TestControl>>,
}

#[aether_actor::actor(root)]
impl NativeActor for GuestHost {
    type Config = ();
    type Params = HostParams;
    const NAMESPACE: &'static str = "test.guest.host";

    fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { params, born: HashMap::new() })
    }

    #[aether_actor::handler::single]
    fn on_publish(&mut self, ctx: &mut NativeCtx<'_>, publish: Publish) {
        let batch = RegistryBatch::publish_module(&self.params.modules[publish.module as usize]);
        ctx.stage_registry_batch(batch, PublishNote);
    }

    #[aether_actor::handler(task)]
    fn on_published(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<RegistryBatchResult>) {
        let _ = ctx.take_context::<PublishNote>();
        let _ = self.params.seen.send(Seen::Published(done.into_output()));
    }

    #[aether_actor::handler::single]
    fn on_hatch(&mut self, ctx: &mut NativeCtx<'_>, hatch: Hatch) {
        let spec = self.params.births[hatch.birth as usize].clone();
        let birth = GuestBirth {
            namespace: spec.namespace,
            module: spec.module,
            key: spec.key.map(Subname::Named),
            parent: spec.parent.map(|parent| self.born[&parent].erase()),
        };
        let staged = ctx
            .spawn_guest::<GuestHosted, TestControl>(birth, (), self.params.seen.clone())
            .stage_with(BirthNote { birth: hatch.birth });
        let seen = match staged {
            Ok(_) => Seen::Staged(hatch.birth),
            Err((error, note)) => Seen::Refused { birth: note.birth, error },
        };
        let _ = self.params.seen.send(seen);
    }

    #[aether_actor::handler(task)]
    fn on_born(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<GuestOutcome<TestControl>>) {
        let BirthNote { birth } = ctx.take_context::<BirthNote>().expect("every birth stages with its note");
        let outcome = done.into_output();
        if let Ok(control) = &outcome.result {
            self.born.insert(birth, *control);
        }
        let _ = self.params.seen.send(Seen::Born { birth, outcome });
    }

    #[aether_actor::handler::single]
    fn on_relay(&mut self, ctx: &mut NativeCtx<'_>, relay: Relay) -> Pending<Reply> {
        let (pending, held) = ctx.hold::<Reply>();
        held.hand_off(ctx, self.born[&relay.birth], &Ask { value: relay.value });
        pending
    }

    #[aether_actor::handler::single]
    fn on_go(&mut self, ctx: &mut NativeCtx<'_>, go: Go) {
        ctx.send_to(self.born[&go.birth], &Answer);
    }
}

/// The native host a guest runs in. It answers an [`Ask`] in its own name,
/// holding the reply until it is told to [`Answer`].
struct GuestHosted {
    seen: Sender<Seen>,
    parked: Option<(Held<Reply>, u32)>,
}

#[aether_actor::actor(instanced, root)]
impl NativeActor for GuestHosted {
    type Config = ();
    type Params = Sender<Seen>;
    const NAMESPACE: &'static str = "test.guest.hosted";

    fn init((): (), seen: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { seen, parked: None })
    }

    #[aether_actor::handler::single]
    fn on_ask(&mut self, ctx: &mut NativeCtx<'_>, ask: Ask) -> Pending<Reply> {
        let (pending, held) = ctx.hold::<Reply>();
        self.parked = Some((held, ask.value));
        let _ = self.seen.send(Seen::Asked);
        pending
    }

    #[aether_actor::handler::single]
    fn on_answer(&mut self, ctx: &mut NativeCtx<'_>, _answer: Answer) {
        let (held, value) = self.parked.take().expect("an answer follows an ask");
        held.answer(ctx, &Reply { value });
    }
}

/// A checked-in module exporting each of `namespaces`, with no handlers.
struct Modules {
    cache: ModuleCache,
    blobs: BlobCheckIn,
}

impl Modules {
    fn new() -> Self {
        let blobs = BlobCheckIn::new(BlobStore::new().expect("spawn the reclaim thread"));
        Self { cache: ModuleCache::new(Arc::new(Engine::default())), blobs }
    }

    fn exporting(&self, namespaces: &[&str]) -> Module {
        let section: Vec<u8> = namespaces
            .iter()
            .flat_map(|namespace| {
                let boundary = InputsRecord::ActorBoundary { namespace: (*namespace).to_owned().into() };
                [vec![INPUTS_SECTION_VERSION], wire::to_vec(&boundary).expect("encode a boundary record")].concat()
            })
            .collect();
        let escaped = section.iter().fold(String::new(), |mut escaped, byte| {
            write!(escaped, "\\{byte:02x}").expect("write to a String");
            escaped
        });
        let wat = format!(r#"(module (@custom "{INPUTS_SECTION}" "{escaped}") (func (export "noop")))"#);
        let code = Blob::from(wat::parse_str(wat).expect("parse the fixture WAT"));
        self.cache.check_in(&self.blobs, &code).expect("check the module in")
    }
}

struct Booted {
    chassis: PassiveChassis<TestChassis>,
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    seen: Receiver<Seen>,
    /// Kept alive for the modules the host publishes.
    _modules: Modules,
}

impl Booted {
    fn send<K: Kind>(&self, mail: &K)
    where
        GuestHost: HandlesKind<K>,
    {
        let _ = self.chassis.send_tracked(self.chassis.actor_ref::<GuestHost>(), mail, None);
    }

    fn next(&self) -> Seen {
        self.seen.recv_timeout(PATIENCE).expect("the host reports")
    }

    /// Publish module `module` and wait for its batch to commit.
    fn publish(&self, module: u32) {
        self.send(&Publish { module });
        assert!(matches!(self.next(), Seen::Published(Ok(()))), "the module publishes");
    }

    /// Stage birth `birth` and return its decided outcome.
    fn hatch(&self, birth: u32) -> GuestOutcome<TestControl> {
        self.send(&Hatch { birth });
        assert!(matches!(self.next(), Seen::Staged(staged) if staged == birth), "birth {birth} stages");
        match self.next() {
            Seen::Born { birth: born, outcome } if born == birth => outcome,
            _ => panic!("birth {birth} completes"),
        }
    }
}

/// Boot a host whose first module publishes [`HATCHED`] and whose second
/// publishes [`OTHER`], with `births` built over the first module's hash.
fn boot(births: impl FnOnce(BlobHash) -> Vec<BirthSpec>) -> Booted {
    let (registry, mailer) = bare_substrate();
    let modules = Modules::new();
    let hatched = modules.exporting(&[HATCHED]);
    let births = births(hatched.hash());
    let other = modules.exporting(&[OTHER]);
    let (seen_tx, seen) = mpsc::channel();
    let params = HostParams { seen: seen_tx, modules: vec![hatched, other], births };
    let chassis = boot_test_chassis_with::<GuestHost>(&registry, &mailer, (), params);
    Booted { chassis, registry, mailer, seen, _modules: modules }
}

fn spec(namespace: &'static str, module: BlobHash, key: Option<&'static str>, parent: Option<u32>) -> BirthSpec {
    BirthSpec { namespace, module, key, parent }
}

/// Catches a guest birth that folds the host type's namespace instead of the
/// published one, and a rendered name that disagrees with the folded id: the
/// registry then holds no `Live` route under the outcome's name at the
/// reference's position.
#[test]
fn a_guest_lands_at_its_published_name() {
    let booted = boot(|module| {
        vec![
            spec(HATCHED, module, None, None),
            spec(HATCHED, module, Some("k"), None),
            spec(HATCHED, module, Some("k"), Some(0)),
        ]
    });
    booted.publish(0);

    for (birth, name) in [(0, HATCHED.to_owned()), (1, format!("{HATCHED}:k")), (2, format!("{HATCHED}/{HATCHED}:k"))] {
        let outcome = booted.hatch(birth);
        assert_eq!(outcome.canonical_name.as_str(), name);
        let control = outcome.result.unwrap_or_else(|error| panic!("birth {birth} is refused: {error:?}"));
        assert_eq!(
            booted.registry.live_route(&outcome.canonical_name),
            Some(control.erase().id()),
            "{name} is Live at the position its reference proves",
        );
    }
    drop(booted);
}

/// Catches an owner that lets a host birth code the publication table does
/// not bind at that namespace, and a refusal that strands a `Starting`
/// route.
#[test]
fn a_guest_birth_the_table_does_not_bind_is_refused_and_leaves_no_route() {
    let booted = boot(|module| {
        vec![
            spec("test.guest.unpublished", module, None, None),
            spec(GuestHost::NAMESPACE, module, Some("k"), None),
            spec(OTHER, module, None, None),
        ]
    });
    booted.publish(0);
    booted.publish(1);

    for (birth, namespace, name) in [
        (0, "test.guest.unpublished", "test.guest.unpublished".to_owned()),
        (1, GuestHost::NAMESPACE, format!("{}:k", GuestHost::NAMESPACE)),
        (2, OTHER, OTHER.to_owned()),
    ] {
        let outcome = booted.hatch(birth);
        assert_eq!(outcome.canonical_name.as_str(), name);
        assert!(
            matches!(&outcome.result, Err(SpawnError::GuestNotPublished { namespace: refused }) if refused == namespace),
            "birth {birth} is refused as unpublished: {:?}",
            outcome.result,
        );
        assert_eq!(booted.registry.mailbox_name(lineage_mailbox_id(&name)), None, "{name} leaves no route");
    }
    drop(booted);
}

/// Catches a `parent/NS` birth slipping in ahead of #6822: a keyless child
/// must be refused at staging and hand its context back.
#[test]
fn a_keyless_child_is_refused_at_staging() {
    let booted = boot(|module| vec![spec(HATCHED, module, None, None), spec(HATCHED, module, None, Some(0))]);
    booted.publish(0);
    assert!(booted.hatch(0).result.is_ok(), "the parent is born");

    booted.send(&Hatch { birth: 1 });
    assert!(
        matches!(booted.next(), Seen::Refused { birth: 1, error: SpawnError::GuestPlacement }),
        "the keyless child is refused with its note handed back",
    );
    drop(booted);
}

/// Catches a hand-off that releases the hold before its push is counted,
/// drops the requester's correlation, or stamps the host as the replier:
/// the chain would settle while the guest still owes the reply, the
/// requester could not match the reply, or it would keep the wrong sender.
#[test]
fn a_handed_off_reply_is_answered_by_the_guest_in_its_own_name() {
    let booted = boot(|module| vec![spec(HATCHED, module, Some("control"), None)]);
    booted.publish(0);
    let control = booted.hatch(0).result.expect("the guest is born");

    let (reply_tx, replies) = mpsc::channel::<OwnedDispatch>();
    let sink_mailer = Arc::clone(&booted.mailer);
    let requester = registered_ref(
        &booted.registry,
        "test.guest.requester",
        Arc::new(move |dispatch: OwnedDispatch| {
            // The requester is the reply's terminal consumer: it finishes the
            // reply so the chain it joined can settle.
            sink_mailer.record_finished(dispatch.mail_id, dispatch.root);
            dispatch.discharge();
            let _ = reply_tx.send(dispatch);
        }),
    );
    let (root, settled): (MailId, _) = booted.chassis.send_tracked(
        booted.chassis.actor_ref::<GuestHost>(),
        &Relay { birth: 0, value: 9 },
        Some(ReplyTarget::Actor { to: requester, correlation: 77 }),
    );
    assert!(matches!(booted.next(), Seen::Asked), "the guest receives the handed-off ask");

    let counter = booted.mailer.trace_handle().settlement_counter();
    assert_eq!(counter.held_open(root), 1, "the guest's held reply keeps the requester's chain open");
    assert!(settled.recv_timeout(QUIET).is_err(), "the chain stays open until the guest answers");
    assert!(replies.recv_timeout(QUIET).is_err(), "nothing answers the requester before the guest does");

    booted.send(&Go { birth: 0 });
    let reply = replies.recv_timeout(PATIENCE).expect("the guest answers the requester");
    assert_eq!(Reply::decode_from_bytes(reply.payload.bytes()).expect("a Reply"), Reply { value: 9 });
    assert_eq!(reply.sender.correlation_id, 77, "the requester's correlation is echoed");
    assert_eq!(
        reply.mail_id.map(|mail_id| mail_id.sender),
        Some(control.erase().id()),
        "the guest replies in its own name",
    );
    settled.recv_timeout(PATIENCE).expect("the chain settles once the guest answers");
    drop(booted);
}
