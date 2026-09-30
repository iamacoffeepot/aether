//! What leaves a ctx and under whose chain: `send_to` or flat `send` inherits
//! the handler's causal chain while a detached one mints a fresh root, and
//! every reply entry point accepts a `Pod`-without-`Serialize` cast kind
//! (ADR-0100).
//!
//! Each verb runs inside a handler turn of the booted [`SendProbe`], whose
//! trigger kinds each run one verb and record the lineage the turn ran under.
//! The plain sends land in finishing sinks, one standing at [`StubActor`]'s
//! namespace, where the sender's declared dependency folds, and one standing
//! in for a [`UncheckedCastRelay`]. The context sends go to the pooled
//! [`Bouncer`], whose answer runs a real reply turn on the probe that takes
//! the stored context back.

use std::sync::Arc;
use std::sync::mpsc::Receiver;

use aether_actor::{
    ActorRef, Addressable, ErasedActorRef, HandlesKind, MailSender, OutboundReply, ReplyMode, Single, Unchecked,
    Undeclared,
};
use aether_data::{Encoded, Kind, MailId, RequestId};

use crate::actor::native::envelope::Envelope;
use crate::actor::native::{DeferredReply, Erased, NativeActor, NativeCtx, NativeInitCtx, TaskDone};
use crate::chassis::builder::ReplyTarget;
use crate::chassis::error::BootError;
use crate::mail::mailer::Mailer;
use crate::mail::registry::Registry;
use crate::mail::{Source, SourceAddr};
use crate::testing::{PumpedDriver, bare_substrate, boot_test_chassis_with};

use super::support::{Bouncer, CastOnly, NativeRequestContext, Poke, Poked, StubActor, sink};

#[aether_actor::protocol]
trait CastRelay {
    fn cast(mail: CastOnly) -> Undeclared;
}

struct UncheckedCastRelay {
    received: u32,
}

#[aether_actor::actor(instanced, root)]
impl NativeActor for UncheckedCastRelay {
    const NAMESPACE: &'static str = "test.unchecked_cast_relay";
    type Config = ();

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { received: 0 })
    }

    #[handler::unchecked(reason = "test: a cast-only receiver exercising the unchecked row")]
    fn on_cast(&mut self, _ctx: &mut NativeCtx<'_, Self, Unchecked>, _mail: CastOnly) {
        self.received += 1;
    }
}

/// Runs `send_to` through the declared dependency's proof.
#[aether_data::kind(name = "test.native_send.send_to")]
struct SendTo;

/// Runs `send_to_with_context` through a borrow of the dependency's proof.
#[aether_data::kind(name = "test.native_send.send_to_with_context", copy)]
struct SendToWithContext {
    value: u32,
}

/// Runs `send_detached_to_with_context` through the dependency's proof.
#[aether_data::kind(name = "test.native_send.send_detached_to_with_context", copy)]
struct SendDetachedToWithContext {
    value: u32,
}

/// Runs `forward_to` through the relay's protocol reference.
#[aether_data::kind(name = "test.native_send.forward_to")]
struct ForwardTo;

/// Runs the inherent `send_detached_to` through the relay's protocol
/// reference, then the `MailSender` delegation through the same reference.
#[aether_data::kind(name = "test.native_send.send_detached_to")]
struct SendDetachedTo;

/// Runs `send_encoded_detached_to` through the relay's protocol reference.
#[aether_data::kind(name = "test.native_send.send_encoded_detached_to")]
struct SendEncodedDetachedTo;

/// Runs the flat `send_detached::<StubActor>`.
#[aether_data::kind(name = "test.native_send.send_detached")]
struct SendDetached;

/// Runs the flat `send::<StubActor>`.
#[aether_data::kind(name = "test.native_send.send")]
struct FlatSend;

/// Runs the flat `send_with_context::<StubActor>`.
#[aether_data::kind(name = "test.native_send.send_with_context", copy)]
struct SendWithContext {
    value: u32,
}

/// What one [`SendProbe`] turn read off its ctx and what its verbs emitted.
#[derive(Debug, Clone)]
struct Turn {
    /// The handled mail.
    mail: Option<MailId>,
    /// The root of the chain the turn ran under.
    root: Option<MailId>,
    /// Where the handled mail's reply goes.
    reply_target: Source,
    /// The ids the turn's verbs returned or minted, in call order.
    emitted: Vec<Option<MailId>>,
    /// The fresh chains the turn's detached sends rooted, which the test
    /// settles beside the turn's own.
    detached: Vec<MailId>,
}

impl Turn {
    fn of<M: ReplyMode>(
        ctx: &NativeCtx<'_, SendProbe, M>,
        emitted: Vec<Option<MailId>>,
        detached: Vec<MailId>,
    ) -> Self {
        Self {
            mail: ctx.in_flight_mail_id(),
            root: ctx.in_flight_root(),
            reply_target: ctx.reply_target(),
            emitted,
            detached,
        }
    }
}

/// The id of the mail this turn last sent, for a verb that returns none: the
/// binding mints each id from its own mailbox and the correlation it
/// advances per send (ADR-0042).
fn last_sent<M: ReplyMode>(ctx: &NativeCtx<'_, SendProbe, M>) -> MailId {
    MailId::new(ctx.binding.self_mailbox(), ctx.prev_correlation())
}

/// What one [`SendProbe`] reply turn read off its ctx.
#[derive(Debug, Clone, PartialEq)]
struct Reply {
    /// The request the reply answers.
    answers: Option<RequestId>,
    /// The root of the chain the reply ran under.
    root: Option<MailId>,
    /// The actor that answered.
    sender: Option<ErasedActorRef>,
    /// The context the reply turn's `take_context` recovered.
    taken: Option<NativeRequestContext>,
}

/// A pumped root that declares [`StubActor`] and [`Bouncer`], holds a
/// [`UncheckedCastRelay`] proof, runs one send verb per trigger, and takes the
/// context back on each [`Poked`] reply.
struct SendProbe {
    relay: ActorRef<UncheckedCastRelay>,
    turns: Vec<Turn>,
    replies: Vec<Reply>,
}

#[aether_actor::actor(singleton, root, depends(StubActor, Bouncer))]
impl NativeActor for SendProbe {
    const NAMESPACE: &'static str = "test.native_send.sender";
    type Config = ();
    type Params = ActorRef<UncheckedCastRelay>;

    fn init((): (), relay: ActorRef<UncheckedCastRelay>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { relay, turns: Vec::new(), replies: Vec::new() })
    }

    #[handler::single]
    fn on_send_to(&mut self, ctx: &mut NativeCtx<'_>, _trigger: SendTo) {
        ctx.send_to(ctx.actor_ref::<StubActor>(), &CastOnly { code: 1 });
        self.turns.push(Turn::of(ctx, Vec::new(), Vec::new()));
    }

    #[handler::single]
    fn on_send_to_with_context(&mut self, ctx: &mut NativeCtx<'_>, trigger: SendToWithContext) {
        let bouncer = ctx.actor_ref::<Bouncer>();
        let borrowed = &bouncer;
        let sent = ctx.send_to_with_context(borrowed, &Poke, NativeRequestContext { value: trigger.value });
        self.turns.push(Turn::of(ctx, vec![Some(sent)], Vec::new()));
    }

    #[handler::single]
    fn on_send_detached_to_with_context(&mut self, ctx: &mut NativeCtx<'_>, trigger: SendDetachedToWithContext) {
        let bouncer = ctx.actor_ref::<Bouncer>();
        let sent = ctx.send_detached_to_with_context(bouncer, &Poke, NativeRequestContext { value: trigger.value });
        self.turns.push(Turn::of(ctx, vec![Some(sent)], vec![sent]));
    }

    #[handler::unchecked(reason = "test: forwards the request, reply pinned to the requester")]
    fn on_forward_to(&mut self, ctx: &mut NativeCtx<'_, Self, Unchecked>, _trigger: ForwardTo) {
        ctx.forward_to(self.relay.narrow::<CastRelay>(), &CastOnly { code: 9 });
        self.turns.push(Turn::of(ctx, Vec::new(), Vec::new()));
    }

    #[handler::single]
    fn on_send_detached_to(&mut self, ctx: &mut NativeCtx<'_>, _trigger: SendDetachedTo) {
        let inherent = ctx.send_detached_to(self.relay.narrow::<CastRelay>(), &CastOnly { code: 10 });
        MailSender::send_detached_to(ctx, self.relay.narrow::<CastRelay>(), &CastOnly { code: 11 });
        let delegated = last_sent(ctx);
        self.turns.push(Turn::of(ctx, vec![Some(inherent), Some(delegated)], vec![inherent, delegated]));
    }

    #[handler::single]
    fn on_send_encoded_detached_to(&mut self, ctx: &mut NativeCtx<'_>, _trigger: SendEncodedDetachedTo) {
        let payload = Encoded::new(&CastOnly { code: 12 });
        let sent = ctx.send_encoded_detached_to(self.relay.narrow::<CastRelay>(), &payload);
        self.turns.push(Turn::of(ctx, vec![sent], sent.into_iter().collect()));
    }

    #[handler::single]
    fn on_send_detached(&mut self, ctx: &mut NativeCtx<'_>, _trigger: SendDetached) {
        ctx.send_detached::<StubActor>(&CastOnly { code: 5 });
        let sent = last_sent(ctx);
        self.turns.push(Turn::of(ctx, vec![Some(sent)], vec![sent]));
    }

    #[handler::single]
    fn on_send(&mut self, ctx: &mut NativeCtx<'_>, _trigger: FlatSend) {
        ctx.send::<StubActor>(&CastOnly { code: 6 });
        self.turns.push(Turn::of(ctx, Vec::new(), Vec::new()));
    }

    #[handler::single]
    fn on_send_with_context(&mut self, ctx: &mut NativeCtx<'_>, trigger: SendWithContext) {
        let sent = ctx.send_with_context::<Bouncer>(&Poke, NativeRequestContext { value: trigger.value });
        self.turns.push(Turn::of(ctx, vec![Some(sent)], Vec::new()));
    }

    #[handler::single]
    fn on_poked(&mut self, ctx: &mut NativeCtx<'_>, _poked: Poked) {
        let taken = ctx.take_context::<NativeRequestContext>();
        self.replies.push(Reply {
            answers: ctx.in_reply_to(),
            root: ctx.in_flight_root(),
            sender: ctx.sender(),
            taken,
        });
    }
}

/// A booted [`SendProbe`] beside its pooled [`Bouncer`], the sink at
/// [`StubActor`]'s namespace, and the sink its [`UncheckedCastRelay`] proof
/// points at.
struct Rig {
    driver: PumpedDriver<SendProbe>,
    bouncer: ErasedActorRef,
    stub: ErasedActorRef,
    stub_mail: Receiver<Envelope>,
    relay: ErasedActorRef,
    relay_mail: Receiver<Envelope>,
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
}

impl Rig {
    fn boot() -> Self {
        let (registry, mailer) = bare_substrate();
        let (stub, stub_mail) = sink(&registry, &mailer, StubActor::NAMESPACE);
        let (relay, relay_mail) = sink(&registry, &mailer, "test.native_send.relay_sink");
        let driver = PumpedDriver::boot(
            boot_test_chassis_with::<Bouncer>(&registry, &mailer, (), ()),
            (),
            Registry::declared_dependency::<UncheckedCastRelay>(relay.id()),
        );
        let bouncer = driver.chassis().actor_ref::<Bouncer>().erase();

        Self { driver, bouncer, stub, stub_mail, relay, relay_mail, registry, mailer }
    }

    /// Push `trigger` to the sender as a chassis root answered to `reply`,
    /// settle it and every root the turn emitted, and return the turn.
    fn run<K: Kind>(&mut self, trigger: &K, reply: Option<ReplyTarget>) -> (MailId, Turn)
    where
        SendProbe: HandlesKind<K>,
    {
        let sender = self.driver.chassis().actor_ref::<SendProbe>();
        let root = self.driver.send_and_settle(sender, trigger, reply);
        let turn = self.driver.read_state(|sender| sender.turns.last().cloned()).flatten().expect("the turn ran");
        self.driver.settle(&turn.detached);
        (root, turn)
    }

    /// What the sender's last reply turn read.
    fn last_reply(&self) -> Reply {
        self.driver.read_state(|sender| sender.replies.last().cloned()).flatten().expect("the reply turn ran")
    }
}

/// ADR-0232 §1: the flat `send_to` family sends through a held reference.
/// `send_to` lands at the reference's id under the turn's root with the
/// handled mail as parent. The context variants reach the reference's actor,
/// `send_to_with_context` on the turn's chain and
/// `send_detached_to_with_context` on the chain its own mail roots, and both
/// store the context under the correlation of the mail they routed: the
/// answering actor's real reply turn takes it back, which is how the bloomery
/// driver's replies find their way back to the continuation that sent them.
/// The legs cover a typed reference by value and a borrow of one.
#[test]
fn send_to_family_inherits_or_detaches_and_stores_context() {
    let mut rig = Rig::boot();

    let (root, turn) = rig.run(&SendTo, None);
    let sent = rig.stub_mail.try_recv().expect("send_to routed at flush");
    assert_eq!(turn.root, Some(root), "the turn runs under the tracked root");
    assert!(turn.mail.is_some(), "the turn handles a mail with an id");
    assert_eq!(sent.recipient, rig.stub.id(), "send_to addresses the reference's id");
    assert_eq!(sent.root, Some(root), "send_to inherits the caller's root");
    assert_eq!(sent.parent_mail, turn.mail, "send_to's parent is the handled mail");

    let (root, turn) = rig.run(&SendToWithContext { value: 21 }, None);
    let inherited_id = turn.emitted[0].expect("send_to_with_context returns its id");
    assert_eq!(
        rig.last_reply(),
        Reply {
            answers: Some(RequestId(inherited_id.correlation_id)),
            root: Some(root),
            sender: Some(rig.bouncer),
            taken: Some(NativeRequestContext { value: 21 }),
        },
        "the reference's actor answers on the caller's chain, and its reply takes the context back",
    );

    let (_root, turn) = rig.run(&SendDetachedToWithContext { value: 34 }, None);
    let detached_id = turn.emitted[0].expect("send_detached_to_with_context returns its id");
    assert_eq!(
        rig.last_reply(),
        Reply {
            answers: Some(RequestId(detached_id.correlation_id)),
            root: Some(detached_id),
            sender: Some(rig.bouncer),
            taken: Some(NativeRequestContext { value: 34 }),
        },
        "the proof's actor answers on the chain the detached mail roots, and its reply takes the context back",
    );
}

/// ADR-0231 §9: a relay accepts the forwarded kind only through the target's
/// typed row, while preserving the original requester's reply destination and
/// the inbound parent/root. The protocol row is unchecked, so this also catches
/// an implementation that accidentally rejects `Undeclared` relay targets.
#[test]
fn forward_to_typed_unchecked_protocol_preserves_reply_target_and_lineage() {
    let mut rig = Rig::boot();
    let (requester, _replies) = sink(&rig.registry, &rig.mailer, "test.native_send.requester");

    let (root, turn) = rig.run(&ForwardTo, Some(ReplyTarget::Actor { to: requester, correlation: 73 }));

    let forwarded = rig.relay_mail.try_recv().expect("forward_to routed at flush");
    assert_eq!(forwarded.recipient, rig.relay.id(), "forward_to addresses the typed target");
    assert_eq!(
        forwarded.sender,
        Source::with_correlation(SourceAddr::Component(requester.id()), 73),
        "the target replies directly to the original requester",
    );
    assert_eq!(forwarded.sender, turn.reply_target, "the forward keeps the handled mail's reply target");
    assert_eq!(forwarded.parent_mail, turn.mail, "the forwarded mail remains a child of the inbound");
    assert_eq!(forwarded.root, Some(root), "the forwarded mail remains in the inbound chain");
}

/// The inherent typed detached verb returns the id minted by its one buffered
/// push, starts a parentless chain rooted at that id, and stamps this actor as
/// the reply destination. The compatibility trait delegates to the same path
/// while retaining its shared `()` return signature.
#[test]
fn typed_detached_send_returns_emitted_id_and_mail_sender_delegates() {
    let mut rig = Rig::boot();
    let sender = rig.driver.chassis().actor_ref::<SendProbe>().erase();

    let (_root, turn) = rig.run(&SendDetachedTo, None);
    let (emitted_id, delegated_id) =
        (turn.emitted[0].expect("the inherent verb"), turn.emitted[1].expect("the delegation"));
    let arrived = [
        rig.relay_mail.try_recv().expect("a detached send routed at flush"),
        rig.relay_mail.try_recv().expect("a detached send routed at flush"),
    ];
    let routed = |id: MailId| arrived.iter().find(|mail| mail.mail_id == Some(id)).expect("the minted id was routed");

    let detached = routed(emitted_id);
    assert!(detached.parent_mail.is_none(), "detached mail has no parent");
    assert_eq!(detached.root, Some(emitted_id), "detached mail roots its own chain");
    assert_eq!(
        detached.sender,
        Source::with_correlation(SourceAddr::Component(sender.id()), emitted_id.correlation_id),
        "replies address the actor that sent the detached mail",
    );

    let delegated = routed(delegated_id);
    assert_eq!(delegated.recipient, rig.relay.id(), "compatibility delegation keeps the target");
    assert!(delegated.parent_mail.is_none(), "compatibility delegation stays detached");
    assert_eq!(delegated.root, delegated.mail_id, "compatibility delegation roots the emitted mail");
}

/// A payload encoded off the sending thread reaches an unchecked protocol row as
/// the kind its `Encoded<K>` names, byte for byte, on a fresh chain. Catches
/// the verb stamping a kind other than `K`, sending bytes other than the
/// encode, or inheriting the handler's chain the way `send_to` does.
#[test]
fn encoded_detached_send_delivers_the_encoded_kind_through_a_unchecked_protocol_row() {
    let mut rig = Rig::boot();

    let (_root, turn) = rig.run(&SendEncodedDetachedTo, None);
    let emitted_id = turn.emitted[0].expect("an ordinary kind sends");

    let sent = rig.relay_mail.try_recv().expect("the encoded send routed at flush");
    assert_eq!(sent.recipient, rig.relay.id(), "the send addresses the protocol reference's target");
    assert_eq!(sent.kind, CastOnly::ID, "the kind is the one the payload was encoded as");
    assert_eq!(bytemuck::pod_read_unaligned::<CastOnly>(sent.payload.bytes()).code, 12, "the payload decodes as sent");
    assert_eq!(sent.mail_id, Some(emitted_id), "the verb returns the push's id");
    assert!(sent.parent_mail.is_none(), "the encoded send carries no parent edge");
    assert_eq!(sent.root, Some(emitted_id), "the encoded send roots its own chain");
}

/// ADR-0232 §1–§2: the flat `send_detached::<R>` on a ctx typed by an actor
/// that declares `R` lands at the position the dependency's proof points to,
/// and roots a fresh chain despite the handler's in-flight lineage
/// (ADR-0080 §7). The sink stands at the stub actor's own namespace, so a
/// verb that resolved any other position, or inherited the running chain,
/// fails here rather than only in the fleet proxy's reports.
#[test]
fn flat_send_detached_reaches_the_declared_dependency_on_a_fresh_chain() {
    let mut rig = Rig::boot();

    let (_root, turn) = rig.run(&SendDetached, None);

    let detached = rig.stub_mail.try_recv().expect("flat send_detached routed at flush");
    assert_eq!(detached.recipient, rig.stub.id(), "flat send_detached addresses the declared dependency");
    assert!(detached.parent_mail.is_none(), "flat send_detached carries no parent edge");
    assert_eq!(detached.mail_id, turn.emitted[0], "the turn's last send is the detached mail");
    assert_eq!(detached.root, detached.mail_id, "flat send_detached is its own root");
}

/// ADR-0232 §1: the flat `send::<R>` and `send_with_context::<R>` on a ctx
/// typed by an actor that declares `R` reach the actor the dependency's proof
/// points to under the handler's in-flight root (ADR-0080 §7); `send` lands
/// with the handled mail as parent, and `send_with_context` stores its
/// context under the routed mail's correlation, which the answering actor's
/// real reply turn takes back. A body copied from `send_detached` with no
/// lineage, a wrong recipient, or a context stored under another correlation
/// fails here rather than only in the audio and text caps' fs round trips.
#[test]
fn flat_send_and_send_with_context_reach_the_declared_dependency_on_the_handlers_chain() {
    let mut rig = Rig::boot();

    let (root, turn) = rig.run(&FlatSend, None);
    let sent = rig.stub_mail.try_recv().expect("flat send routed at flush");
    assert!(turn.mail.is_some(), "the turn handles a mail with an id");
    assert_eq!(sent.recipient, rig.stub.id(), "flat send addresses the declared dependency");
    assert_eq!(sent.root, Some(root), "flat send inherits the caller's root");
    assert_eq!(sent.parent_mail, turn.mail, "flat send's parent is the handled mail");

    let (root, turn) = rig.run(&SendWithContext { value: 55 }, None);
    let context_id = turn.emitted[0].expect("send_with_context returns its id");
    assert_eq!(
        rig.last_reply(),
        Reply {
            answers: Some(RequestId(context_id.correlation_id)),
            root: Some(root),
            sender: Some(rig.bouncer),
            taken: Some(NativeRequestContext { value: 55 }),
        },
        "the declared dependency answers on the caller's chain, and its reply takes the context back",
    );
}

/// One `TaskDone<CastOnly, ()>` per `resolve*` method, bundled into a tuple
/// so `_assert_cast_kind_repliable`'s parameter count stays under clippy's
/// `too_many_arguments` threshold without a suppression.
type CastOnlyTaskDones =
    (TaskDone<CastOnly, ()>, TaskDone<CastOnly, ()>, TaskDone<CastOnly, ()>, TaskDone<CastOnly, ()>);

/// Type-level proof (ADR-0100): a `Pod`-without-`Serialize` cast kind
/// is repliable through every native reply entry point — the bounds
/// relaxed from `K: Kind + serde::Serialize` to `K: Kind` (now
/// `K: ActorMail`, ADR-0233). Never
/// called; the compile is the assertion. If a reply bound regains a
/// `serde::Serialize` half, this stops compiling. Covers the direct
/// entry points (`OutboundReply::reply`, `reply_to`, `reply_to_target`)
/// and the offload reply paths (`DeferredReply::reply` and every
/// `TaskDone::resolve*`) alike.
#[allow(dead_code)]
fn _assert_cast_kind_repliable(
    ctx: &mut NativeCtx<'_, Erased, Unchecked>,
    sender: Source,
    deferred: DeferredReply,
    task_dones: CastOnlyTaskDones,
    task_ctx: &mut NativeCtx<'_, Erased, Single>,
) {
    OutboundReply::reply(ctx, &CastOnly { code: 2 });
    OutboundReply::reply_to(ctx, sender, &CastOnly { code: 3 });
    ctx.reply_to_target(sender, &CastOnly { code: 4 }, None, None);

    let (task_resolve, task_resolve_with, task_resolve_value, task_resolve_err) = task_dones;
    deferred.reply(task_ctx, &CastOnly { code: 5 });
    task_resolve.resolve(task_ctx);
    task_resolve_with.resolve_with(task_ctx, |output, _context| *output);
    task_resolve_value.resolve_value(task_ctx, &CastOnly { code: 6 });
    task_resolve_err.resolve_err(task_ctx, &CastOnly { code: 7 });
}
