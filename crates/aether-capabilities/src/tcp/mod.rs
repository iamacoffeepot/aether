//! `aether.tcp` cap (issue 607 Phase 6a, ADR-0079).
//!
//! Three-tier shape: [`TcpCapability`] (Singleton control plane) →
//! [`TcpListenerActor`] (Instanced, one per bound port) → eventually
//! `TcpSessionActor` (Instanced, Phase 6b — per connection). Phase 6a
//! lands the singleton + listener and a stub accept handler that
//! drops accepted streams; Phase 6b adds the session spawn and the
//! read/write surface.
//!
//! ## Supervision shape
//!
//! `TcpCapability` is the supervisor of its listener fleet: it spawns
//! listeners, monitors them, and replies to unbind requests on their
//! close. The cap holds its own `MailboxId → ListenerEntry` map; it
//! does NOT walk the chassis-wide actor registry to enumerate
//! children. Cap handlers don't introspect the registry — the
//! cap-as-supervisor pattern keeps the actor model intact (caps
//! communicate via mail at runtime; chassis-level introspection is a
//! test/embedder affordance, not a handler-side surface).
//!
//! ## Mail surface
//!
//! Control plane (mailed to `aether.tcp`):
//! - `BindListener { addr, name? }` → `BindListenerResult`
//! - `UnbindListener { listener_name }` → `UnbindListenerResult`
//!   (asynchronous reply: the cap monitors the listener at spawn time
//!   and replies only after `MonitorNotice` arrives)
//! - `ListListeners` → `ListListenersResult`
//!
//! Listener (mailed to `aether.tcp.listener:<name>`):
//! - `Close` → cooperative shutdown via `ctx.shutdown()`
//!
//! ## Threading
//!
//! Each listener owns one sidecar OS thread that holds the
//! `std::net::TcpListener` and runs a blocking accept loop. On
//! `unwire` the listener flips a shutdown flag and self-connects
//! to its bound port to wake the blocked accept; the accept returns,
//! sees the flag, breaks; the dispatcher thread joins.

#[cfg(not(target_arch = "wasm32"))]
mod config;
pub mod kinds;
mod listener;
mod session;

pub use kinds::*;
pub use listener::TcpListenerActor;
pub use session::TcpSessionActor;
// `TcpListenerConfig` and `TcpSessionConfig` carry `std::net`
// types (native-only) so they live in `config` and only re-export
// under `not(target_arch = "wasm32")`. The actor markers themselves
// (above) are always-on so wasm callers can name them in
// [`TcpWasmExt::listener`] / [`TcpWasmExt::session`] type parameters.
#[cfg(not(target_arch = "wasm32"))]
pub use config::{TcpListenerConfig, TcpSessionConfig};

use aether_actor::{Addressable, WasmActorMailbox};
use aether_data::{ActorId, Tag, fold_lineage, with_tag};
// Always-on imports — every kind named in the ext-trait helpers
// must be reachable from wasm too so the `TcpWasmExt` impl
// compiles under `--target wasm32-unknown-unknown
// --no-default-features` (issue 832 acceptance criteria).
use aether_kinds::MonitorNotice;
#[cfg(not(target_arch = "wasm32"))]
use aether_substrate::actor::native::NativeActorMailbox;

/// ADR-0099 §3: the `MailboxId` of a tcp session — a grandchild of the
/// cap (cap → listener → session). The session's lineage is reconstructed
/// from the path of names and folded: `cap_carry` (the cap's own id —
/// it is depth-1, so id == carry) carries the listener node, then the
/// session node. Sessions are therefore *per-listener*: two listeners'
/// identically-named sessions get distinct ids, where the pre-0099 flat
/// `hash("aether.tcp.session:NAME")` form collided.
fn session_mailbox_id(cap_carry: u64, listener_name: &str, session_name: &str) -> u64 {
    let listener_carry = fold_lineage(
        cap_carry,
        ActorId::instanced(TcpListenerActor::NAMESPACE, listener_name),
    );
    let session_node = ActorId::instanced(TcpSessionActor::NAMESPACE, session_name);
    with_tag(Tag::Mailbox, fold_lineage(listener_carry, session_node))
}

/// Sender-side facade for FFI guests addressing
/// [`TcpCapability`] through a `ctx.actor::<TcpCapability>()`
/// handle.
///
/// Two distinct surfaces:
///
/// 1. Request helpers — [`bind_listener`](Self::bind_listener),
///    [`unbind_listener`](Self::unbind_listener),
///    [`list_listeners`](Self::list_listeners),
///    [`close`](Self::close), [`session_write`](Self::session_write),
///    [`session_close`](Self::session_close). Mirror
///    [`crate::fs::FsMailboxExt`] (issue 580): lift the cap-shaped
///    kinds (`Close`, `SessionWrite`, ...) one indirection above the
///    raw `.send(&Kind { .. })` so component code stops reconstructing
///    the struct (and the `.into()` ceremony) at every call site.
///    `close`, `session_write`, `session_close` internally resolve the
///    addressed listener / session actor — the request kind body itself
///    has no name field (the addressing rides the mailbox).
///
/// 2. Peer resolvers — [`listener::<R>`](Self::listener) and
///    [`session::<R>`](Self::session). Mirror
///    [`crate::component::ComponentHostWasmExt::loaded`] (issue 654):
///    the "aether.tcp.listener:" / "aether.tcp.session:" prefixes live
///    in exactly two methods in the workspace — these — so a future
///    namespace rename touches one constant ([`TcpListenerActor::NAMESPACE`]
///    / [`TcpSessionActor::NAMESPACE`]) and propagates everywhere.
///
/// All request methods are fire-and-forget. Replies arrive on the
/// matching `*Result` kinds (see ADR-0079 + the kind definitions in
/// `crate::tcp::kinds`). Synchronous wrappers (`bind_listener_sync`
/// etc.) were on the original issue 580 sketch — parked as a follow-up
/// so this PR stays mechanical.
///
/// The generic escape hatch is unaffected: `mailbox.send(&CustomKind { .. })`
/// still works for any `K` the cap declares via `HandlesKind<K>`, since
/// `send` is an inherent method on the underlying mailbox type.
pub trait TcpWasmExt {
    /// Mail `aether.tcp.bind_listener { addr, name }` to the cap.
    /// Reply: `BindListenerResult`. Pass `name = None` to let the cap
    /// default the subname to the bound port (typically with `addr =
    /// "127.0.0.1:0"` so the OS picks a free port).
    fn bind_listener(&self, addr: &str, name: Option<&str>);

    /// Mail `aether.tcp.unbind_listener { listener_name }` to the cap.
    /// Reply: `UnbindListenerResult` (asynchronous — the cap parks the
    /// reply until the listener's `MonitorNotice` arrives).
    fn unbind_listener(&self, listener_name: &str);

    /// Mail `aether.tcp.list_listeners` to the cap. Reply:
    /// `ListListenersResult`.
    fn list_listeners(&self);

    /// Mail `aether.tcp.close` to the named `TcpListenerActor`,
    /// asking it to shut down cooperatively. Equivalent to
    /// `self.listener::<TcpListenerActor>(listener_name).send(&Close::default())`.
    /// Fire-and-forget at the kind level; the close response rides via
    /// the cap's monitor on the listener, not via the `Close` kind.
    fn close(&self, listener_name: &str);

    /// Mail `aether.tcp.session_write { bytes }` to the named
    /// `TcpSessionActor`. The session's handler does a blocking write
    /// on the dispatcher thread. Fire-and-forget — failures surface
    /// via the session's close path, not via a reply to this send.
    fn session_write(&self, listener_name: &str, session_name: &str, bytes: &[u8]);

    /// Mail `aether.tcp.session_close` to the named `TcpSessionActor`,
    /// asking it to close gracefully. Fire-and-forget; the close
    /// fan-out fires `MonitorNotice` to the parent listener that spawned
    /// the session.
    fn session_close(&self, listener_name: &str, session_name: &str);

    /// Resolve a typed listener-instance mailbox for the bound
    /// listener named `name`. The full mailbox address is
    /// `format!("{}:{}", TcpListenerActor::NAMESPACE, name)`. `R` is
    /// the listener-side actor type (typically [`TcpListenerActor`]
    /// itself, but the type parameter lets callers address a custom
    /// wrapper that handles a different kind vocabulary on the same
    /// mailbox).
    fn listener<R: Addressable>(&self, name: &str) -> WasmActorMailbox<'_, R>;

    /// Resolve a typed session-instance mailbox for the open session
    /// named `name`. The full mailbox address is
    /// `format!("{}:{}", TcpSessionActor::NAMESPACE, name)`. See
    /// [`Self::listener`] for the `R` parameter shape.
    fn session<R: Addressable>(
        &self,
        listener_name: &str,
        session_name: &str,
    ) -> WasmActorMailbox<'_, R>;
}

impl TcpWasmExt for WasmActorMailbox<'_, TcpCapability> {
    //noinspection DuplicatedCode
    fn bind_listener(&self, addr: &str, name: Option<&str>) {
        self.send(&BindListener {
            addr: addr.into(),
            name: name.map(Into::into),
        });
    }
    fn unbind_listener(&self, listener_name: &str) {
        self.send(&UnbindListener {
            listener_name: listener_name.into(),
        });
    }
    fn list_listeners(&self) {
        self.send(&ListListeners::default());
    }
    fn close(&self, listener_name: &str) {
        self.listener::<TcpListenerActor>(listener_name)
            .send(&Close::default());
    }
    //noinspection DuplicatedCode
    fn session_write(&self, listener_name: &str, session_name: &str, bytes: &[u8]) {
        self.session::<TcpSessionActor>(listener_name, session_name)
            .send(&SessionWrite {
                bytes: bytes.to_vec(),
            });
    }
    fn session_close(&self, listener_name: &str, session_name: &str) {
        self.session::<TcpSessionActor>(listener_name, session_name)
            .send(&SessionClose::default());
    }
    fn listener<R: Addressable>(&self, name: &str) -> WasmActorMailbox<'_, R> {
        // ADR-0099 §3: a listener is this cap's child — fold its node
        // onto the cap's carry (the cap is depth-1, so `self`'s id is
        // its carry).
        self.resolve_peer_scoped::<R>(TcpListenerActor::NAMESPACE, name)
    }
    fn session<R: Addressable>(
        &self,
        listener_name: &str,
        session_name: &str,
    ) -> WasmActorMailbox<'_, R> {
        // The session id is folded by a custom scheme rather than by name, so
        // rewrap it with `at`, inheriting this cap handle's ctx binding so the
        // session handle's sends stamp the same origin (issue 1987).
        self.at::<R>(session_mailbox_id(
            self.mailbox_id().0,
            listener_name,
            session_name,
        ))
    }
}

/// Sender-side facade for native cap-to-cap callers addressing
/// [`TcpCapability`] through a `ctx.actor::<TcpCapability>()` handle
/// that returns a [`NativeActorMailbox`]. Same shape as [`TcpWasmExt`]
/// on the wasm transport — split into two traits because the listener /
/// session peer resolvers return [`NativeActorMailbox<'a, R>`] here
/// (with a transport-binding lifetime) vs [`WasmActorMailbox<R>`] on
/// FFI, and a single trait can't carry both signatures. The precedent
/// is [`crate::component::ComponentHostWasmExt`] /
/// [`crate::component::ComponentHostNativeExt`] (issue 654).
#[cfg(not(target_arch = "wasm32"))]
pub trait TcpNativeExt {
    /// Mail `aether.tcp.bind_listener { addr, name }` to the cap.
    fn bind_listener(&self, addr: &str, name: Option<&str>);

    /// Mail `aether.tcp.unbind_listener { listener_name }` to the cap.
    fn unbind_listener(&self, listener_name: &str);

    /// Mail `aether.tcp.list_listeners` to the cap.
    fn list_listeners(&self);

    /// Mail `aether.tcp.close` to the named `TcpListenerActor`.
    fn close(&self, listener_name: &str);

    /// Mail `aether.tcp.session_write { bytes }` to the named
    /// `TcpSessionActor`.
    fn session_write(&self, listener_name: &str, session_name: &str, bytes: &[u8]);

    /// Mail `aether.tcp.session_close` to the named `TcpSessionActor`.
    fn session_close(&self, listener_name: &str, session_name: &str);

    /// Resolve a typed listener-instance mailbox. See
    /// [`TcpWasmExt::listener`] for the addressing rationale; the
    /// returned handle inherits the parent mailbox's `'a` binding ref
    /// so `.send::<K>(&mail)` dispatches through the same
    /// `NativeBinding` without re-threading the ctx.
    fn listener<R: Addressable>(&self, name: &str) -> NativeActorMailbox<'_, R>;

    /// Resolve a typed session-instance mailbox. See
    /// [`TcpWasmExt::session`] for the addressing rationale.
    fn session<R: Addressable>(
        &self,
        listener_name: &str,
        session_name: &str,
    ) -> NativeActorMailbox<'_, R>;
}

#[cfg(not(target_arch = "wasm32"))]
impl TcpNativeExt for NativeActorMailbox<'_, TcpCapability> {
    //noinspection DuplicatedCode
    fn bind_listener(&self, addr: &str, name: Option<&str>) {
        self.send(&BindListener {
            addr: addr.into(),
            name: name.map(Into::into),
        });
    }
    fn unbind_listener(&self, listener_name: &str) {
        self.send(&UnbindListener {
            listener_name: listener_name.into(),
        });
    }
    fn list_listeners(&self) {
        self.send(&ListListeners::default());
    }
    fn close(&self, listener_name: &str) {
        self.listener::<TcpListenerActor>(listener_name)
            .send(&Close::default());
    }
    //noinspection DuplicatedCode
    fn session_write(&self, listener_name: &str, session_name: &str, bytes: &[u8]) {
        self.session::<TcpSessionActor>(listener_name, session_name)
            .send(&SessionWrite {
                bytes: bytes.to_vec(),
            });
    }
    fn session_close(&self, listener_name: &str, session_name: &str) {
        self.session::<TcpSessionActor>(listener_name, session_name)
            .send(&SessionClose::default());
    }
    fn listener<R: Addressable>(&self, name: &str) -> NativeActorMailbox<'_, R> {
        // ADR-0099 §3: fold the listener node onto the cap's carry (the
        // cap is depth-1, so `self`'s id is its carry).
        self.resolve_peer_scoped::<R>(TcpListenerActor::NAMESPACE, name)
    }
    fn session<R: Addressable>(
        &self,
        listener_name: &str,
        session_name: &str,
    ) -> NativeActorMailbox<'_, R> {
        NativeActorMailbox::__new(
            session_mailbox_id(self.mailbox_id().0, listener_name, session_name),
            self.binding(),
        )
    }
}

/// `aether.tcp` cap **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`), the
/// per-handler `HandlesKind` markers, and the singleton name-inventory entry,
/// all emitted always-on by `#[actor]`. The state-bearing runtime
/// (`TcpCapabilityState`, the cap's listener-fleet supervisor map) lives
/// behind the one `feature = "runtime"` gate, so a transport-only build never
/// names `TcpCapabilityState` nor pulls `aether_substrate` through this cap.
///
/// The cap is the supervisor of its listener fleet: it spawns listeners,
/// monitors them, and replies to unbind requests on their close. It holds its
/// own `MailboxId → ListenerEntry` map; it does NOT walk the chassis-wide
/// actor registry to enumerate children.
#[actor(singleton)]
pub struct TcpCapability;

// The `#[actor]` attribute path stays always-on (the macro divides what it
// emits). Everything that names an `aether_substrate` / `std::net` type — the
// handler/init ctx, the runtime state, the supervisor structs, and the
// `#[runtime] impl NativeActor` itself — lives in the `runtime` module below,
// gated once by `feature = "runtime"`. The handled kinds (`BindListener` /
// `UnbindListener` / `ListListeners`) stay always-on via `pub use kinds::*`
// and `MonitorNotice` via the always-on `aether_kinds` import above — the
// always-on `HandlesKind<K>` markers `#[actor]` emits name them.
use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        BindListener, BindListenerResult, ListListeners, ListListenersResult, TcpCapability,
        UnbindListener, UnbindListenerResult,
    };
    use crate::test_chassis::TestChassis;
    use aether_actor::Addressable;
    use aether_data::{Kind, SessionToken, Uuid};
    use aether_kinds::descriptors;
    use aether_kinds::trace::Nanos;
    use aether_substrate::chassis::builder::{Builder, PassiveChassis};
    use aether_substrate::mail::MailId;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
    use aether_substrate::mail::registry::OwnedDispatch;
    use aether_substrate::mail::registry::{MailboxEntry, Registry};
    use aether_substrate::mail::{MailRef, Source, SourceAddr};

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>, mpsc::Receiver<EgressEvent>) {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
        (registry, mailer, rx)
    }

    /// Boot a fresh substrate with `TcpCapability` registered as a
    /// passive actor and return the pieces every test in this
    /// module reaches for: the kind registry (for mailbox lookup
    /// in [`drive_and_decode`]), the egress receiver (for reply
    /// decode), and the [`PassiveChassis`] (held by the caller so
    /// the cap's actor thread stays alive for the test body).
    ///
    /// Collapses the previously-duplicated `fresh_substrate()` +
    /// `Builder::<TestChassis>::new(...)` chain that opened every
    /// test (issue 796).
    fn boot_tcp_substrate() -> (
        Arc<Registry>,
        mpsc::Receiver<EgressEvent>,
        PassiveChassis<TestChassis>,
    ) {
        let (registry, mailer, rx) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<TcpCapability>(())
            .build_passive()
            .expect("TcpCapability boots");
        (registry, rx, chassis)
    }

    fn session_reply() -> Source {
        Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xfeed))))
    }

    /// Push an encoded mail (via the kind's `encode_into_bytes`) at
    /// the cap's mailbox via the registered sink handler, then wait
    /// for the next outbound reply on `rx` and decode as `R`.
    fn drive_and_decode<K, R>(
        registry: &Arc<Registry>,
        rx: &mpsc::Receiver<EgressEvent>,
        cap_namespace: &str,
        mail: &K,
    ) -> R
    where
        K: Kind,
        R: Kind,
    {
        let id = registry
            .lookup(cap_namespace)
            .expect("cap mailbox registered");
        let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("cap entry") else {
            panic!("expected mailbox entry");
        };
        let bytes = mail.encode_into_bytes();
        handler.enqueue(OwnedDispatch::disarmed(
            K::ID,
            K::NAME.to_owned(),
            None,
            session_reply(),
            MailRef::from(bytes),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            aether_data::MailboxId(0),
        ));

        let deadline = Instant::now() + Duration::from_secs(2);
        let frame = loop {
            if let Ok(f) = rx.try_recv() {
                break f;
            }
            assert!(
                Instant::now() < deadline,
                "reply did not arrive within deadline for {}",
                K::NAME
            );
            thread::sleep(Duration::from_millis(5));
        };
        let payload = match frame {
            EgressEvent::ToSession { payload, .. } => payload,
            other => panic!("expected ToSession egress, got {other:?}"),
        };
        R::decode_from_bytes(&payload).expect("decode reply")
    }

    /// Issue 607 Phase 6a: bind → list → unbind round-trip on a
    /// loopback port. Asserts the cap-local supervisor map
    /// reflects every step (bound, listed, unbound).
    #[test]
    fn bind_then_list_then_unbind_roundtrip() {
        let (registry, rx, _chassis) = boot_tcp_substrate();

        // Bind to port 0 — let the OS pick a free port.
        let bind_reply: BindListenerResult = drive_and_decode(
            &registry,
            &rx,
            TcpCapability::NAMESPACE,
            &BindListener {
                addr: "127.0.0.1:0".into(),
                name: None,
            },
        );
        let (listener_name, local_port) = match bind_reply {
            BindListenerResult::Ok {
                listener_name,
                local_port,
                ..
            } => (listener_name, local_port),
            BindListenerResult::Err { reason, .. } => panic!("bind failed: {reason}"),
        };
        assert_eq!(
            listener_name,
            local_port.to_string(),
            "default subname should be the bound port",
        );
        assert!(local_port > 0, "OS-picked port should be non-zero");

        // List enumerates the one listener.
        let list_reply: ListListenersResult = drive_and_decode(
            &registry,
            &rx,
            TcpCapability::NAMESPACE,
            &ListListeners::default(),
        );
        assert_eq!(list_reply.listeners.len(), 1, "exactly one listener");
        let entry = &list_reply.listeners[0];
        assert_eq!(entry.name, listener_name);
        assert_eq!(entry.port, local_port);
        assert_eq!(entry.addr, "127.0.0.1:0");

        // Unbind — asynchronous reply via MonitorNotice.
        let unbind_reply: UnbindListenerResult = drive_and_decode(
            &registry,
            &rx,
            TcpCapability::NAMESPACE,
            &UnbindListener {
                listener_name: listener_name.clone(),
            },
        );
        match unbind_reply {
            UnbindListenerResult::Ok { listener_name: ln } => assert_eq!(ln, listener_name),
            UnbindListenerResult::Err { reason, .. } => panic!("unbind failed: {reason}"),
        }

        // List should now be empty — cap-local supervisor map
        // dropped the entry on MonitorNotice.
        let list_reply: ListListenersResult = drive_and_decode(
            &registry,
            &rx,
            TcpCapability::NAMESPACE,
            &ListListeners::default(),
        );
        assert!(
            list_reply.listeners.is_empty(),
            "list should drop the unbound listener",
        );
    }

    /// Binding the same port twice fails the second bind. Uses
    /// the first bind's actually-bound port to drive the second.
    #[test]
    fn bind_port_in_use_returns_err() {
        let (registry, rx, _chassis) = boot_tcp_substrate();

        let first: BindListenerResult = drive_and_decode(
            &registry,
            &rx,
            TcpCapability::NAMESPACE,
            &BindListener {
                addr: "127.0.0.1:0".into(),
                name: Some("first".into()),
            },
        );
        let local_port = match first {
            BindListenerResult::Ok { local_port, .. } => local_port,
            BindListenerResult::Err { reason, .. } => panic!("first bind failed: {reason}"),
        };

        // Second bind on the same port — must fail.
        let second: BindListenerResult = drive_and_decode(
            &registry,
            &rx,
            TcpCapability::NAMESPACE,
            &BindListener {
                addr: format!("127.0.0.1:{local_port}"),
                name: Some("second".into()),
            },
        );
        match second {
            BindListenerResult::Ok { .. } => panic!("expected port-in-use Err"),
            BindListenerResult::Err { reason, addr } => {
                assert_eq!(addr, format!("127.0.0.1:{local_port}"));
                assert!(
                    reason.starts_with("bind failed:"),
                    "expected bind-fail reason, got: {reason}",
                );
            }
        }
    }

    /// Unbind on an unknown name surfaces an Err with the name
    /// echoed back.
    #[test]
    fn unbind_unknown_listener_errors() {
        let (registry, rx, _chassis) = boot_tcp_substrate();

        let reply: UnbindListenerResult = drive_and_decode(
            &registry,
            &rx,
            TcpCapability::NAMESPACE,
            &UnbindListener {
                listener_name: "nope".into(),
            },
        );
        match reply {
            UnbindListenerResult::Err { listener_name, .. } => {
                assert_eq!(listener_name, "nope");
            }
            UnbindListenerResult::Ok { .. } => panic!("expected Err for unknown listener"),
        }
    }

    // Pre-#775 the session round-trip test asserted that
    // SessionData / SessionClosed broadcasts arrived at the egress
    // after a real TCP client wrote then dropped. Issue 775 retired
    // the BroadcastCapability + observation fan-out, so the
    // session actor no longer publishes those kinds — the test was
    // deleted with the broadcasts.

    /// Two concurrent binds on different ports both surface in
    /// `ListListeners`.
    #[test]
    fn list_enumerates_two_concurrent_listeners() {
        let (registry, rx, _chassis) = boot_tcp_substrate();

        let _: BindListenerResult = drive_and_decode(
            &registry,
            &rx,
            TcpCapability::NAMESPACE,
            &BindListener {
                addr: "127.0.0.1:0".into(),
                name: Some("admin".into()),
            },
        );
        let _: BindListenerResult = drive_and_decode(
            &registry,
            &rx,
            TcpCapability::NAMESPACE,
            &BindListener {
                addr: "127.0.0.1:0".into(),
                name: Some("game".into()),
            },
        );

        let list: ListListenersResult = drive_and_decode(
            &registry,
            &rx,
            TcpCapability::NAMESPACE,
            &ListListeners::default(),
        );
        let mut names: Vec<String> = list.listeners.iter().map(|l| l.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["admin".to_string(), "game".to_string()]);
    }
}
