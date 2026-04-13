// Wire types for the engine ↔ hub protocol. Direction is enforced by
// the top-level enums `EngineToHub` / `HubToEngine`; the bodies are
// plain structs so they're ergonomic to construct and pattern-match.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Hub-assigned stable identity for an engine connection. Fresh per
/// connect; not preserved across reconnects (resume-with-id is a V1
/// concern per ADR-0006).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EngineId(pub Uuid);

/// First frame the engine sends after the TCP connection is open.
/// The hub replies with a `Welcome` carrying the assigned `EngineId`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
    pub pid: u32,
    pub started_unix: u64,
    pub version: String,
}

/// Hub's reply to `Hello`. Carries the `EngineId` the engine should
/// treat as its identity for the rest of this connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub engine_id: EngineId,
}

/// A piece of mail routed by the hub to an engine. Kind and recipient
/// are carried by name; the engine resolves them against its local
/// registry (per ADR-0005's kind registry).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailFrame {
    pub recipient_name: String,
    pub kind_name: String,
    pub payload: Vec<u8>,
    pub count: u32,
}

/// Optional clean-shutdown marker. Either side may send it; receipt is
/// a signal that a subsequent TCP close is intentional rather than a
/// crash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goodbye {
    pub reason: String,
}

/// Frames an engine sends to the hub. V0 omits engine-originated mail
/// and replies — those land when pub-sub lands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineToHub {
    Hello(Hello),
    Heartbeat,
    Goodbye(Goodbye),
}

/// Frames the hub sends to an engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HubToEngine {
    Welcome(Welcome),
    Heartbeat,
    Mail(MailFrame),
    Goodbye(Goodbye),
}
