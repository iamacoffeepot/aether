//! A listener and its sessions close when the consumer they deliver to
//! closes (issue 7552), driven through the real capability, a real loopback
//! socket, and a consumer that shuts itself down.
//!
//! No test here sleeps to order mail. A departure's notices are posted to
//! its monitors in the order they registered. The capability registers its
//! monitor on a listener while it answers the bind, and [`Watcher`] registers
//! its own only after that reply, so once the [`Watcher`] has handled the
//! listener's notice the capability's is already in the capability's inbox,
//! and a `ListListeners` sent after that is handled behind it. The
//! [`Watcher`] holds the reply to [`AwaitDeparture`] until its notice
//! arrives, which keeps that request's chain open, so settling the request
//! is the wait.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::Duration;

use aether_actor::{ActorPath, ActorRef, Addressable, HeldReply, ProtocolPath, actor};
use aether_data::{ErasedActorPath, LoadName};
use aether_kinds::MonitorNotice;
use aether_substrate::MonitorHandle;
use aether_substrate::actor::native::spawn::Subname;
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::chassis::builder::PassiveChassis;
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::outbound::EgressEvent;
use aether_substrate::testing::TestChassis;

use super::{
    BindListener, BindListenerResult, CapturedSessionMail, ListListeners, ListListenersResult, SessionConsumer,
    ShutDown, TcpCapability, TcpConsumer, TcpListenerActor, address, boot_tcp_substrate_with, drive_and_decode,
    framed_body, send_and_settle,
};

/// Monitor the actor at `target`; the reply confirms the monitor stands.
#[aether_data::kind(name = "test.tcp.watch", no_serde)]
struct Watch {
    target: ErasedActorPath,
}

#[aether_data::kind(name = "test.tcp.watching", copy, no_serde)]
struct Watching;

/// Answered once the watched actor's `MonitorNotice` has arrived.
#[aether_data::kind(name = "test.tcp.await_departure", copy, no_serde)]
struct AwaitDeparture;

#[aether_data::kind(name = "test.tcp.noticed", copy, partial_eq, no_serde)]
struct Noticed {
    notified: bool,
}

impl HeldReply for Noticed {
    fn unanswered() -> Self {
        Self { notified: false }
    }
}

/// Monitors one actor, the way the capability monitors a listener, and holds
/// an [`AwaitDeparture`] until that actor's notice arrives.
struct Watcher {
    watch: Option<MonitorHandle>,
    departed: bool,
    waiting: Option<Held<Noticed>>,
}

#[actor(singleton, root)]
impl NativeActor for Watcher {
    const NAMESPACE: &'static str = "test.tcp.watcher";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { watch: None, departed: false, waiting: None })
    }

    #[handler::request]
    fn on_watch(&mut self, ctx: &mut NativeCtx<'_>, watch: Watch) -> Watching {
        let proven = ctx.resolve_path(&watch.target).expect("the watched actor is live");
        self.watch = Some(ctx.monitor(proven));
        self.departed = false;

        Watching
    }

    #[handler::request]
    fn on_await_departure(&mut self, ctx: &mut NativeCtx<'_>, _await: AwaitDeparture) -> Pending<Noticed> {
        let (pending, held) = ctx.hold::<Noticed>();
        if self.departed {
            held.answer(ctx, &Noticed { notified: true });
        } else {
            self.waiting = Some(held);
        }

        pending
    }

    #[handler::event]
    fn on_monitor_notice(&mut self, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        drop(self.watch.take());
        self.departed = true;
        if let Some(held) = self.waiting.take() {
            held.answer(ctx, &Noticed { notified: true });
        }
    }
}

type Booted = (mpsc::Receiver<EgressEvent>, PassiveChassis<TestChassis>);

/// The capability with a [`Watcher`] composed beside it.
fn boot_with_watcher() -> Booted {
    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate_with(|builder| builder.with_actor::<Watcher>(()));

    (rx, chassis)
}

/// A [`SessionConsumer`] at the root instance `key`: the reference a test
/// shuts it down through, its path as a bind's `consumer`, and its captures.
fn spawn_closable_consumer(
    chassis: &PassiveChassis<TestChassis>,
    key: &str,
) -> (ActorRef<SessionConsumer>, ProtocolPath<TcpConsumer>, mpsc::Receiver<CapturedSessionMail>) {
    let (captures, received) = mpsc::channel();
    let consumer = chassis
        .spawn_actor::<SessionConsumer>(Subname::Named(key), captures, ())
        .finish()
        .expect("the session consumer spawns");
    let path = ActorPath::<SessionConsumer>::instance(&LoadName::new(key).expect("a valid key")).narrow();

    (consumer, path, received)
}

/// Bind a listener named `name` on an OS-picked loopback port and answer the
/// port.
fn bind((rx, chassis): &Booted, name: &str, consumer: Option<ProtocolPath<TcpConsumer>>) -> u16 {
    let tcp = chassis.actor_ref::<TcpCapability>();
    let bound: BindListenerResult = drive_and_decode(
        chassis,
        rx,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some(name.into()), consumer },
    );

    match bound {
        BindListenerResult::Ok { local_port, .. } => local_port,
        BindListenerResult::Err(error) => panic!("bind failed: {error:?}"),
    }
}

/// Have the [`Watcher`] monitor the bound listener named `name`.
fn watch_listener((rx, chassis): &Booted, name: &str) {
    let listener = format!("{}/{}:{name}", TcpCapability::NAMESPACE, TcpListenerActor::NAMESPACE);
    let _: Watching =
        drive_and_decode(chassis, rx, chassis.actor_ref::<Watcher>(), &Watch { target: address(&listener) });
}

/// Wait until the listener the [`Watcher`] monitors has closed.
fn await_departure((rx, chassis): &Booted) {
    let noticed: Noticed = drive_and_decode(chassis, rx, chassis.actor_ref::<Watcher>(), &AwaitDeparture);
    assert_eq!(noticed, Noticed { notified: true }, "the watched listener's notice arrives");
}

/// The names of the listeners the capability lists.
fn listed((rx, chassis): &Booted) -> Vec<String> {
    let list: ListListenersResult =
        drive_and_decode(chassis, rx, chassis.actor_ref::<TcpCapability>(), &ListListeners::default());

    list.listeners.into_iter().map(|listener| listener.name).collect()
}

/// A consumer that closes without unbinding takes its listener with it: the
/// listener closes, the capability's entry goes, and the port stops
/// accepting. Fails for a listener that never monitors its consumer or
/// monitors and does not close (the watcher's notice never arrives), and for
/// an entry the capability keeps after the listener is gone.
#[test]
fn a_listener_closes_when_its_consumer_closes() {
    let booted = boot_with_watcher();
    let (consumer, consumer_path, _received) = spawn_closable_consumer(&booted.1, "closing-consumer");
    let local_port = bind(&booted, "closing", Some(consumer_path));
    watch_listener(&booted, "closing");

    send_and_settle(&booted.1, consumer, &ShutDown, None);
    await_departure(&booted);

    assert!(listed(&booted).is_empty(), "the closed consumer's listener leaves the capability's list");
    let dialed = TcpStream::connect(("127.0.0.1", local_port));
    assert!(dialed.is_err(), "the closed consumer's port no longer accepts");
}

/// A consumer that closes takes its live sessions with it: the peer of a
/// session that was delivering to it reads end of stream. Fails for a
/// session that never monitors its consumer, which keeps the peer's
/// connection open and its read thread running with nothing to deliver to.
#[test]
fn a_session_closes_when_its_consumer_closes() {
    let booted = boot_with_watcher();
    let (consumer, consumer_path, received) = spawn_closable_consumer(&booted.1, "session-consumer");
    let local_port = bind(&booted, "sessions", Some(consumer_path));
    let mut client = TcpStream::connect(("127.0.0.1", local_port)).expect("connect loopback client");
    client.write_all(&framed_body(b"live")).expect("write one complete frame");

    // Session mail is driven by the socket, not by a root this test holds, so
    // the capture channel is the only signal and its wait is time-bounded.
    // The delivery shows the session is live and bound to the consumer.
    let delivered = received.recv_timeout(Duration::from_secs(2)).expect("SessionData reaches the consumer");
    assert!(matches!(delivered, CapturedSessionMail::Data(_)), "expected SessionData, got {delivered:?}");

    send_and_settle(&booted.1, consumer, &ShutDown, None);

    // The close reaches this plain socket through the kernel, not through a
    // chain the test holds; the read timeout only keeps a session that
    // outlived its consumer from hanging the read.
    client.set_read_timeout(Some(Duration::from_secs(5))).expect("bound the wait for the close");
    let mut trailing = Vec::new();
    let read = client.read_to_end(&mut trailing).expect("the session closes the peer's connection");
    assert_eq!(read, 0, "the session writes nothing more before it closes");
}

/// One consumer's close ends only what was bound to it: a listener bound
/// with no consumer stays listed and bound. Fails when a consumer's close
/// reaches a listener that was not bound to it, such as a close that sweeps
/// every listener, or a listener with no consumer that monitors something
/// and closes on its notice.
#[test]
fn a_listener_with_no_consumer_stays_bound_when_another_actor_closes() {
    let booted = boot_with_watcher();
    let (consumer, consumer_path, _received) = spawn_closable_consumer(&booted.1, "other-consumer");
    let unowned_port = bind(&booted, "unowned", None);
    bind(&booted, "owned", Some(consumer_path));
    watch_listener(&booted, "owned");

    send_and_settle(&booted.1, consumer, &ShutDown, None);
    await_departure(&booted);

    assert_eq!(listed(&booted), ["unowned"], "the listener with no consumer is the one still listed");
    TcpStream::connect(("127.0.0.1", unowned_port)).expect("the listener with no consumer still accepts");
}
