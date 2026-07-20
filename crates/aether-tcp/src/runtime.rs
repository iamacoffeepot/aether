//! The `aether.tcp` cap runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! [`TcpCapability`](super::TcpCapability) identity never names these types
//! nor pulls `aether_substrate`. The substrate / `std::net`-typed imports are
//! gated once by this module rather than line-by-line; the `#[actor] impl`
//! reaches the state, ctx types, and supervisor structs through the single
//! `use runtime::*` glob in the parent.

pub use std::collections::HashMap;
pub use std::collections::hash_map::Entry;
pub use std::net::{TcpListener, TcpStream};
pub use std::sync::Arc;
pub use std::sync::mpsc;
pub use std::thread;

pub use aether_actor::Manual;
// The manual handlers issue their own replies through `ctx.reply` /
// `ctx.reply_to`, the `OutboundReply` trait methods, so the trait must be in
// scope where those handler bodies expand.
pub use aether_actor::OutboundReply;
pub use aether_substrate::actor::monitor::MonitorHandle;
pub use aether_substrate::actor::native::spawn::Subname;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::runtime::trace::SettlementHold;
pub use aether_substrate::{KindId, Mail, Mailer};

pub use aether_data::Kind;

use aether_actor::runtime;
// `MonitorNotice` is named by `on_monitor_notice`'s signature; the parent's
// import of it is private, so re-import it directly where the body expands.
use aether_kinds::MonitorNotice;
// The moved handler bodies name the cap kinds (`BindListener`, `Close`,
// `ListenerInfo`, …) and the listener child actor + its config; bring them in
// from the parent module where they live always-on.
#[allow(clippy::wildcard_imports)]
use super::kinds::*;
use super::{TcpCapability, TcpListenerActor, TcpListenerConfig, TcpSessionActor, TcpSessionConfig};

/// `aether.tcp` runtime state (issue 607 Phase 6a, ADR-0079). The singleton
/// control-plane cap owns its listener fleet directly — it is the supervisor,
/// not a thin shim over the chassis registry. Each `on_bind` registers a
/// monitor on the new listener and inserts a [`ListenerEntry`] into
/// `listeners`; `on_monitor_notice` removes the entry on listener close.
/// The addressing identity is the distinct ZST
/// [`TcpCapability`](super::TcpCapability). Living in this private module keeps
/// it `pub`-enough to satisfy the `NativeActor::State` interface without
/// exposing it as crate-public API.
///
/// Issue 629 / Phase B: plain `HashMap` fields. The dispatcher thread is the
/// sole writer / reader; pre-Phase-A's `Mutex<HashMap<...>>` was a
/// worker-pool-era tax, not a contention point.
pub struct TcpCapabilityState {
    /// Live listeners spawned by this cap. Key is the listener's
    /// full-name `MailboxId`. Each entry holds the bind metadata
    /// surfaced via `ListListeners` plus the monitor handle that
    /// pins the cap's monitor on the listener until close.
    pub listeners: HashMap<aether_data::MailboxId, ListenerEntry>,
    /// Outstanding unbind replies parked until `MonitorNotice`
    /// arrives from the listener being closed. Key is the same
    /// `MailboxId` as `listeners`; the cap's monitor (registered
    /// at spawn time) is what fires the notice.
    pub pending_unbinds: HashMap<aether_data::MailboxId, PendingUnbind>,
    /// Monotonic id assigned to the next outbound connect attempt.
    pub next_connect_id: u64,
    /// Outstanding connect replies parked until the dial sidecar
    /// reports completion. Keyed by `next_connect_id` values.
    pub pending_connects: HashMap<u64, PendingConnect>,
    /// Dial-sidecar result channel. The dispatcher drains it on each
    /// `ConnectReady` wake and correlates results by connect id.
    pub connect_rx: mpsc::Receiver<(u64, Result<TcpStream, String>)>,
    /// Retained sender cloned into each one-shot dial sidecar.
    pub connect_tx: mpsc::Sender<(u64, Result<TcpStream, String>)>,
    /// Wake-mail plumbing captured at init for one-shot dial sidecars.
    pub connect_mailer: Arc<Mailer>,
    pub connect_self_id: aether_data::MailboxId,
    pub connect_ready_kind: KindId,
}

/// Cap-local supervisor state for one live listener. Drops with
/// the entry; `MonitorHandle::Drop` is idempotent with the close
/// path's index drain.
pub struct ListenerEntry {
    pub addr: String,
    pub port: u16,
    pub name: String,
    // Held to keep the cap's monitor registered against the
    // listener for its lifetime. Drops when the entry is removed
    // (in `on_monitor_notice`).
    _monitor_handle: MonitorHandle,
}

pub struct PendingUnbind {
    pub sender: aether_data::Source,
    pub hold: SettlementHold,
    pub listener_name: String,
}

pub struct PendingConnect {
    pub sender: aether_data::Source,
    pub hold: SettlementHold,
    pub addr: String,
    pub name: Option<String>,
    pub consumer: Option<aether_data::MailboxId>,
}

fn reply_to_pending_connect(
    ctx: &mut NativeCtx<'_, Manual>,
    sender: aether_data::Source,
    hold: SettlementHold,
    reply: &ConnectResult,
) {
    ctx.reply_to_target(sender, reply, hold.root(), None);
    drop(hold);
}

#[runtime]
impl NativeActor for TcpCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// cap-local listener-fleet supervisor map.
    type State = TcpCapabilityState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.tcp";

    fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<TcpCapabilityState, BootError> {
        let (connect_tx, connect_rx) = mpsc::channel::<(u64, Result<TcpStream, String>)>();
        Ok(TcpCapabilityState {
            listeners: HashMap::new(),
            pending_unbinds: HashMap::new(),
            next_connect_id: 0,
            pending_connects: HashMap::new(),
            connect_rx,
            connect_tx,
            connect_mailer: ctx.mailer(),
            connect_self_id: ctx.self_id(),
            connect_ready_kind: KindId(<ConnectReady as Kind>::ID.0),
        })
    }

    /// Dial an outbound TCP stream on a one-shot transport thread and
    /// park the caller's reply until `ConnectReady` reports completion.
    ///
    /// # Agent
    /// Reply: `ConnectResult`. Asynchronous — the shared cap dispatcher
    /// remains available while the OS resolves and connects `mail.addr`.
    #[handler::manual]
    fn on_connect(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: Connect) {
        let id = state.next_connect_id;
        state.next_connect_id += 1;
        state.pending_connects.insert(
            id,
            PendingConnect {
                sender: ctx.reply_target(),
                hold: ctx.acquire_settlement_hold(),
                addr: mail.addr.clone(),
                name: mail.name,
                consumer: mail.consumer,
            },
        );

        let connect_tx = state.connect_tx.clone();
        let mailer = Arc::clone(&state.connect_mailer);
        let self_id = state.connect_self_id;
        let connect_ready_kind = state.connect_ready_kind;
        let addr = mail.addr;

        // Transport thread below the mail layer — it carries a dial result
        // in; no inbound chain to inherit, so no settlement umbrella applies.
        #[allow(clippy::disallowed_methods)]
        let spawn_result = thread::Builder::new().name(format!("aether-tcp-connect-{id}")).spawn(move || {
            if connect_tx
                .send((id, TcpStream::connect(&addr).map_err(|error| format!("connect failed: {error}"))))
                .is_ok()
            {
                mailer.push(Mail::new(self_id, connect_ready_kind, ConnectReady {}.encode_into_bytes(), 1));
            }
        });

        if let Err(error) = spawn_result {
            let PendingConnect { sender, hold, addr, .. } =
                state.pending_connects.remove(&id).expect("connect inserted before thread spawn");
            reply_to_pending_connect(
                ctx,
                sender,
                hold,
                &ConnectResult::Err { addr, reason: format!("connect thread spawn failed: {error}") },
            );
        }
    }

    /// Drain completed outbound dials, spawn one existing
    /// `TcpSessionActor` per connected stream, and complete each parked
    /// `ConnectResult` reply by connect id.
    #[handler::manual]
    fn on_connect_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, _mail: ConnectReady) {
        while let Ok((id, result)) = state.connect_rx.try_recv() {
            let Some(PendingConnect { sender, hold, addr, name, consumer }) = state.pending_connects.remove(&id) else {
                continue;
            };

            match result {
                Ok(stream) => {
                    let session_name = name.unwrap_or_else(|| format!("conn-{id}"));
                    let peer = match stream.peer_addr() {
                        Ok(peer) => peer.to_string(),
                        Err(error) => {
                            reply_to_pending_connect(
                                ctx,
                                sender,
                                hold,
                                &ConnectResult::Err { addr, reason: format!("peer_addr failed: {error}") },
                            );
                            continue;
                        }
                    };
                    match ctx
                        .spawn_child::<TcpSessionActor>(
                            Subname::Named(&session_name),
                            TcpSessionConfig {
                                stream: Some(stream),
                                peer: peer.clone(),
                                session_name: session_name.clone(),
                                consumer,
                            },
                        )
                        .finish()
                    {
                        Ok(session_id) => {
                            reply_to_pending_connect(
                                ctx,
                                sender,
                                hold,
                                &ConnectResult::Ok { session_name, session_id, peer },
                            );
                        }
                        Err(error) => {
                            reply_to_pending_connect(
                                ctx,
                                sender,
                                hold,
                                &ConnectResult::Err { addr, reason: format!("spawn failed: {error:?}") },
                            );
                        }
                    }
                }
                Err(reason) => {
                    reply_to_pending_connect(ctx, sender, hold, &ConnectResult::Err { addr, reason });
                }
            }
        }
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
    #[handler::single]
    fn on_bind(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: BindListener) -> BindListenerResult {
        let listener = match TcpListener::bind(&mail.addr) {
            Ok(l) => l,
            Err(e) => {
                return BindListenerResult::Err { addr: mail.addr, reason: format!("bind failed: {e}") };
            }
        };
        let local_port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(e) => {
                return BindListenerResult::Err { addr: mail.addr, reason: format!("local_addr failed: {e}") };
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
                    consumer: mail.consumer,
                },
            )
            .finish()
        {
            Ok(id) => id,
            Err(e) => {
                return BindListenerResult::Err { addr: mail.addr, reason: format!("spawn failed: {e:?}") };
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
                return BindListenerResult::Err { addr: mail.addr, reason: format!("monitor failed: {e:?}") };
            }
        };

        state.listeners.insert(
            listener_id,
            ListenerEntry {
                addr: mail.addr,
                port: local_port,
                name: subname_str.clone(),
                _monitor_handle: monitor_handle,
            },
        );

        BindListenerResult::Ok { listener_name: subname_str, listener_id, local_port }
    }

    /// Mail `Close` to the named listener and park the
    /// originator's reply target. Reply fires from
    /// `on_monitor_notice` once the listener tombstones.
    ///
    /// # Agent
    /// Reply: `UnbindListenerResult`. Asynchronous — the response
    /// fires after the listener's accept thread joins and its
    /// `MonitorNotice` arrives at this cap.
    #[handler::manual]
    fn on_unbind(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: UnbindListener) {
        // Resolve listener_id from the cap-local supervisor map by
        // name. The cap is the source of truth for "what listeners
        // exist"; no registry walk needed.
        let listener_id = state.listeners.iter().find(|(_, entry)| entry.name == mail.listener_name).map(|(id, _)| *id);
        let Some(listener_id) = listener_id else {
            ctx.reply(&UnbindListenerResult::Err {
                listener_name: mail.listener_name,
                reason: "no such listener (or already closed)".into(),
            });
            return;
        };
        // Park the reply target keyed on listener_id. The cap's
        // already-registered monitor (set at spawn time) fires
        // MonitorNotice on close, which drives the reply. Preserve
        // the first caller when a duplicate request arrives while
        // that close is still in flight; replacing it would drop the
        // original settlement hold without ever replying.
        let Entry::Vacant(pending) = state.pending_unbinds.entry(listener_id) else {
            ctx.reply(&UnbindListenerResult::Err {
                listener_name: mail.listener_name,
                reason: "unbind already in progress".into(),
            });
            return;
        };
        pending.insert(PendingUnbind {
            sender: ctx.reply_target(),
            hold: ctx.acquire_settlement_hold(),
            listener_name: mail.listener_name,
        });
        // Mail Close to the listener by its stored id. ADR-0099 §3:
        // the listener is a spawned child, so its id is the lineage
        // fold, not `hash(NAMESPACE:name)` — re-resolving by name
        // would reach a flat id nothing is registered under. The cap
        // already holds the folded id from the spawn (the
        // `state.listeners` key), so address it directly.
        ctx.actor_at::<TcpListenerActor>(listener_id).send(&Close::default());
    }

    /// Walk the cap-local listener map and report metadata.
    ///
    /// # Agent
    /// Reply: `ListListenersResult`.
    #[handler::single]
    fn on_list(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListListeners) -> ListListenersResult {
        let listeners: Vec<ListenerInfo> = state
            .listeners
            .values()
            .map(|entry| ListenerInfo { name: entry.name.clone(), addr: entry.addr.clone(), port: entry.port })
            .collect();
        ListListenersResult { listeners }
    }

    /// Listener tombstoned — remove from the supervisor map and
    /// fire the parked unbind reply if one is waiting.
    ///
    /// `MonitorNotice.target` identifies which listener closed.
    /// The cap's monitor on every spawned listener (registered in
    /// `on_bind`) fires this notice; if the close came from an
    /// unbind request, `pending_unbinds` has an entry with the
    /// originator to reply to.
    #[handler::manual]
    fn on_monitor_notice(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, notice: MonitorNotice) {
        // Drop the supervisor entry. The held MonitorHandle drops
        // here; deregister is idempotent with the close path's
        // forward-index drain.
        let _entry = state.listeners.remove(&notice.target);
        // Fire the parked unbind reply if one was waiting.
        if let Some(PendingUnbind { sender, hold, listener_name }) = state.pending_unbinds.remove(&notice.target) {
            ctx.reply_to_target(sender, &UnbindListenerResult::Ok { listener_name }, hold.root(), None);
            drop(hold);
        }
        // Else: notice came from a non-unbind close (chassis
        // shutdown, future trap). Nothing to reply to; the
        // supervisor entry is gone, that's the cleanup.
    }
}
