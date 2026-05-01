//! Engine ↔ hub channel wire types per ADR-0006. Direction is enforced
//! by the top-level enums [`EngineToHub`] / [`HubToEngine`]; the bodies
//! are plain structs so they're ergonomic to construct and pattern-match.
//!
//! ADR-0072 folded `aether-hub-protocol` into the hub crate. The wire
//! vocabulary lives here next to the runtime that speaks it; the
//! framing helpers ([`encode_frame`], [`read_frame`], [`write_frame`])
//! moved to [`aether_codec::frame`] because length-prefixed postcard is
//! generic codec-shaped machinery, reusable by any future protocol.
//!
//! ADR-0069 / ADR-0071 phase 7c earlier hoisted the schema vocabulary
//! (`SchemaType`, `KindShape`, `KindLabels`, `InputsRecord`, canonical
//! bytes, and the identity types `EngineId` / `SessionToken` / `Uuid`)
//! into `aether-data`. They're re-exported below so call sites that
//! used to write `aether_hub_protocol::EngineId` can switch to
//! `aether_hub::wire::EngineId` without chasing through `aether-data`.
//!
//! [`encode_frame`]: aether_codec::frame::encode_frame
//! [`read_frame`]: aether_codec::frame::read_frame
//! [`write_frame`]: aether_codec::frame::write_frame

use aether_data::{KindDescriptor, KindId, MailboxId};
use serde::{Deserialize, Serialize};

pub use aether_data::{EngineId, SessionToken, Uuid};

/// First frame the engine sends after the TCP connection is open.
/// The hub replies with a `Welcome` carrying the assigned `EngineId`.
///
/// `kinds` declares every mail kind this engine's registry knows about
/// along with enough structural detail for the hub to encode agent-
/// supplied params for that kind (ADR-0007). Engines that don't want
/// schema-driven encoding can send an empty `kinds` and only the raw
/// `payload_bytes` path will work for their clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
    pub pid: u32,
    pub started_unix: u64,
    pub version: String,
    pub kinds: Vec<KindDescriptor>,
}

/// Hub's reply to `Hello`. Carries the `EngineId` the engine should
/// treat as its identity for the rest of this connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub engine_id: EngineId,
}

/// A piece of mail routed by the hub to an engine. Kind and recipient
/// are carried by name; the engine resolves them against its local
/// registry (per ADR-0005's kind registry). `sender` is the hub's
/// routing handle for the originating Claude session — components
/// that want to reply-to-sender echo it back on an outbound
/// `EngineMailFrame` (ADR-0008).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailFrame {
    pub recipient_name: String,
    pub kind_name: String,
    pub payload: Vec<u8>,
    pub count: u32,
    pub sender: SessionToken,
    /// ADR-0042: opaque correlation id the session-originating
    /// caller attached. Echoed verbatim on any reply the engine
    /// emits. `0` means "no correlation" — current MCP `send_mail`
    /// doesn't populate this; tooling that wants end-to-end
    /// correlation sets it explicitly.
    #[serde(default)]
    pub correlation_id: u64,
}

/// A piece of mail the engine is sending to one or more Claude
/// sessions through the hub. The hub owns session routing, so the
/// engine addresses by `ClaudeAddress` rather than by session id or
/// recipient name (ADR-0008). `origin` is the substrate-local mailbox
/// name of the emitting component (ADR-0011); `None` for substrate
/// core pushes that have no sending mailbox. The hub forwards it
/// verbatim and does not validate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineMailFrame {
    pub address: ClaudeAddress,
    pub kind_name: String,
    pub payload: Vec<u8>,
    pub origin: Option<String>,
    /// ADR-0042 correlation echo. For session-addressed replies,
    /// the engine copies the `correlation_id` off the inbound
    /// `MailFrame` so the originating session can correlate its
    /// request to the reply. `0` for broadcasts and substrate-
    /// originated mail that didn't originate a correlation.
    #[serde(default)]
    pub correlation_id: u64,
}

/// How an engine-originated mail is addressed at the hub. `Session`
/// targets the specific MCP session whose token the engine is echoing
/// from an earlier inbound mail; `Broadcast` fan-outs to every
/// currently attached session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaudeAddress {
    Session(SessionToken),
    Broadcast,
}

/// Optional clean-shutdown marker. Either side may send it; receipt is
/// a signal that a subsequent TCP close is intentional rather than a
/// crash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goodbye {
    pub reason: String,
}

/// One captured log entry forwarded from an engine to the hub
/// (ADR-0023). Sequence is monotonic per substrate boot starting at 0
/// — agents poll `engine_logs` with `since: <last>` to consume
/// incrementally without re-receiving entries. `message` is the
/// already-formatted event text (tracing's structured fields are
/// flattened into it); per-line cap is enforced at capture time
/// (>16 KiB truncated with a `...[truncated]` marker).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp_unix_ms: u64,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    pub sequence: u64,
}

/// Severity for `LogEntry`. Mirrors `tracing::Level`. Ordered
/// most-verbose to least-verbose so a min-level filter can be
/// expressed as `entry.level >= min`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Mail bubbled up from an engine to the hub-substrate (ADR-0037
/// Phase 1). An engine sends this when its local scheduler cannot
/// resolve the target mailbox id — the expected case for a client
/// component addressing a hub-resident component by name
/// (`ctx.resolve_sink::<K>("tic_tac_toe.server")`). Fields are
/// id-only: the sender hashed from the name already, and names
/// don't survive the hash; the hub-substrate looks up the
/// component by id against its own registry.
///
/// `source_mailbox_id` (Phase 2) carries the sending component's
/// local mailbox id so the hub-chassis's reply peripheral can
/// route replies back to it. The source `engine_id` isn't on the
/// wire — the hub knows which TCP connection the frame arrived on.
/// `None` means "no reply target" (broadcast-origin, substrate-
/// generated, no `from_component` attribution); the hub-side
/// sender handle will be `NO_REPLY_HANDLE` for the receiving
/// component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineMailToHubSubstrateFrame {
    pub recipient_mailbox_id: MailboxId,
    pub kind_id: KindId,
    pub payload: Vec<u8>,
    pub count: u32,
    pub source_mailbox_id: Option<MailboxId>,
    /// ADR-0042 correlation id the originating component's
    /// `SubstrateCtx::send` minted. Carried across the hub so a
    /// bubbled-up mail's reply (ADR-0037 Phase 2) can echo back
    /// through `MailByIdFrame` and reach a parked `wait_reply_p32`
    /// on the original sender.
    #[serde(default)]
    pub correlation_id: u64,
}

/// Reply mail leaving the hub-substrate for a remote engine's
/// mailbox (ADR-0037 Phase 2). The hub-chassis's reply peripheral
/// emits this when a hub-resident component calls `ctx.reply` on
/// a sender that resolves to `ReplyEntry::RemoteEngineMailbox`.
/// The hub then forwards to the target engine's connection as
/// `HubToEngine::MailById`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailToEngineMailboxFrame {
    pub target_engine_id: EngineId,
    pub target_mailbox_id: MailboxId,
    pub kind_id: KindId,
    pub payload: Vec<u8>,
    pub count: u32,
    /// ADR-0042 correlation echo. Set by the reply-emitting engine
    /// so the target engine can correlate the reply to its original
    /// bubble-up request.
    #[serde(default)]
    pub correlation_id: u64,
}

/// Mail delivered to a specific mailbox id on an engine (ADR-0037
/// Phase 2 reply path). Unlike `MailFrame` which carries
/// `recipient_name` (used by `HubToEngine::Mail`), this is strictly
/// id-addressed — replies land without the sender having to know
/// the mailbox's name. The receiver's `HubClient` reader resolves
/// the id against the local `Registry` and pushes onto the
/// `Mailer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailByIdFrame {
    pub recipient_mailbox_id: MailboxId,
    pub kind_id: KindId,
    pub payload: Vec<u8>,
    pub count: u32,
    /// ADR-0042 correlation echo. Carries through from
    /// `EngineMailToHubSubstrateFrame.correlation_id` when the hub
    /// forwards a reply for a bubbled-up request back to the
    /// originating engine.
    #[serde(default)]
    pub correlation_id: u64,
}

/// Frames an engine sends to the hub. `Mail` is the observation path
/// (ADR-0008): engine-originated mail addressed to a Claude session
/// or broadcast to all sessions. `KindsChanged` (ADR-0010 §4) tells
/// the hub to replace its cached descriptor list for this engine —
/// needed after `aether.control.load_component` /
/// `aether.control.replace_component` registers a new kind, which the
/// hub would otherwise miss since its cache is pinned at `Hello`.
/// `LogBatch` (ADR-0023) carries captured log entries from the
/// substrate's tracing layer; the hub appends them to a per-engine
/// ring buffer served via the `engine_logs` MCP tool.
/// `MailToHubSubstrate` (ADR-0037 Phase 1) carries mail the engine
/// couldn't deliver locally — the hub-substrate resolves the id
/// against its own registry and dispatches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineToHub {
    Hello(Hello),
    Heartbeat,
    Mail(EngineMailFrame),
    KindsChanged(Vec<KindDescriptor>),
    LogBatch(Vec<LogEntry>),
    Goodbye(Goodbye),
    MailToHubSubstrate(EngineMailToHubSubstrateFrame),
    /// Reply to a remote engine's mailbox (ADR-0037 Phase 2). The
    /// hub looks up the target engine in its registry and forwards
    /// via `HubToEngine::MailById`.
    MailToEngineMailbox(MailToEngineMailboxFrame),
}

/// Frames the hub sends to an engine. `MailById` (ADR-0037
/// Phase 2) is the id-addressed delivery path used for replies
/// routed back to an engine whose component originated a bubbled-
/// up mail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HubToEngine {
    Welcome(Welcome),
    Heartbeat,
    Mail(MailFrame),
    Goodbye(Goodbye),
    MailById(MailByIdFrame),
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_codec::frame::{encode_frame, read_frame, write_frame};
    use aether_data::SchemaType;
    use std::io::Cursor;

    #[test]
    fn hello_roundtrip() {
        let hello = EngineToHub::Hello(Hello {
            name: "hello-triangle".into(),
            pid: 8910,
            started_unix: 1_712_345_678,
            version: "0.1.0".into(),
            kinds: vec![],
        });

        let mut buf = Vec::new();
        write_frame(&mut buf, &hello).unwrap();

        let mut r = Cursor::new(buf);
        let back: EngineToHub = read_frame(&mut r).unwrap();
        match back {
            EngineToHub::Hello(h) => {
                assert_eq!(h.name, "hello-triangle");
                assert_eq!(h.pid, 8910);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn welcome_roundtrip() {
        let id = EngineId(Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0));
        let msg = HubToEngine::Welcome(Welcome { engine_id: id });

        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: HubToEngine = read_frame(&mut Cursor::new(buf)).unwrap();
        match back {
            HubToEngine::Welcome(w) => assert_eq!(w.engine_id, id),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn mail_frame_roundtrip() {
        let sender = SessionToken(Uuid::from_u128(0xa_b_c_d));
        let msg = HubToEngine::Mail(MailFrame {
            recipient_name: "hello".into(),
            kind_name: "aether.tick".into(),
            payload: vec![],
            count: 1,
            sender,
            correlation_id: 0,
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: HubToEngine = read_frame(&mut Cursor::new(buf)).unwrap();
        match back {
            HubToEngine::Mail(m) => {
                assert_eq!(m.recipient_name, "hello");
                assert_eq!(m.kind_name, "aether.tick");
                assert_eq!(m.count, 1);
                assert_eq!(m.sender, sender);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn engine_mail_frame_session_roundtrip() {
        let token = SessionToken(Uuid::from_u128(0x1));
        let msg = EngineToHub::Mail(EngineMailFrame {
            address: ClaudeAddress::Session(token),
            kind_name: "aether.observation.ping".into(),
            payload: vec![1, 2, 3],
            origin: Some("physics".into()),
            correlation_id: 0,
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: EngineToHub = read_frame(&mut Cursor::new(buf)).unwrap();
        match back {
            EngineToHub::Mail(m) => {
                assert_eq!(m.address, ClaudeAddress::Session(token));
                assert_eq!(m.kind_name, "aether.observation.ping");
                assert_eq!(m.payload, vec![1, 2, 3]);
                assert_eq!(m.origin.as_deref(), Some("physics"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn engine_mail_frame_broadcast_roundtrip() {
        let msg = EngineToHub::Mail(EngineMailFrame {
            address: ClaudeAddress::Broadcast,
            kind_name: "aether.observation.world".into(),
            payload: vec![],
            origin: None,
            correlation_id: 0,
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: EngineToHub = read_frame(&mut Cursor::new(buf)).unwrap();
        match back {
            EngineToHub::Mail(m) => {
                assert_eq!(m.address, ClaudeAddress::Broadcast);
                assert!(m.origin.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn kinds_changed_roundtrip() {
        let msg = EngineToHub::KindsChanged(vec![
            KindDescriptor {
                name: "aether.tick".into(),
                schema: SchemaType::Unit,
                is_stream: false,
            },
            KindDescriptor {
                name: "physics.contact".into(),
                schema: SchemaType::Bytes,
                is_stream: false,
            },
        ]);
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: EngineToHub = read_frame(&mut Cursor::new(buf)).unwrap();
        match back {
            EngineToHub::KindsChanged(k) => {
                assert_eq!(k.len(), 2);
                assert_eq!(k[0].name, "aether.tick");
                assert_eq!(k[1].name, "physics.contact");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn log_batch_roundtrip() {
        let msg = EngineToHub::LogBatch(vec![
            LogEntry {
                timestamp_unix_ms: 1_713_379_200_123,
                level: LogLevel::Error,
                target: "aether_substrate::component".into(),
                message: "trap in deliver: unreachable".into(),
                sequence: 47,
            },
            LogEntry {
                timestamp_unix_ms: 1_713_379_200_456,
                level: LogLevel::Info,
                target: "aether_substrate::scheduler".into(),
                message: "boot complete".into(),
                sequence: 48,
            },
        ]);
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: EngineToHub = read_frame(&mut Cursor::new(buf)).unwrap();
        match back {
            EngineToHub::LogBatch(entries) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].sequence, 47);
                assert_eq!(entries[0].level, LogLevel::Error);
                assert_eq!(entries[1].target, "aether_substrate::scheduler");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn log_level_ordering() {
        assert!(LogLevel::Error > LogLevel::Warn);
        assert!(LogLevel::Warn > LogLevel::Info);
        assert!(LogLevel::Info > LogLevel::Debug);
        assert!(LogLevel::Debug > LogLevel::Trace);
    }

    #[test]
    fn heartbeat_both_directions() {
        for buf in [
            encode_frame(&EngineToHub::Heartbeat),
            encode_frame(&HubToEngine::Heartbeat),
        ] {
            // Smallest possible frame: 4 byte prefix + 1 byte postcard tag.
            assert_eq!(buf.len(), 5);
        }
    }

    #[test]
    fn multiple_frames_back_to_back() {
        let a = EngineToHub::Hello(Hello {
            name: "a".into(),
            pid: 1,
            started_unix: 0,
            version: "0".into(),
            kinds: vec![],
        });
        let b = EngineToHub::Heartbeat;
        let c = EngineToHub::Goodbye(Goodbye {
            reason: "done".into(),
        });

        let mut buf = Vec::new();
        write_frame(&mut buf, &a).unwrap();
        write_frame(&mut buf, &b).unwrap();
        write_frame(&mut buf, &c).unwrap();

        let mut r = Cursor::new(buf);
        let _: EngineToHub = read_frame(&mut r).unwrap();
        let _: EngineToHub = read_frame(&mut r).unwrap();
        let _: EngineToHub = read_frame(&mut r).unwrap();
    }
}
