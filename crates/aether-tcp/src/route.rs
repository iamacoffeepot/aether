//! Sender-side peer-addressing facades for the `aether.tcp` cluster —
//! the "routing" seam of the [`TcpCapability`] control plane.

use aether_actor::WasmActorMailbox;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::NativeActorMailbox;

use super::{
    BindListener, Close, Connect, ListListeners, SessionClose, SessionWrite, TcpCapability, TcpListenerActor,
    TcpSessionActor, UnbindListener,
};

/// Sender-side facade for FFI guests addressing
/// [`TcpCapability`] through a `ctx.actor::<TcpCapability>()`
/// handle.
///
/// Two distinct surfaces:
///
/// 1. Request helpers — [`connect`](Self::connect),
///    [`bind_listener`](Self::bind_listener),
///    [`unbind_listener`](Self::unbind_listener),
///    [`list_listeners`](Self::list_listeners),
///    [`close`](Self::close), [`session_write`](Self::session_write),
///    [`session_close`](Self::session_close),
///    [`connect_session_write`](Self::connect_session_write), and
///    [`connect_session_close`](Self::connect_session_close). Mirror
///    `aether_fs::FsMailboxExt` (issue 580): lift the cap-shaped
///    kinds (`Close`, `SessionWrite`, ...) one indirection above the
///    raw `.send(&Kind { .. })` so component code stops reconstructing
///    the struct (and the `.into()` ceremony) at every call site.
///    `close`, `session_write`, `session_close` internally resolve the
///    addressed listener / session actor — the request kind body itself
///    has no name field (the addressing rides the mailbox).
///
/// 2. Peer resolvers — [`listener`](Self::listener),
///    [`session`](Self::session), and
///    [`connect_session`](Self::connect_session). Each walks the
///    declared [`TcpCapability`] → [`TcpListenerActor`] /
///    [`TcpSessionActor`] edges through typed child resolution and
///    returns the child handle that resolution produces.
///
/// All request methods are fire-and-forget. Replies arrive on the
/// matching `*Result` kinds (see ADR-0079 + the kind definitions in
/// [`crate::kinds`]). Synchronous wrappers (`bind_listener_sync`
/// etc.) were on the original issue 580 sketch — parked as a follow-up
/// so this PR stays mechanical.
///
/// The generic escape hatch is unaffected: `mailbox.send(&CustomKind { .. })`
/// still works for any `K` the cap declares via `HandlesKind<K>`, since
/// `send` is an inherent method on the underlying mailbox type.
pub trait TcpWasmExt {
    /// Mail `aether.tcp.connect { addr, name, consumer }` to the cap.
    /// Reply: `ConnectResult`. Pass `name = None` for a `conn-N`
    /// subname. Pass `consumer` to receive framed session data and
    /// close notices at that mailbox — `ctx.self_id()` to receive them
    /// yourself.
    fn connect(&self, addr: &str, name: Option<&str>, consumer: Option<aether_data::MailboxId>);

    /// Mail `aether.tcp.bind_listener { addr, name, consumer }` to the cap.
    /// Reply: `BindListenerResult`. Pass `name = None` to let the cap
    /// default the subname to the bound port (typically with `addr =
    /// "127.0.0.1:0"` so the OS picks a free port). Pass `consumer` to
    /// receive every accepted session's framed data and close notices at
    /// that mailbox — `ctx.self_id()` to receive them yourself.
    fn bind_listener(&self, addr: &str, name: Option<&str>, consumer: Option<aether_data::MailboxId>);

    /// Mail `aether.tcp.unbind_listener { listener_name }` to the cap.
    /// Reply: `UnbindListenerResult` (asynchronous — the cap parks the
    /// reply until the listener's `MonitorNotice` arrives).
    fn unbind_listener(&self, listener_name: &str);

    /// Mail `aether.tcp.list_listeners` to the cap. Reply:
    /// `ListListenersResult`.
    fn list_listeners(&self);

    /// Mail `aether.tcp.close` to the named `TcpListenerActor`,
    /// asking it to shut down cooperatively. Equivalent to
    /// `self.listener(listener_name).send(&Close::default())`.
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

    /// Mail `aether.tcp.session_write { bytes }` to a connect-side
    /// `TcpSessionActor` that is a direct child of this cap.
    fn connect_session_write(&self, name: &str, bytes: &[u8]);

    /// Mail `aether.tcp.session_close` to a connect-side session.
    fn connect_session_close(&self, name: &str);

    /// Resolve the [`TcpListenerActor`] handle for the bound listener
    /// named `name`, directly beneath [`TcpCapability`].
    fn listener(&self, name: &str) -> WasmActorMailbox<'_, TcpListenerActor>;

    /// Resolve the [`TcpSessionActor`] handle for the open session named
    /// `session_name` beneath the listener named `listener_name`.
    fn session(&self, listener_name: &str, session_name: &str) -> WasmActorMailbox<'_, TcpSessionActor>;

    /// Resolve a connect-side [`TcpSessionActor`] handle. Unlike
    /// [`Self::session`], this folds cap → session directly.
    fn connect_session(&self, name: &str) -> WasmActorMailbox<'_, TcpSessionActor>;
}

impl TcpWasmExt for WasmActorMailbox<'_, TcpCapability> {
    fn connect(&self, addr: &str, name: Option<&str>, consumer: Option<aether_data::MailboxId>) {
        self.send(&Connect { addr: addr.into(), name: name.map(Into::into), consumer });
    }
    fn bind_listener(&self, addr: &str, name: Option<&str>, consumer: Option<aether_data::MailboxId>) {
        self.send(&BindListener { addr: addr.into(), name: name.map(Into::into), consumer });
    }
    fn unbind_listener(&self, listener_name: &str) {
        self.send(&UnbindListener { listener_name: listener_name.into() });
    }
    fn list_listeners(&self) {
        self.send(&ListListeners::default());
    }
    fn close(&self, listener_name: &str) {
        self.listener(listener_name).send(&Close::default());
    }
    fn session_write(&self, listener_name: &str, session_name: &str, bytes: &[u8]) {
        self.session(listener_name, session_name).send(&SessionWrite { bytes: bytes.to_vec() });
    }
    fn session_close(&self, listener_name: &str, session_name: &str) {
        self.session(listener_name, session_name).send(&SessionClose::default());
    }
    fn connect_session_write(&self, name: &str, bytes: &[u8]) {
        self.connect_session(name).send(&SessionWrite { bytes: bytes.to_vec() });
    }
    fn connect_session_close(&self, name: &str) {
        self.connect_session(name).send(&SessionClose::default());
    }
    fn listener(&self, name: &str) -> WasmActorMailbox<'_, TcpListenerActor> {
        self.resolve::<TcpListenerActor>(name)
    }
    fn session(&self, listener_name: &str, session_name: &str) -> WasmActorMailbox<'_, TcpSessionActor> {
        self.resolve::<TcpListenerActor>(listener_name).resolve::<TcpSessionActor>(session_name)
    }
    fn connect_session(&self, name: &str) -> WasmActorMailbox<'_, TcpSessionActor> {
        self.resolve::<TcpSessionActor>(name)
    }
}

/// Sender-side facade for native cap-to-cap callers addressing
/// [`TcpCapability`] through a `ctx.actor::<TcpCapability>()` handle
/// that returns a [`NativeActorMailbox`]. Same shape as [`TcpWasmExt`]
/// on the wasm transport — split into two traits because the listener /
/// session peer resolvers return [`NativeActorMailbox<'a, R>`] here
/// (with a transport-binding lifetime) vs [`WasmActorMailbox<R>`] on
/// FFI, and a single trait can't carry both signatures (issue 654).
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub trait TcpNativeExt {
    /// Mail `aether.tcp.connect { addr, name, consumer }` to the cap.
    /// Pass `consumer` to receive framed session data and close notices
    /// at that mailbox — `ctx.self_id()` to receive them yourself.
    fn connect(&self, addr: &str, name: Option<&str>, consumer: Option<aether_data::MailboxId>);

    /// Mail `aether.tcp.bind_listener { addr, name, consumer }` to the cap.
    /// Pass `consumer` to receive every accepted session's framed data
    /// and close notices at that mailbox — `ctx.self_id()` to receive
    /// them yourself.
    fn bind_listener(&self, addr: &str, name: Option<&str>, consumer: Option<aether_data::MailboxId>);

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

    /// Mail `aether.tcp.session_write { bytes }` to a connect-side session.
    fn connect_session_write(&self, name: &str, bytes: &[u8]);

    /// Mail `aether.tcp.session_close` to a connect-side session.
    fn connect_session_close(&self, name: &str);

    /// Resolve the [`TcpListenerActor`] handle for the bound listener
    /// named `name`. The returned handle inherits the parent mailbox's
    /// `'a` binding ref so `.send::<K>(&mail)` dispatches through the same
    /// `NativeBinding` without re-threading the ctx.
    fn listener(&self, name: &str) -> NativeActorMailbox<'_, TcpListenerActor>;

    /// Resolve the [`TcpSessionActor`] handle for the open session named
    /// `session_name` beneath the listener named `listener_name`.
    fn session(&self, listener_name: &str, session_name: &str) -> NativeActorMailbox<'_, TcpSessionActor>;

    /// Resolve a connect-side [`TcpSessionActor`] handle. This folds the
    /// session directly beneath the cap, without a listener node.
    fn connect_session(&self, name: &str) -> NativeActorMailbox<'_, TcpSessionActor>;
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl TcpNativeExt for NativeActorMailbox<'_, TcpCapability> {
    fn connect(&self, addr: &str, name: Option<&str>, consumer: Option<aether_data::MailboxId>) {
        self.send(&Connect { addr: addr.into(), name: name.map(Into::into), consumer });
    }
    fn bind_listener(&self, addr: &str, name: Option<&str>, consumer: Option<aether_data::MailboxId>) {
        self.send(&BindListener { addr: addr.into(), name: name.map(Into::into), consumer });
    }
    fn unbind_listener(&self, listener_name: &str) {
        self.send(&UnbindListener { listener_name: listener_name.into() });
    }
    fn list_listeners(&self) {
        self.send(&ListListeners::default());
    }
    fn close(&self, listener_name: &str) {
        self.listener(listener_name).send(&Close::default());
    }
    fn session_write(&self, listener_name: &str, session_name: &str, bytes: &[u8]) {
        self.session(listener_name, session_name).send(&SessionWrite { bytes: bytes.to_vec() });
    }
    fn session_close(&self, listener_name: &str, session_name: &str) {
        self.session(listener_name, session_name).send(&SessionClose::default());
    }
    fn connect_session_write(&self, name: &str, bytes: &[u8]) {
        self.connect_session(name).send(&SessionWrite { bytes: bytes.to_vec() });
    }
    fn connect_session_close(&self, name: &str) {
        self.connect_session(name).send(&SessionClose::default());
    }
    fn listener(&self, name: &str) -> NativeActorMailbox<'_, TcpListenerActor> {
        self.resolve::<TcpListenerActor>(name)
    }
    fn session(&self, listener_name: &str, session_name: &str) -> NativeActorMailbox<'_, TcpSessionActor> {
        self.resolve::<TcpListenerActor>(listener_name).resolve::<TcpSessionActor>(session_name)
    }
    fn connect_session(&self, name: &str) -> NativeActorMailbox<'_, TcpSessionActor> {
        self.resolve::<TcpSessionActor>(name)
    }
}

#[cfg(test)]
mod tests {
    use aether_actor::wasm::NO_INBOUND_SOURCE;
    use aether_actor::wasm::inline::Registry;
    use aether_actor::{Addressable, Erased, Manual, WasmCtx};
    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    use aether_data::{MailId, Source};
    use aether_data::{MailboxId, mailbox_id_from_path};
    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    use aether_substrate::actor::native::NativeCtx;
    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    use aether_substrate::testing::{bare_substrate, unrouted_binding};

    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    use super::TcpNativeExt;
    use super::{TcpCapability, TcpListenerActor, TcpSessionActor, TcpWasmExt};

    const LISTENER_NAME: &str = "game";
    const SHARED_SESSION_NAME: &str = "shared";

    #[allow(
        clippy::disallowed_methods,
        reason = "route tests compare typed facade ids with the canonical rendered lineage boundary"
    )]
    fn canonical_mailbox_id(path: &str) -> MailboxId {
        mailbox_id_from_path(path)
    }

    fn canonical_listener_mailbox_id() -> MailboxId {
        canonical_mailbox_id(&format!("{}/{}:{LISTENER_NAME}", TcpCapability::NAMESPACE, TcpListenerActor::NAMESPACE))
    }

    fn canonical_accepted_route_mailbox_id() -> MailboxId {
        canonical_mailbox_id(&format!(
            "{}/{}:{LISTENER_NAME}/{}:{SHARED_SESSION_NAME}",
            TcpCapability::NAMESPACE,
            TcpListenerActor::NAMESPACE,
            TcpSessionActor::NAMESPACE,
        ))
    }

    fn canonical_outbound_route_mailbox_id() -> MailboxId {
        canonical_mailbox_id(&format!(
            "{}/{}:{SHARED_SESSION_NAME}",
            TcpCapability::NAMESPACE,
            TcpSessionActor::NAMESPACE,
        ))
    }

    fn assert_canonical_route_ids(
        listener_mailbox_id: MailboxId,
        accepted_route_mailbox_id: MailboxId,
        outbound_route_mailbox_id: MailboxId,
    ) {
        assert_eq!(listener_mailbox_id, canonical_listener_mailbox_id());
        assert_eq!(accepted_route_mailbox_id, canonical_accepted_route_mailbox_id());
        assert_eq!(outbound_route_mailbox_id, canonical_outbound_route_mailbox_id());
        assert_ne!(
            accepted_route_mailbox_id, outbound_route_mailbox_id,
            "the same session discriminator beneath listener and capability parents must remain distinct",
        );
    }

    #[test]
    fn wasm_facade_resolves_each_canonical_tcp_lineage() {
        let registry = Registry::new();
        let ctx: WasmCtx<'_, Erased, Manual> = WasmCtx::__new(0, &registry, NO_INBOUND_SOURCE);
        let capability = ctx.actor::<TcpCapability>();

        assert_canonical_route_ids(
            capability.listener(LISTENER_NAME).mailbox_id(),
            capability.session(LISTENER_NAME, SHARED_SESSION_NAME).mailbox_id(),
            capability.connect_session(SHARED_SESSION_NAME).mailbox_id(),
        );
    }

    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    #[test]
    fn native_facade_resolves_each_canonical_tcp_lineage() {
        let (_, mailer) = bare_substrate();
        let binding = unrouted_binding(&mailer);
        let ctx = NativeCtx::new_dispatching(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let capability = ctx.actor::<TcpCapability>();

        assert_canonical_route_ids(
            capability.listener(LISTENER_NAME).mailbox_id(),
            capability.session(LISTENER_NAME, SHARED_SESSION_NAME).mailbox_id(),
            capability.connect_session(SHARED_SESSION_NAME).mailbox_id(),
        );
    }
}
