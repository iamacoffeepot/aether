// Session registry for Claude MCP sessions. ADR-0008 PR 2.
//
// Mirror of `EngineRegistry` on the Claude side of the hub: each
// attached MCP session gets a `SessionToken` minted by the hub, an
// mpsc queue for inbound observation mail from engines, and a
// `SessionHandle` that deregisters on drop. The `Hub` service
// instance rmcp builds per session holds an `Arc<SessionHandle>`, so
// when the session ends (last Hub clone drops) the registry entry is
// removed automatically.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aether_hub_protocol::{EngineMailFrame, SessionToken, Uuid};
use tokio::sync::mpsc;

/// Bound on the per-session inbound observation queue. Back-pressure
/// shape: if a Claude session isn't draining fast enough, engine mail
/// senders await space on the channel.
pub const SESSION_CHANNEL_CAPACITY: usize = 256;

/// One entry in the hub's session table. `mail_tx` is how the engine
/// connection handlers push observation mail at a specific session
/// (via `ClaudeAddress::Session(token)`) or at all sessions (via
/// `ClaudeAddress::Broadcast`, which iterates the registry).
#[derive(Clone, Debug)]
pub struct SessionRecord {
    pub token: SessionToken,
    pub mail_tx: mpsc::Sender<EngineMailFrame>,
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
    sessions: SessionRegistry,
}

impl SessionHandle {
    /// Mint a fresh token, insert a `SessionRecord` with a bounded
    /// mpsc, and hand back both the handle and the receiver. The
    /// receiver drains inbound observation mail for this session —
    /// PR 3 wires it to the `receive_mail` MCP tool.
    pub fn mint(sessions: &SessionRegistry) -> (Self, mpsc::Receiver<EngineMailFrame>) {
        let token = SessionToken(Uuid::new_v4());
        let (tx, rx) = mpsc::channel(SESSION_CHANNEL_CAPACITY);
        sessions.insert(SessionRecord { token, mail_tx: tx });
        (
            Self {
                token,
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
}
