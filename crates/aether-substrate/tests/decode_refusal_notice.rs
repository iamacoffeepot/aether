//! A native recipient that refuses a request's payload at decode answers the
//! reply target a `DecodeRefused` only when that target opts in with a
//! declared handler for it; every other sender hears nothing (#7054 (d),
//! #7076).
//!
//! Each asker is composed on a [`SubstrateHarness`] beside the refuser and
//! sends it a truncated payload through the production dispatcher. No typed
//! verb sends bytes that are not an encode of their kind, so the asker proves
//! the truncated payload through the boundary (`accept_call`) and forwards it
//! from a turn it sent itself, which keeps the asker as the reply target and
//! the probe in the request chain. Each scenario waits on the request chain's
//! settlement before it reads what the asker received.

use std::sync::mpsc::{self, Sender};

use aether_actor::{Addressable, ErasedActorRef, ProtocolRef, Unchecked, Undeclared};
use aether_data::{ErasedActorPath, Kind, KindId};
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

    #[aether_actor::handler::single]
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

    #[aether_actor::handler::single]
    fn on_ask(&mut self, ctx: &mut NativeCtx<'_>, _ask: Ask) {
        ctx.send_to(self.me.expect("the asker cast itself at wire"), &Forward);
    }

    #[aether_actor::handler::unchecked(reason = "test: forwards the probe, reply target pinned to this asker")]
    fn on_forward(&mut self, ctx: &mut NativeCtx<'_, Self, Unchecked>, _forward: Forward) {
        let _ = self;
        forward_truncated_probe(ctx);
    }

    #[aether_actor::handler::single]
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

    #[aether_actor::handler::single]
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
