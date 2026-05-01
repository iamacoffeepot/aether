// Session registry for Claude MCP sessions. ADR-0008 PR 2.
//
// Mirror of `EngineRegistry` on the Claude side of the hub: each
// attached MCP session gets a `SessionToken` minted by the hub, an
// mpsc queue for inbound observation mail from engines, and a
// `SessionHandle` that deregisters on drop. The `Hub` service
// instance rmcp builds per session holds an `Arc<SessionHandle>`, so
// when the session ends (last Hub clone drops) the registry entry is
// removed automatically.
//
// Await-reply routing: a tool call that issues a synchronous
// request — one whose reply arrives as observation mail of a known
// kind — can register a waiter via `SessionHandle::await_reply`. The
// next inbound mail of that kind bypasses the general inbound queue
// and resolves the waiter's oneshot. Guard-on-drop deregisters on
// timeout, cancel, or session drop. Unmatched inbound mail still
// falls through to `receive_mail` unchanged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aether_hub_protocol::{EngineId, SessionToken, Uuid};
use tokio::sync::{mpsc, oneshot};

/// Bound on the per-session inbound observation queue. Back-pressure
/// shape: if a Claude session isn't draining fast enough, engine mail
/// senders await space on the channel.
pub const SESSION_CHANNEL_CAPACITY: usize = 256;

/// Observation mail queued for a specific Claude session. Wraps the
/// wire-level `EngineMailFrame` with the hub-known engine id and a
/// flag so the draining session can tell broadcasts apart from
/// targeted replies when it pulls via `receive_mail`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedMail {
    pub engine_id: EngineId,
    pub kind_name: String,
    pub payload: Vec<u8>,
    pub broadcast: bool,
    /// Substrate-attested origin — the mailbox name of the emitting
    /// component, forwarded verbatim from `EngineMailFrame::origin`
    /// (ADR-0011). `None` for substrate-core pushes with no sending
    /// mailbox.
    pub origin: Option<String>,
}

/// One entry in the hub's session table. `mail_tx` is how the engine
/// connection handlers push observation mail at a specific session
/// (via `ClaudeAddress::Session(token)`) or at all sessions (via
/// `ClaudeAddress::Broadcast`, which iterates the registry).
/// `replies` is the kind-keyed registry of pending synchronous-reply
/// waiters — see `PendingReplies`.
#[derive(Clone, Debug)]
pub struct SessionRecord {
    pub token: SessionToken,
    pub mail_tx: mpsc::Sender<QueuedMail>,
    pub replies: Arc<PendingReplies>,
}

/// Per-session map of pending reply-waiters, keyed by mail kind name.
/// A tool call that expects a specific reply kind registers one
/// entry; the engine reader consults the map on every inbound mail
/// addressed to this session and diverts a matching mail to the
/// waiter's oneshot channel instead of the general inbound queue.
///
/// Only one waiter per kind at a time. A concurrent registration
/// attempt for the same kind returns `None`, letting the caller
/// decide whether to reject or retry.
#[derive(Debug, Default)]
pub struct PendingReplies {
    inner: Mutex<HashMap<String, oneshot::Sender<QueuedMail>>>,
}

impl PendingReplies {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register interest in the next mail of `kind`. Returns a
    /// receiver paired with a guard that auto-deregisters on drop, or
    /// `None` if another waiter is already registered for this kind.
    pub fn register(
        self: &Arc<Self>,
        kind: String,
    ) -> Option<(PendingReplyGuard, oneshot::Receiver<QueuedMail>)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.contains_key(&kind) {
            return None;
        }
        let (tx, rx) = oneshot::channel();
        inner.insert(kind.clone(), tx);
        Some((
            PendingReplyGuard {
                kind,
                replies: Arc::clone(self),
            },
            rx,
        ))
    }

    /// Try to deliver an inbound mail to a registered waiter. Returns
    /// `None` if a waiter was found (mail was either delivered or
    /// dropped along with a dead receiver — either way, it's handled);
    /// `Some(mail)` if no waiter was registered, letting the caller
    /// fall through to the general inbound queue.
    pub fn try_deliver(&self, kind: &str, mail: QueuedMail) -> Option<QueuedMail> {
        let mut inner = self.inner.lock().unwrap();
        match inner.remove(kind) {
            Some(tx) => {
                // Ignore send error: receiver was dropped (caller
                // timed out or cancelled). Mail is consumed either
                // way; it does not fall through.
                let _ = tx.send(mail);
                None
            }
            None => Some(mail),
        }
    }
}

/// RAII guard tying a registered waiter to the caller's scope. On
/// drop — whether from normal completion, timeout, cancel, or
/// session-wide teardown — the registry entry for this kind is
/// removed. If an inbound mail has already matched and resolved the
/// oneshot, the remove-on-drop is a no-op.
pub struct PendingReplyGuard {
    kind: String,
    replies: Arc<PendingReplies>,
}

impl Drop for PendingReplyGuard {
    fn drop(&mut self) {
        self.replies.inner.lock().unwrap().remove(&self.kind);
    }
}

/// Thread-safe map of live MCP sessions. Cheap to clone; all clones
/// share the same underlying table.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<Mutex<HashMap<SessionToken, SessionRecord>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, record: SessionRecord) {
        self.inner.lock().unwrap().insert(record.token, record);
    }

    pub fn remove(&self, token: &SessionToken) {
        self.inner.lock().unwrap().remove(token);
    }

    pub fn get(&self, token: &SessionToken) -> Option<SessionRecord> {
        self.inner.lock().unwrap().get(token).cloned()
    }

    pub fn list(&self) -> Vec<SessionRecord> {
        self.inner.lock().unwrap().values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }
}

/// Holder that owns a session's presence in the registry. Dropping
/// the handle (when the `Hub` service — and every clone of it — is
/// dropped by rmcp at end-of-session) removes the entry.
pub struct SessionHandle {
    pub token: SessionToken,
    pub replies: Arc<PendingReplies>,
    sessions: SessionRegistry,
}

impl SessionHandle {
    /// Mint a fresh token, insert a `SessionRecord` with a bounded
    /// mpsc, and hand back both the handle and the receiver. The
    /// receiver drains inbound observation mail for this session —
    /// PR 3 wires it to the `receive_mail` MCP tool.
    pub fn mint(sessions: &SessionRegistry) -> (Self, mpsc::Receiver<QueuedMail>) {
        let token = SessionToken(Uuid::new_v4());
        let (tx, rx) = mpsc::channel(SESSION_CHANNEL_CAPACITY);
        let replies = PendingReplies::new();
        sessions.insert(SessionRecord {
            token,
            mail_tx: tx,
            replies: Arc::clone(&replies),
        });
        (
            Self {
                token,
                replies,
                sessions: sessions.clone(),
            },
            rx,
        )
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.sessions.remove(&self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_inserts_and_drop_removes() {
        let sessions = SessionRegistry::new();
        assert!(sessions.is_empty());

        let (handle, _rx) = SessionHandle::mint(&sessions);
        let token = handle.token;
        assert_eq!(sessions.len(), 1);
        assert!(sessions.get(&token).is_some());

        drop(handle);
        assert!(sessions.is_empty());
        assert!(sessions.get(&token).is_none());
    }

    #[test]
    fn list_sees_every_live_session() {
        let sessions = SessionRegistry::new();
        let (h1, _rx1) = SessionHandle::mint(&sessions);
        let (h2, _rx2) = SessionHandle::mint(&sessions);
        let (h3, _rx3) = SessionHandle::mint(&sessions);

        let list = sessions.list();
        assert_eq!(list.len(), 3);

        drop(h2);
        let list = sessions.list();
        assert_eq!(list.len(), 2);
        let tokens: Vec<_> = list.iter().map(|r| r.token).collect();
        assert!(tokens.contains(&h1.token));
        assert!(tokens.contains(&h3.token));
    }

    /// Pins the check-and-insert atomicity of `PendingReplies::register`:
    /// `N` threads racing to register the same kind must produce
    /// exactly one `Some` and `N-1` `None`. Regression guard against a
    /// future refactor that splits the `contains_key` check from the
    /// `insert` across separate lock acquisitions — which would allow
    /// two callers to both observe "not in flight" and both insert,
    /// silently dropping one caller's waiter.
    #[test]
    fn register_is_atomic_under_concurrent_same_kind_calls() {
        use std::sync::Barrier;
        use std::thread;

        const RACERS: usize = 16;
        const KIND: &str = "aether.capture_frame_result";

        let replies = PendingReplies::new();
        let barrier = Arc::new(Barrier::new(RACERS));

        let handles: Vec<_> = (0..RACERS)
            .map(|_| {
                let replies = Arc::clone(&replies);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    // Every racer hits the mutex at the same moment so
                    // the contention window is maximally exercised.
                    // The returned Option<(guard, rx)> must be held by
                    // the caller for the duration of the test — the
                    // `join`-into-Vec below keeps the winner's guard
                    // alive, so its Drop doesn't fire early and let a
                    // late racer win "after" the winner.
                    barrier.wait();
                    replies.register(KIND.to_string())
                })
            })
            .collect();

        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = outcomes.iter().filter(|o| o.is_some()).count();
        let losers = outcomes.iter().filter(|o| o.is_none()).count();
        assert_eq!(winners, 1, "exactly one racer must register successfully");
        assert_eq!(losers, RACERS - 1, "every other racer must see None");
    }

    /// Companion to `register_is_atomic_...`: after the single winner
    /// drops its guard, the slot is released and a subsequent
    /// registration for the same kind succeeds. Protects against a
    /// refactor that forgot to clear the map entry on drop.
    #[test]
    fn register_slot_is_reusable_after_guard_drop() {
        let replies = PendingReplies::new();
        let kind = "aether.capture_frame_result".to_string();

        let first = replies.register(kind.clone()).expect("first register wins");
        assert!(replies.register(kind.clone()).is_none());
        drop(first);
        let second = replies.register(kind.clone()).expect("slot reusable");
        drop(second);
    }
}
