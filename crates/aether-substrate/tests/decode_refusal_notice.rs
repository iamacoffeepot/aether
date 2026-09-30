//! A native recipient that refuses a request's payload at decode answers the
//! reply target a `DecodeRefused` only when that target opts in with a
//! declared handler for it; every other sender hears nothing (#7054 (d),
//! #7076). A request whose typed path does not prove is the exception: its
//! replying row answers it with its reply's `From<PathRefused>`, and nothing
//! else (ADR-0231 §3).
//!
//! Each asker is composed on a [`SubstrateHarness`] beside the refuser and
//! sends it a truncated payload through the production dispatcher. No typed
//! verb sends bytes that are not an encode of their kind, so the asker proves
//! the truncated payload through the boundary (`accept_call`) and forwards it
//! from a turn it sent itself, which keeps the asker as the reply target and
//! the probe in the request chain. Each scenario waits on the request chain's
//! settlement before it reads what the asker received.

use std::sync::mpsc::{self, Sender};

use aether_actor::{
    ActorPath, Addressable, ErasedActorRef, PathRefusal, PathRefused, ProtocolPath, ProtocolRef, Unchecked, Undeclared,
};
use aether_data::{ErasedActorPath, Kind, KindId, LoadName};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::DecodeRefused;
use aether_substrate::actor::native::envelope::Envelope;
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeInitCtx};

/// What the opted-in asker records of each notice: its sender and the notice.
type Notice = (Option<ErasedActorRef>, DecodeRefused);

/// The refuser's request: a fixed eight-byte field, so seven bytes refuse.
#[aether_data::kind(name = "test.decode_refusal.probe", copy)]
struct Probe {
    value: u64,
}

/// Starts an asker's request to the refuser.
#[aether_data::kind(name = "test.decode_refusal.ask", copy)]
struct Ask;

/// What an asker sends itself, so the turn that forwards the probe has the
/// asker as its reply target.
#[aether_data::kind(name = "test.decode_refusal.forward", copy)]
struct Forward;

/// An asker's own forwarding row.
#[aether_actor::protocol]
trait Forwarding {
    fn forward(mail: Forward) -> Undeclared;
}

/// This asker as a [`Forwarding`] target, cast once at `wire` from the path
/// its namespace names.
fn cast_self<A>(ctx: &NativeCtx<'_, A>, namespace: &str) -> Option<ProtocolRef<Forwarding>> {
    let me = ctx.resolve_path(&ErasedActorPath::new(namespace).expect("a canonical path")).ok()?;

    ctx.cast(me)
}

/// Forward the refuser seven bytes of an encoded [`Probe`], one short of its
/// field, under the handler's chain with its reply target: the boundary item
/// proves the refuser's path and carries the bytes as given.
fn forward_truncated_probe<A>(ctx: &NativeCtx<'_, A, Unchecked>) {
    let mut bytes = Probe { value: 7 }.encode_into_bytes();
    bytes.pop();
    let refuser = ErasedActorPath::new(Refuser::NAMESPACE).expect("a canonical path");

    ctx.deliver_forwarded(ctx.accept_call(&refuser, <Probe as Kind>::ID, bytes).expect("the refuser is live"));
}

/// A strict recipient of [`Probe`], which never sees a decodable one.
struct Refuser;

#[aether_actor::actor(root)]
impl NativeActor for Refuser {
    type Config = ();
    const NAMESPACE: &'static str = "test.decode_refusal.refuser";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::tell]
    fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _probe: Probe) {
        let _ = self;
        panic!("a truncated probe never decodes");
    }
}

/// An asker that declares a `DecodeRefused` handler, and records each notice
/// with its sender.
struct NoticedAsker {
    notices: Sender<Notice>,
    me: Option<ProtocolRef<Forwarding>>,
}

#[aether_actor::actor(root, depends(Refuser))]
impl NativeActor for NoticedAsker {
    type Config = ();
    type Params = Sender<Notice>;
    const NAMESPACE: &'static str = "test.decode_refusal.noticed_asker";

    fn init((): (), notices: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { notices, me: None })
    }

    fn wire(state: &mut Self, ctx: &mut NativeCtx<'_>) {
        state.me = cast_self(ctx, Self::NAMESPACE);
    }

    #[aether_actor::handler::tell]
    fn on_ask(&mut self, ctx: &mut NativeCtx<'_>, _ask: Ask) {
        ctx.send_to(self.me.expect("the asker cast itself at wire"), &Forward);
    }

    #[aether_actor::handler::unchecked(reason = "test: forwards the probe, reply target pinned to this asker")]
    fn on_forward(&mut self, ctx: &mut NativeCtx<'_, Self, Unchecked>, _forward: Forward) {
        let _ = self;
        forward_truncated_probe(ctx);
    }

    #[aether_actor::handler::tell]
    fn on_decode_refused(&mut self, ctx: &mut NativeCtx<'_>, notice: DecodeRefused) {
        self.notices.send((ctx.sender(), notice)).expect("notice receiver stays live");
    }
}

/// An asker with only a `#[fallback]`, which records the kind of every mail
/// it receives.
struct FallbackAsker {
    arrivals: Sender<KindId>,
    me: Option<ProtocolRef<Forwarding>>,
}

#[aether_actor::actor(root, depends(Refuser))]
impl NativeActor for FallbackAsker {
    type Config = ();
    type Params = Sender<KindId>;
    const NAMESPACE: &'static str = "test.decode_refusal.fallback_asker";

    fn init((): (), arrivals: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { arrivals, me: None })
    }

    fn wire(state: &mut Self, ctx: &mut NativeCtx<'_>) {
        state.me = cast_self(ctx, Self::NAMESPACE);
    }

    #[aether_actor::handler::tell]
    fn on_ask(&mut self, ctx: &mut NativeCtx<'_>, _ask: Ask) {
        ctx.send_to(self.me.expect("the asker cast itself at wire"), &Forward);
    }

    #[aether_actor::handler::unchecked(reason = "test: forwards the probe, reply target pinned to this asker")]
    fn on_forward(&mut self, ctx: &mut NativeCtx<'_, Self, Unchecked>, _forward: Forward) {
        let _ = self;
        forward_truncated_probe(ctx);
    }

    #[aether_actor::fallback]
    fn on_other(&mut self, _ctx: &mut NativeCtx<'_>, env: &Envelope) {
        self.arrivals.send(env.kind).expect("arrival receiver stays live");
    }
}

/// An asker that declares the row hears exactly one notice, sent by the
/// refuser and naming the refused kind, before its request chain settles.
/// Fails if the refusal answers nothing, answers twice, lands after
/// `Settled`, or carries some other actor as its sender.
#[test]
fn an_asker_declaring_the_row_hears_one_notice_from_the_refuser() {
    let (notices_tx, notices) = mpsc::channel();
    let mut harness = SubstrateHarness::builder()
        .with_actor::<Refuser>(())
        .with_actor::<NoticedAsker>(notices_tx)
        .build()
        .expect("the refuser and its asker boot");
    let asker = harness.actor_ref::<NoticedAsker>();

    harness.execute(vec![("ask", HarnessOp::send_and_settle(&asker, &Ask))]).expect("the request chain settles");

    let (sender, notice) = notices.try_recv().expect("the refusal is answered before the chain settles");
    assert_eq!(sender, Some(harness.actor_ref::<Refuser>().erase()), "the notice's sender is the refuser");
    assert_eq!(notice.kind, <Probe as Kind>::ID, "the notice names the refused kind");
    assert!(notices.try_recv().is_err(), "one refusal answers one notice");
}

/// An asker with only a `#[fallback]` hears nothing: a fallback is not an
/// opt-in, so an in-engine sender keeps #7054 (d). Fails if the refuser
/// answers every reply target, or counts a fallback as a declared row.
#[test]
fn an_asker_with_only_a_fallback_hears_nothing() {
    let (arrivals_tx, arrivals) = mpsc::channel();
    let mut harness = SubstrateHarness::builder()
        .with_actor::<Refuser>(())
        .with_actor::<FallbackAsker>(arrivals_tx)
        .build()
        .expect("the refuser and its asker boot");
    let asker = harness.actor_ref::<FallbackAsker>();

    harness.execute(vec![("ask", HarnessOp::send_and_settle(&asker, &Ask))]).expect("the request chain settles");

    assert!(arrivals.try_recv().is_err(), "nothing reaches an asker that did not opt in");
}

/// A silent row a never-spawned [`Absent`] covers, so a path narrowed to it
/// is well-typed but no route has stood at it.
#[aether_data::kind(name = "test.decode_refusal.poke", copy)]
struct Poke;

#[aether_actor::protocol]
trait Poking {
    fn poke(mail: Poke);
}

/// An instanced actor that is never spawned.
struct Absent;

#[aether_actor::actor(instanced, root)]
impl NativeActor for Absent {
    type Config = ();
    const NAMESPACE: &'static str = "test.decode_refusal.absent";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::tell]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _poke: Poke) {
        let _ = self;
    }
}

/// The path of an [`Absent`] no route has stood at.
fn absent_path() -> ProtocolPath<Poking> {
    ActorPath::<Absent>::instance(&LoadName::new("nowhere").expect("a valid key")).narrow()
}

/// A request that carries a typed path, answered with [`PathAnswer`].
#[aether_data::kind(name = "test.decode_refusal.path_probe", no_serde)]
struct PathProbe {
    path: ProtocolPath<Poking>,
}

/// A tell that carries a typed path, answered with nothing.
#[aether_data::kind(name = "test.decode_refusal.path_tell", no_serde)]
struct PathTell {
    path: ProtocolPath<Poking>,
}

/// [`PathProbe`]'s reply, which can name a refused path.
#[aether_data::kind(name = "test.decode_refusal.path_answer", partial_eq)]
enum PathAnswer {
    Ok,
    Err(PathRefused),
}

impl From<PathRefused> for PathAnswer {
    fn from(refused: PathRefused) -> Self {
        Self::Err(refused)
    }
}

/// A recipient of both path-carrying kinds, which never sees a decodable one.
struct PathRefuser;

#[aether_actor::actor(root)]
impl NativeActor for PathRefuser {
    type Config = ();
    const NAMESPACE: &'static str = "test.decode_refusal.path_refuser";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::request]
    fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _probe: PathProbe) -> PathAnswer {
        let _ = self;
        panic!("an unprovable path never decodes");
    }

    #[aether_actor::handler::tell]
    fn on_tell(&mut self, _ctx: &mut NativeCtx<'_>, _tell: PathTell) {
        let _ = self;
        panic!("an unprovable path never decodes");
    }
}

/// Starts a [`PathProbe`] from the [`PathAsker`].
#[aether_data::kind(name = "test.decode_refusal.ask_probe", copy)]
struct AskProbe;

/// Starts a [`PathTell`] from the [`PathAsker`].
#[aether_data::kind(name = "test.decode_refusal.ask_tell", copy)]
struct AskTell;

/// What the [`PathAsker`] hears back.
#[derive(Debug, PartialEq)]
enum Heard {
    Answer(PathAnswer),
    Refused(KindId),
}

/// An asker that opts into `DecodeRefused` and records every answer it hears.
struct PathAsker {
    heard: Sender<Heard>,
}

#[aether_actor::actor(root, depends(PathRefuser))]
impl NativeActor for PathAsker {
    type Config = ();
    type Params = Sender<Heard>;
    const NAMESPACE: &'static str = "test.decode_refusal.path_asker";

    fn init((): (), heard: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { heard })
    }

    #[aether_actor::handler::tell]
    fn on_ask_probe(&mut self, ctx: &mut NativeCtx<'_>, _ask: AskProbe) {
        let _ = self;
        ctx.send::<PathRefuser>(&PathProbe { path: absent_path() });
    }

    #[aether_actor::handler::tell]
    fn on_ask_tell(&mut self, ctx: &mut NativeCtx<'_>, _ask: AskTell) {
        let _ = self;
        ctx.send::<PathRefuser>(&PathTell { path: absent_path() });
    }

    #[aether_actor::handler::response]
    fn on_answer(&mut self, _ctx: &mut NativeCtx<'_>, answer: PathAnswer) {
        self.heard.send(Heard::Answer(answer)).expect("heard receiver stays live");
    }

    #[aether_actor::handler::tell]
    fn on_decode_refused(&mut self, _ctx: &mut NativeCtx<'_>, notice: DecodeRefused) {
        self.heard.send(Heard::Refused(notice.kind)).expect("heard receiver stays live");
    }
}

fn path_harness() -> (SubstrateHarness, mpsc::Receiver<Heard>) {
    let (heard_tx, heard) = mpsc::channel();
    let harness = SubstrateHarness::builder()
        .with_actor::<PathRefuser>(())
        .with_actor::<PathAsker>(heard_tx)
        .build()
        .expect("the path refuser and its asker boot");

    (harness, heard)
}

/// A request whose typed path no route has stood at is answered exactly once,
/// with its reply's `Err` naming the path, even to an asker that opted into
/// `DecodeRefused`. Fails if the refusal is dropped (the requester waits
/// forever), answered twice (an RPC caller would get two replies), or
/// answered with a notice rather than the request's own reply.
#[test]
fn a_request_whose_path_does_not_prove_is_answered_once_with_its_reply() {
    let (mut harness, heard) = path_harness();
    let asker = harness.actor_ref::<PathAsker>();

    harness.execute(vec![("ask", HarnessOp::send_and_settle(&asker, &AskProbe))]).expect("the request chain settles");

    let refused = PathRefused { path: absent_path().as_erased().clone(), reason: PathRefusal::Unpublished };
    assert_eq!(heard.try_iter().collect::<Vec<_>>(), [Heard::Answer(PathAnswer::Err(refused))]);
}

/// A silent row of a path-carrying kind keeps the drop: the opted-in asker
/// hears the `DecodeRefused` notice and no reply. Fails if the answer reaches
/// a row that has no reply to give.
#[test]
fn a_tell_whose_path_does_not_prove_keeps_the_drop() {
    let (mut harness, heard) = path_harness();
    let asker = harness.actor_ref::<PathAsker>();

    harness.execute(vec![("ask", HarnessOp::send_and_settle(&asker, &AskTell))]).expect("the request chain settles");

    assert_eq!(heard.try_iter().collect::<Vec<_>>(), [Heard::Refused(<PathTell as Kind>::ID)]);
}
