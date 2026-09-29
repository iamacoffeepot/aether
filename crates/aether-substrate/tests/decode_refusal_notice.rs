//! A native recipient that refuses a request's payload at decode answers the
//! reply target a `DecodeRefused` only when that target opts in with a
//! declared handler for it; every other sender hears nothing (#7054 (d),
//! #7076).
//!
//! Each asker is composed on a [`SubstrateHarness`] beside the refuser and
//! sends it a truncated payload through the production dispatcher. Each
//! scenario waits on the request chain's settlement before it reads what the
//! asker received.

use std::sync::mpsc::{self, Sender};

use aether_actor::ErasedActorRef;
use aether_data::{Kind, KindId};
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

/// Send the refuser seven bytes of an encoded [`Probe`], one short of its
/// field, under the handler's chain.
fn send_truncated_probe<A>(ctx: &NativeCtx<'_, A>, refuser: ErasedActorRef) {
    let mut bytes = Probe { value: 7 }.encode_into_bytes();
    bytes.pop();
    let sent = ctx.send_envelope_tracked_to(refuser, <Probe as Kind>::ID, &bytes);
    assert!(sent.is_some(), "the truncated probe is sent");
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
}

#[aether_actor::actor(root, depends(Refuser))]
impl NativeActor for NoticedAsker {
    type Config = ();
    type Params = Sender<Notice>;
    const NAMESPACE: &'static str = "test.decode_refusal.noticed_asker";

    fn init((): (), notices: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { notices })
    }

    #[aether_actor::handler::single]
    fn on_ask(&mut self, ctx: &mut NativeCtx<'_>, _ask: Ask) {
        let _ = self;
        send_truncated_probe(ctx, ctx.actor_ref::<Refuser>().erase());
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
}

#[aether_actor::actor(root, depends(Refuser))]
impl NativeActor for FallbackAsker {
    type Config = ();
    type Params = Sender<KindId>;
    const NAMESPACE: &'static str = "test.decode_refusal.fallback_asker";

    fn init((): (), arrivals: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { arrivals })
    }

    #[aether_actor::handler::single]
    fn on_ask(&mut self, ctx: &mut NativeCtx<'_>, _ask: Ask) {
        let _ = self;
        send_truncated_probe(ctx, ctx.actor_ref::<Refuser>().erase());
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
