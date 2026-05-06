// Substrate-side outbound facade (ADR-0070 phase 4 / ADR-0071 phase 7).
//
// `HubOutbound` is the single point through which substrate code emits
// mail destined for "outside" — Claude sessions, other engines'
// mailboxes, the bubble-up path for locally-unknown mailboxes, and the
// log-capture / kinds-changed observation channels. Pre-refactor the
// type carried an mpsc `Sender<EngineToHub>` and call sites built
// `EngineToHub` enum variants inline; this module replaces that with
// seven high-level methods on an `EgressBackend` trait. The substrate
// no longer constructs hub frames; the translation lives in
// `aether-substrate-bundle::hub`'s `HubProtocolBackend` (ADR-0073).
//
// Identity types (`SessionToken`, `EngineId`) live in `aether-data`
// (ADR-0071 phase 7c) so the substrate describes egress targets
// without depending on hub-protocol framing. ADR-0070's "substrate
// has no hub knowledge" invariant is satisfied: no hub-protocol dep
// remains in `Cargo.toml`.

use std::sync::mpsc;
use std::sync::{Arc, OnceLock};

use aether_data::{EngineId, KindDescriptor, KindId, MailboxId, SessionToken};

use crate::mail::{ReplyTarget, ReplyTo};

/// Substrate-side mirror of the hub-protocol log entry shape (ADR-0023).
/// Held by the log-capture ring and handed to the egress backend
/// in batches; the hub backend converts to `aether_data::LogEntry`
/// at the wire boundary. Field shape matches the wire type so the
/// conversion is a struct copy.
///
/// Issue #581 added `origin`: the `MailboxId` of the actor whose
/// dispatch buffered this entry. `None` means host-emitted (substrate
/// boot, scheduler, panic hook — no actor stamp at the time of
/// emission). The hub's `engine_logs` MCP tool surfaces it for
/// per-actor attribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub timestamp_unix_ms: u64,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    pub sequence: u64,
    pub origin: Option<MailboxId>,
}

/// Severity for `LogEntry`. Mirrors `tracing::Level`. Ordered
/// most-verbose to least-verbose so a min-level filter can be
/// expressed as `entry.level >= min`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Pluggable egress backend the substrate calls through `HubOutbound`.
/// Every substrate egress intent is one method; implementations decide
/// how to satisfy it (the `aether-hub::HubProtocolBackend` translates
/// to `EngineToHub` frames and pushes onto a TCP writer channel; the
/// substrate-side `DroppingBackend` is the no-op default for the
/// pre-attach window and for chassis that opt out of hub bridging).
///
/// All methods are silent on failure — a hub disconnect, a closed
/// writer channel, or an outright dropping backend looks the same to
/// the caller. Substrate code already treats hub egress as
/// fire-and-forget at every site that uses these intents.
pub trait EgressBackend: Send + Sync {
    /// Whether this backend is currently delivering frames. Used by
    /// `Mailer::route_mail` to decide whether to bubble-up an
    /// unresolved mail or warn-drop locally.
    fn is_connected(&self) -> bool {
        false
    }

    /// Reply or push mail addressed at a specific Claude session
    /// (ADR-0008). `kind_name` is the kind's wire name; `origin` is
    /// the substrate-local emitting mailbox name (`None` for
    /// substrate-generated mail with no source mailbox).
    fn egress_to_session(
        &self,
        session: SessionToken,
        kind_name: &str,
        payload: Vec<u8>,
        origin: Option<String>,
        correlation_id: u64,
    );

    /// Mail addressed at every attached Claude session (the
    /// `hub.claude.broadcast` sink fan-out). Same shape as
    /// `egress_to_session` minus the session token.
    fn egress_broadcast(
        &self,
        kind_name: &str,
        payload: Vec<u8>,
        origin: Option<String>,
        correlation_id: u64,
    );

    /// Reply or push mail addressed at a specific component on
    /// another engine (ADR-0037 phase 2). Identifies the receiver by
    /// engine + mailbox id rather than by name because the source
    /// component reached the receiver through a hashed name in the
    /// first place.
    fn egress_to_engine_mailbox(
        &self,
        engine_id: EngineId,
        mailbox_id: MailboxId,
        kind_id: KindId,
        payload: Vec<u8>,
        count: u32,
        correlation_id: u64,
    );

    /// Bubble-up of mail whose recipient mailbox didn't resolve in
    /// the local registry (ADR-0037 phase 1). `source_mailbox_id`
    /// carries the local sending component's mailbox id so the hub
    /// can route any reply back through `egress_to_engine_mailbox`.
    fn egress_unresolved_mail(
        &self,
        recipient_mailbox_id: MailboxId,
        kind_id: KindId,
        payload: Vec<u8>,
        count: u32,
        source_mailbox_id: Option<MailboxId>,
        correlation_id: u64,
    );

    /// Announce a kind-vocabulary change (today: every successful
    /// `load_component` re-emits the engine's full descriptor list).
    fn egress_kinds_changed(&self, descriptors: Vec<KindDescriptor>);

    /// Push a batch of captured log entries (ADR-0023).
    fn egress_log_batch(&self, entries: Vec<LogEntry>);
}

/// No-op backend installed before any hub connection is attached and
/// retained on chassis that never attach one (the hub chassis itself,
/// some test harnesses). `is_connected` reports `false`; every egress
/// method is a silent drop.
pub struct DroppingBackend;

impl EgressBackend for DroppingBackend {
    fn egress_to_session(
        &self,
        _session: SessionToken,
        _kind_name: &str,
        _payload: Vec<u8>,
        _origin: Option<String>,
        _correlation_id: u64,
    ) {
    }
    fn egress_broadcast(
        &self,
        _kind_name: &str,
        _payload: Vec<u8>,
        _origin: Option<String>,
        _correlation_id: u64,
    ) {
    }
    fn egress_to_engine_mailbox(
        &self,
        _engine_id: EngineId,
        _mailbox_id: MailboxId,
        _kind_id: KindId,
        _payload: Vec<u8>,
        _count: u32,
        _correlation_id: u64,
    ) {
    }
    fn egress_unresolved_mail(
        &self,
        _recipient_mailbox_id: MailboxId,
        _kind_id: KindId,
        _payload: Vec<u8>,
        _count: u32,
        _source_mailbox_id: Option<MailboxId>,
        _correlation_id: u64,
    ) {
    }
    fn egress_kinds_changed(&self, _descriptors: Vec<KindDescriptor>) {}
    fn egress_log_batch(&self, _entries: Vec<LogEntry>) {}
}

/// Substrate-side mirror of every `EgressBackend` method, reified as
/// an enum so test code can drain a `Receiver<EgressEvent>` and assert
/// on the egress shape without depending on the hub-protocol crate.
/// Used by `RecordingBackend` (which the substrate's own integration
/// tests wire up via `crate::outbound::HubOutbound::attached_loopback`); the hub-aware
/// loopback that returns `EngineToHub` frames lives in `aether-hub`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EgressEvent {
    ToSession {
        session: SessionToken,
        kind_name: String,
        payload: Vec<u8>,
        origin: Option<String>,
        correlation_id: u64,
    },
    Broadcast {
        kind_name: String,
        payload: Vec<u8>,
        origin: Option<String>,
        correlation_id: u64,
    },
    ToEngineMailbox {
        engine_id: EngineId,
        mailbox_id: MailboxId,
        kind_id: KindId,
        payload: Vec<u8>,
        count: u32,
        correlation_id: u64,
    },
    UnresolvedMail {
        recipient_mailbox_id: MailboxId,
        kind_id: KindId,
        payload: Vec<u8>,
        count: u32,
        source_mailbox_id: Option<MailboxId>,
        correlation_id: u64,
    },
    KindsChanged {
        descriptors: Vec<KindDescriptor>,
    },
    LogBatch {
        entries: Vec<LogEntry>,
    },
}

/// Test backend that pushes every egress call onto an mpsc channel
/// for assertion. Reports `is_connected = true` so substrate code
/// that gates on connection (the bubble-up path in `Mailer::route_mail`)
/// exercises the connected path under test.
pub struct RecordingBackend {
    tx: mpsc::Sender<EgressEvent>,
}

impl RecordingBackend {
    pub fn new() -> (Self, mpsc::Receiver<EgressEvent>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx }, rx)
    }
}

impl EgressBackend for RecordingBackend {
    fn is_connected(&self) -> bool {
        true
    }

    fn egress_to_session(
        &self,
        session: SessionToken,
        kind_name: &str,
        payload: Vec<u8>,
        origin: Option<String>,
        correlation_id: u64,
    ) {
        let _ = self.tx.send(EgressEvent::ToSession {
            session,
            kind_name: kind_name.to_owned(),
            payload,
            origin,
            correlation_id,
        });
    }

    fn egress_broadcast(
        &self,
        kind_name: &str,
        payload: Vec<u8>,
        origin: Option<String>,
        correlation_id: u64,
    ) {
        let _ = self.tx.send(EgressEvent::Broadcast {
            kind_name: kind_name.to_owned(),
            payload,
            origin,
            correlation_id,
        });
    }

    fn egress_to_engine_mailbox(
        &self,
        engine_id: EngineId,
        mailbox_id: MailboxId,
        kind_id: KindId,
        payload: Vec<u8>,
        count: u32,
        correlation_id: u64,
    ) {
        let _ = self.tx.send(EgressEvent::ToEngineMailbox {
            engine_id,
            mailbox_id,
            kind_id,
            payload,
            count,
            correlation_id,
        });
    }

    fn egress_unresolved_mail(
        &self,
        recipient_mailbox_id: MailboxId,
        kind_id: KindId,
        payload: Vec<u8>,
        count: u32,
        source_mailbox_id: Option<MailboxId>,
        correlation_id: u64,
    ) {
        let _ = self.tx.send(EgressEvent::UnresolvedMail {
            recipient_mailbox_id,
            kind_id,
            payload,
            count,
            source_mailbox_id,
            correlation_id,
        });
    }

    fn egress_kinds_changed(&self, descriptors: Vec<KindDescriptor>) {
        let _ = self.tx.send(EgressEvent::KindsChanged { descriptors });
    }

    fn egress_log_batch(&self, entries: Vec<LogEntry>) {
        let _ = self.tx.send(EgressEvent::LogBatch { entries });
    }
}

/// Substrate-side outbound facade. Threads through every place that
/// emits hub-bound mail (mailer bubble-up, host_fns guest sends,
/// log_capture, control kind announcements, lifecycle dying-broadcast,
/// boot-installed broadcast sink). Holds an `OnceLock<Arc<dyn EgressBackend>>`:
/// pre-attach the lock is empty and every method drops silently;
/// post-attach the wired backend handles the egress (the standard
/// implementation is `aether-hub::HubProtocolBackend`, which serialises
/// to `EngineToHub` frames and writes them to a TCP socket).
///
/// `send_reply<K>` is a substrate-side encoding convenience — it does
/// the postcard encoding and dispatches based on `ReplyTarget`. Sinks
/// and capture handlers call it without caring which backend is wired.
pub struct HubOutbound {
    backend: OnceLock<Arc<dyn EgressBackend>>,
}

impl HubOutbound {
    /// Fresh facade with no backend attached. Egress methods drop
    /// silently until `attach_backend` is called. The boot path always
    /// constructs through this so substrate-core never holds a
    /// hub-protocol-aware default — the wiring happens at chassis
    /// composition time (the `HubClientCapability` boot, today).
    pub fn disconnected() -> Arc<Self> {
        Arc::new(Self {
            backend: OnceLock::new(),
        })
    }

    /// Wire a backend. Called once by `HubClientCapability::boot` after
    /// a successful TCP handshake (or by an in-process loopback
    /// harness). Subsequent calls warn-and-ignore — `HubOutbound` is
    /// single-backend by design so frames can't race across two
    /// implementations.
    pub fn attach_backend(&self, backend: Arc<dyn EgressBackend>) {
        if self.backend.set(backend).is_err() {
            tracing::warn!(
                target: "aether_substrate::outbound",
                "HubOutbound::attach_backend called twice — ignoring second attach",
            );
        }
    }

    /// Whether a backend is wired and reports itself connected.
    pub fn is_connected(&self) -> bool {
        self.backend
            .get()
            .map(|b| b.is_connected())
            .unwrap_or(false)
    }

    /// Reply or push mail addressed at a Claude session.
    pub fn egress_to_session(
        &self,
        session: SessionToken,
        kind_name: &str,
        payload: Vec<u8>,
        origin: Option<String>,
        correlation_id: u64,
    ) {
        if let Some(b) = self.backend.get() {
            b.egress_to_session(session, kind_name, payload, origin, correlation_id);
        }
    }

    /// Push mail addressed at every attached session.
    pub fn egress_broadcast(
        &self,
        kind_name: &str,
        payload: Vec<u8>,
        origin: Option<String>,
        correlation_id: u64,
    ) {
        if let Some(b) = self.backend.get() {
            b.egress_broadcast(kind_name, payload, origin, correlation_id);
        }
    }

    /// Reply or push mail addressed at a component on another engine.
    pub fn egress_to_engine_mailbox(
        &self,
        engine_id: EngineId,
        mailbox_id: MailboxId,
        kind_id: KindId,
        payload: Vec<u8>,
        count: u32,
        correlation_id: u64,
    ) {
        if let Some(b) = self.backend.get() {
            b.egress_to_engine_mailbox(
                engine_id,
                mailbox_id,
                kind_id,
                payload,
                count,
                correlation_id,
            );
        }
    }

    /// Bubble-up of mail whose recipient mailbox didn't resolve locally.
    pub fn egress_unresolved_mail(
        &self,
        recipient_mailbox_id: MailboxId,
        kind_id: KindId,
        payload: Vec<u8>,
        count: u32,
        source_mailbox_id: Option<MailboxId>,
        correlation_id: u64,
    ) {
        if let Some(b) = self.backend.get() {
            b.egress_unresolved_mail(
                recipient_mailbox_id,
                kind_id,
                payload,
                count,
                source_mailbox_id,
                correlation_id,
            );
        }
    }

    /// Announce a kind-vocabulary change.
    pub fn egress_kinds_changed(&self, descriptors: Vec<KindDescriptor>) {
        if let Some(b) = self.backend.get() {
            b.egress_kinds_changed(descriptors);
        }
    }

    /// Push a batch of captured log entries.
    pub fn egress_log_batch(&self, entries: Vec<LogEntry>) {
        if let Some(b) = self.backend.get() {
            b.egress_log_batch(entries);
        }
    }

    /// Build an outbound facade pre-attached to a `RecordingBackend`,
    /// returning the receiver end so substrate-side tests can drain
    /// `EgressEvent`s. Replaces the pre-refactor `attached_loopback`,
    /// which returned an `mpsc::Receiver<EngineToHub>` — substrate
    /// tests now assert on substrate-side `EgressEvent` variants
    /// instead of hub-protocol frames. The aether-hub crate ships its
    /// own `loopback_outbound` for tests that want to assert on
    /// `EngineToHub` frames specifically.
    pub fn attached_loopback() -> (Arc<Self>, mpsc::Receiver<EgressEvent>) {
        let (backend, rx) = RecordingBackend::new();
        let outbound = Self::disconnected();
        outbound.attach_backend(Arc::new(backend));
        (outbound, rx)
    }

    /// Encode `result` with postcard and route as a reply addressed at
    /// `sender`. Forks on the sender variant: `Session` routes through
    /// `egress_to_session`; `EngineMailbox` routes through
    /// `egress_to_engine_mailbox`; `None` and `Component` are silent
    /// no-ops (the latter is handled by `Mailer::send_reply` rather
    /// than the hub). Returns `true` when the encode succeeded and a
    /// backend method was called; `false` on encode failure or
    /// non-hub-routed targets. Encode errors log at `error` since
    /// they signal a postcard contract violation.
    pub fn send_reply<K>(&self, sender: ReplyTo, result: &K) -> bool
    where
        K: aether_data::Kind + serde::Serialize,
    {
        let payload = match postcard::to_allocvec(result) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    target: "aether_substrate::outbound",
                    kind = K::NAME,
                    error = %e,
                    "reply encode failed",
                );
                return false;
            }
        };
        match sender.target {
            ReplyTarget::Session(token) => {
                self.egress_to_session(token, K::NAME, payload, None, sender.correlation_id);
                true
            }
            ReplyTarget::EngineMailbox {
                engine_id,
                mailbox_id,
            } => {
                self.egress_to_engine_mailbox(
                    engine_id,
                    mailbox_id,
                    K::ID,
                    payload,
                    1,
                    sender.correlation_id,
                );
                true
            }
            ReplyTarget::None | ReplyTarget::Component(_) => false,
        }
    }
}
