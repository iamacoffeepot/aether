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

mod listener;
mod session;

pub use listener::TcpListenerActor;
pub use session::TcpSessionActor;
// `TcpListenerConfig` and `TcpSessionConfig` carry `std::net`
// types (native-only) so they live inside the native bridge mod
// and only re-export under `not(target_arch = "wasm32")`. The
// actor markers themselves (above) are always-on so wasm callers
// can name them in [`TcpFfiExt::listener`] / [`TcpFfiExt::session`]
// type parameters.
#[cfg(not(target_arch = "wasm32"))]
pub use listener::TcpListenerConfig;
#[cfg(not(target_arch = "wasm32"))]
pub use session::TcpSessionConfig;

use aether_actor::{Actor, FfiActorMailbox};
use aether_data::{ActorId, Tag, fold_lineage, with_tag};
// Always-on imports — every kind named in the ext-trait helpers
// must be reachable from wasm too so the `TcpFfiExt` impl
// compiles under `--target wasm32-unknown-unknown
// --no-default-features` (issue 832 acceptance criteria).
use aether_kinds::{
    BindListener, Close, ListListeners, MonitorNotice, SessionClose, SessionWrite, UnbindListener,
};
// Reply / payload kinds only consumed by native handler bodies. Gated to
// avoid an unused-import warning on wasm32 where the bridge stub doesn't
// reference them.
#[cfg(not(target_arch = "wasm32"))]
use aether_kinds::{BindListenerResult, ListListenersResult, ListenerInfo, UnbindListenerResult};
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
///    [`crate::component::ComponentHostFfiExt::loaded`] (issue 654):
///    the "aether.tcp.listener:" / "aether.tcp.session:" prefixes live
///    in exactly two methods in the workspace — these — so a future
///    namespace rename touches one constant ([`TcpListenerActor::NAMESPACE`]
///    / [`TcpSessionActor::NAMESPACE`]) and propagates everywhere.
///
/// All request methods are fire-and-forget. Replies arrive on the
/// matching `*Result` kinds (see ADR-0079 + the kind definitions in
/// `aether_kinds::tcp`). Synchronous wrappers (`bind_listener_sync`
/// etc.) were on the original issue 580 sketch — parked as a follow-up
/// so this PR stays mechanical.
///
/// The generic escape hatch is unaffected: `mailbox.send(&CustomKind { .. })`
/// still works for any `K` the cap declares via `HandlesKind<K>`, since
/// `send` is an inherent method on the underlying mailbox type.
pub trait TcpFfiExt {
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
    fn listener<R: Actor>(&self, name: &str) -> FfiActorMailbox<R>;

    /// Resolve a typed session-instance mailbox for the open session
    /// named `name`. The full mailbox address is
    /// `format!("{}:{}", TcpSessionActor::NAMESPACE, name)`. See
    /// [`Self::listener`] for the `R` parameter shape.
    fn session<R: Actor>(&self, listener_name: &str, session_name: &str) -> FfiActorMailbox<R>;
}

impl TcpFfiExt for FfiActorMailbox<TcpCapability> {
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
    fn listener<R: Actor>(&self, name: &str) -> FfiActorMailbox<R> {
        // ADR-0099 §3: a listener is this cap's child — fold its node
        // onto the cap's carry (the cap is depth-1, so `self`'s id is
        // its carry).
        self.resolve_peer_scoped::<R>(TcpListenerActor::NAMESPACE, name)
    }
    fn session<R: Actor>(&self, listener_name: &str, session_name: &str) -> FfiActorMailbox<R> {
        FfiActorMailbox::__new(session_mailbox_id(
            self.mailbox_id().0,
            listener_name,
            session_name,
        ))
    }
}

/// Sender-side facade for native cap-to-cap callers addressing
/// [`TcpCapability`] through a `ctx.actor::<TcpCapability>()` handle
/// that returns a [`NativeActorMailbox`]. Same shape as [`TcpFfiExt`]
/// on the wasm transport — split into two traits because the listener /
/// session peer resolvers return [`NativeActorMailbox<'a, R>`] here
/// (with a transport-binding lifetime) vs [`FfiActorMailbox<R>`] on
/// FFI, and a single trait can't carry both signatures. The precedent
/// is [`crate::component::ComponentHostFfiExt`] /
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
    /// [`TcpFfiExt::listener`] for the addressing rationale; the
    /// returned handle inherits the parent mailbox's `'a` binding ref
    /// so `.send::<K>(&mail)` dispatches through the same
    /// `NativeBinding` without re-threading the ctx.
    fn listener<R: Actor>(&self, name: &str) -> NativeActorMailbox<'_, R>;

    /// Resolve a typed session-instance mailbox. See
    /// [`TcpFfiExt::session`] for the addressing rationale.
    fn session<R: Actor>(
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
    fn listener<R: Actor>(&self, name: &str) -> NativeActorMailbox<'_, R> {
        // ADR-0099 §3: fold the listener node onto the cap's carry (the
        // cap is depth-1, so `self`'s id is its carry).
        self.resolve_peer_scoped::<R>(TcpListenerActor::NAMESPACE, name)
    }
    fn session<R: Actor>(
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

#[aether_actor::bridge(singleton)]
mod cap_native {
    // Trait-marker kinds the wasm32 bridge stub needs to satisfy
    // HandlesKind; these stay always-imported.
    use super::{BindListener, ListListeners, MonitorNotice, UnbindListener};
    // Reply / payload kinds + native-only types (TcpListenerActor /
    // TcpListenerConfig) only consumed by native handler bodies. The
    // wasm32 stub bridge emits doesn't reference them, so they're
    // gated to avoid an unused-import warning.
    #[cfg(not(target_arch = "wasm32"))]
    use super::{
        BindListenerResult, Close, ListListenersResult, ListenerInfo, TcpListenerActor,
        TcpListenerConfig, UnbindListenerResult,
    };
    use aether_actor::actor;
    use aether_actor::actor::ctx::OutboundReply;
    use aether_substrate::actor::monitor::MonitorHandle;
    use aether_substrate::actor::native::spawn::Subname;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use std::collections::HashMap;
    use std::net::TcpListener;

    /// Singleton control-plane cap. Owns the listener fleet directly
    /// — the cap is the supervisor, not a thin shim over the chassis
    /// registry. Each spawn registers a monitor on the new listener
    /// and inserts a `ListenerEntry` into the cap-local map; the
    /// `on_monitor_notice` handler removes the entry on listener
    /// close.
    ///
    /// Issue 629 / Phase B: plain `HashMap` fields. The dispatcher
    /// thread is the sole writer / reader; pre-Phase-A's
    /// `Mutex<HashMap<...>>` was a worker-pool-era tax, not a
    /// contention point.
    pub struct TcpCapability {
        /// Live listeners spawned by this cap. Key is the listener's
        /// full-name `MailboxId`. Each entry holds the bind metadata
        /// surfaced via `ListListeners` plus the monitor handle that
        /// pins the cap's monitor on the listener until close.
        listeners: HashMap<aether_data::MailboxId, ListenerEntry>,
        /// Outstanding unbind replies parked until `MonitorNotice`
        /// arrives from the listener being closed. Key is the same
        /// `MailboxId` as `listeners`; the cap's monitor (registered
        /// at spawn time) is what fires the notice.
        pending_unbinds: HashMap<aether_data::MailboxId, PendingUnbind>,
    }

    /// Cap-local supervisor state for one live listener. Drops with
    /// the entry; `MonitorHandle::Drop` is idempotent with the close
    /// path's index drain.
    struct ListenerEntry {
        addr: String,
        port: u16,
        name: String,
        // Held to keep the cap's monitor registered against the
        // listener for its lifetime. Drops when the entry is removed
        // (in `on_monitor_notice`).
        _monitor_handle: MonitorHandle,
    }

    struct PendingUnbind {
        sender: aether_data::Source,
        listener_name: String,
    }

    #[actor]
    impl NativeActor for TcpCapability {
        type Config = ();
        const NAMESPACE: &'static str = "aether.tcp";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self {
                listeners: HashMap::new(),
                pending_unbinds: HashMap::new(),
            })
        }

        /// Spawn a fresh `TcpListenerActor` bound to `mail.addr`.
        ///
        /// Binds the socket on the dispatcher thread (so a bind
        /// failure replies `Err` synchronously), then hands the bound
        /// listener through `spawn_child`. After spawn the cap
        /// registers a monitor and inserts the listener into its
        /// supervisor map.
        ///
        /// # Agent
        /// Reply: `BindListenerResult`. `Ok` on successful bind +
        /// spawn; `Err` on addr parse / bind / spawn / monitor failure.
        #[handler]
        fn on_bind(&mut self, ctx: &mut NativeCtx<'_>, mail: BindListener) {
            let listener = match TcpListener::bind(&mail.addr) {
                Ok(l) => l,
                Err(e) => {
                    ctx.reply(&BindListenerResult::Err {
                        addr: mail.addr,
                        reason: format!("bind failed: {e}"),
                    });
                    return;
                }
            };
            let local_port = match listener.local_addr() {
                Ok(addr) => addr.port(),
                Err(e) => {
                    ctx.reply(&BindListenerResult::Err {
                        addr: mail.addr,
                        reason: format!("local_addr failed: {e}"),
                    });
                    return;
                }
            };
            let subname_str = mail.name.clone().unwrap_or_else(|| format!("{local_port}"));

            let listener_id = match ctx
                .spawn_child::<TcpListenerActor>(
                    Subname::Named(&subname_str),
                    TcpListenerConfig {
                        listener: Some(listener),
                        addr: mail.addr.clone(),
                        port: local_port,
                    },
                )
                .finish()
            {
                Ok(id) => id,
                Err(e) => {
                    ctx.reply(&BindListenerResult::Err {
                        addr: mail.addr,
                        reason: format!("spawn failed: {e:?}"),
                    });
                    return;
                }
            };

            // Register the cap's monitor on the freshly-spawned
            // listener. The monitor pins until the entry is removed
            // (in on_monitor_notice).
            let monitor_handle = match ctx.monitor(listener_id) {
                Ok(h) => h,
                Err(e) => {
                    // Listener spawned but monitor failed — extremely
                    // unlikely (listener was just inserted Live). Reply
                    // Err and let the listener live; chassis shutdown
                    // will reap it.
                    ctx.reply(&BindListenerResult::Err {
                        addr: mail.addr,
                        reason: format!("monitor failed: {e:?}"),
                    });
                    return;
                }
            };

            self.listeners.insert(
                listener_id,
                ListenerEntry {
                    addr: mail.addr,
                    port: local_port,
                    name: subname_str.clone(),
                    _monitor_handle: monitor_handle,
                },
            );

            ctx.reply(&BindListenerResult::Ok {
                listener_name: subname_str,
                listener_id,
                local_port,
            });
        }

        /// Mail `Close` to the named listener and park the
        /// originator's reply target. Reply fires from
        /// `on_monitor_notice` once the listener tombstones.
        ///
        /// # Agent
        /// Reply: `UnbindListenerResult`. Asynchronous — the response
        /// fires after the listener's accept thread joins and its
        /// `MonitorNotice` arrives at this cap.
        #[handler]
        fn on_unbind(&mut self, ctx: &mut NativeCtx<'_>, mail: UnbindListener) {
            // Resolve listener_id from the cap-local supervisor map by
            // name. The cap is the source of truth for "what listeners
            // exist"; no registry walk needed.
            let listener_id = self
                .listeners
                .iter()
                .find(|(_, entry)| entry.name == mail.listener_name)
                .map(|(id, _)| *id);
            let Some(listener_id) = listener_id else {
                ctx.reply(&UnbindListenerResult::Err {
                    listener_name: mail.listener_name,
                    reason: "no such listener (or already closed)".into(),
                });
                return;
            };
            // Park the reply target keyed on listener_id. The cap's
            // already-registered monitor (set at spawn time) fires
            // MonitorNotice on close, which drives the reply.
            self.pending_unbinds.insert(
                listener_id,
                PendingUnbind {
                    sender: ctx.reply_target(),
                    listener_name: mail.listener_name,
                },
            );
            // Mail Close to the listener by its stored id. ADR-0099 §3:
            // the listener is a spawned child, so its id is the lineage
            // fold, not `hash(NAMESPACE:name)` — re-resolving by name
            // would reach a flat id nothing is registered under. The cap
            // already holds the folded id from the spawn (the
            // `self.listeners` key), so address it directly.
            ctx.actor_at::<TcpListenerActor>(listener_id)
                .send(&Close::default());
        }

        /// Walk the cap-local listener map and report metadata.
        ///
        /// # Agent
        /// Reply: `ListListenersResult`.
        #[handler]
        fn on_list(&mut self, ctx: &mut NativeCtx<'_>, _mail: ListListeners) {
            let listeners: Vec<ListenerInfo> = self
                .listeners
                .values()
                .map(|entry| ListenerInfo {
                    name: entry.name.clone(),
                    addr: entry.addr.clone(),
                    port: entry.port,
                })
                .collect();
            ctx.reply(&ListListenersResult { listeners });
        }

        /// Listener tombstoned — remove from the supervisor map and
        /// fire the parked unbind reply if one is waiting.
        ///
        /// `MonitorNotice.target` identifies which listener closed.
        /// The cap's monitor on every spawned listener (registered in
        /// `on_bind`) fires this notice; if the close came from an
        /// unbind request, `pending_unbinds` has an entry with the
        /// originator to reply to.
        #[handler]
        fn on_monitor_notice(&mut self, ctx: &mut NativeCtx<'_>, notice: MonitorNotice) {
            // Drop the supervisor entry. The held MonitorHandle drops
            // here; deregister is idempotent with the close path's
            // forward-index drain.
            let _entry = self.listeners.remove(&notice.target);
            // Fire the parked unbind reply if one was waiting.
            let parked = self.pending_unbinds.remove(&notice.target);
            if let Some(parked) = parked {
                ctx.reply_to(
                    parked.sender,
                    &UnbindListenerResult::Ok {
                        listener_name: parked.listener_name,
                    },
                );
            }
            // Else: notice came from a non-unbind close (chassis
            // shutdown, future trap). Nothing to reply to; the
            // supervisor entry is gone, that's the cleanup.
        }
    }

    #[cfg(test)]
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
        use aether_actor::Actor;
        use aether_data::{Kind, SessionToken, Uuid};
        use aether_kinds::descriptors;
        use aether_kinds::trace::Nanos;
        use aether_substrate::chassis::builder::{Builder, PassiveChassis};
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::MailId;
        use aether_substrate::mail::mailer::Mailer;
        use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
        use aether_substrate::mail::registry::OwnedDispatch;
        use aether_substrate::mail::registry::{MailboxEntry, Registry};
        use aether_substrate::mail::{MailRef, Source, SourceAddr};
        use serde::de::DeserializeOwned;

        fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>, mpsc::Receiver<EgressEvent>) {
            let registry = Arc::new(Registry::new());
            for d in descriptors::all() {
                let _ = registry.register_kind_with_descriptor(d);
            }
            let (outbound, rx) = HubOutbound::attached_loopback();
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let mailer =
                Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
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
            R: DeserializeOwned,
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
            postcard::from_bytes(&payload).expect("decode reply")
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
}
