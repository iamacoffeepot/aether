// Per-engine connection handler. Owns the lifecycle for one accepted
// TCP socket: handshake → registered mail loop → teardown.
//
// Concurrency shape: the reader runs on the connection task; a writer
// task owns the write half of the split stream and drains an mpsc fed
// by both the heartbeat ticker and (later) MCP tool handlers. If the
// reader exits, it drops `mail_tx`, which closes the writer, which
// closes the write half — no explicit abort needed.

use std::time::Duration;

use aether_hub_protocol::{
    ClaudeAddress, EngineId, EngineMailFrame, EngineToHub, FrameError, HubToEngine, MAX_FRAME_SIZE,
    Uuid, Welcome,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::registry::{EngineRecord, EngineRegistry};
use crate::session::{QueuedMail, SessionRegistry};
use crate::spawn::PendingSpawns;

/// Cadence at which the hub sends `Heartbeat` to each engine.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Maximum time the hub will wait for any frame from the engine before
/// declaring the connection dead. Three missed heartbeats.
pub const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on the per-engine outbound mail queue. Back-pressure: if the
/// writer falls behind, senders (MCP tool handlers) will await space.
const MAIL_CHANNEL_CAPACITY: usize = 256;

pub async fn handle_connection(
    stream: TcpStream,
    registry: EngineRegistry,
    sessions: SessionRegistry,
    pending: PendingSpawns,
) -> Result<(), FrameError> {
    let (mut reader, mut writer) = stream.into_split();

    // Handshake: first frame must be Hello.
    let first: EngineToHub = read_frame_async(&mut reader).await?;
    let hello = match first {
        EngineToHub::Hello(h) => h,
        other => {
            return Err(FrameError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("expected Hello, got {other:?}"),
            )));
        }
    };

    let engine_id = EngineId(Uuid::new_v4());
    let welcome = HubToEngine::Welcome(Welcome { engine_id });
    write_frame_async(&mut writer, &welcome).await?;

    // If this engine was spawned by the hub, fulfil the waiting spawn
    // with the freshly minted engine id. A `false` return just means
    // the engine was started externally — equally valid, different
    // ownership.
    let spawned = pending.fulfill(hello.pid, engine_id);

    eprintln!(
        "aether-hub: engine registered id={} name={} pid={} version={} spawned={}",
        engine_id.0, hello.name, hello.pid, hello.version, spawned
    );

    let (mail_tx, mut mail_rx) = mpsc::channel::<HubToEngine>(MAIL_CHANNEL_CAPACITY);
    registry.insert(EngineRecord {
        id: engine_id,
        name: hello.name.clone(),
        pid: hello.pid,
        version: hello.version.clone(),
        kinds: hello.kinds,
        mail_tx,
        spawned,
    });

    // Writer task: drains the mpsc into the socket and injects
    // periodic heartbeats. Owning the heartbeat here (instead of a
    // separate task) means the reader dropping `mail_tx` is sufficient
    // to tear both down — no abort dance.
    let writer_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        // First tick fires immediately; skip it so the engine doesn't
        // see a hub-heartbeat before it's read our Welcome.
        interval.tick().await;
        loop {
            tokio::select! {
                msg = mail_rx.recv() => match msg {
                    Some(m) => {
                        if let Err(e) = write_frame_async(&mut writer, &m).await {
                            eprintln!("aether-hub: write failed: {e}");
                            break;
                        }
                    }
                    None => break,
                },
                _ = interval.tick() => {
                    if let Err(e) = write_frame_async(&mut writer, &HubToEngine::Heartbeat).await {
                        eprintln!("aether-hub: heartbeat write failed: {e}");
                        break;
                    }
                }
            }
        }
    });

    // Reader loop: run until the engine goes silent, sends Goodbye, or
    // the socket errors. Removing the registry entry drops the only
    // remaining `mail_tx`, which lets the writer task complete.
    let result = read_loop(&mut reader, &registry, &sessions, engine_id).await;

    registry.remove(&engine_id);
    let _ = writer_task.await;

    match &result {
        Ok(()) => eprintln!("aether-hub: engine {} goodbye", engine_id.0),
        Err(e) => eprintln!("aether-hub: engine {} dropped: {e}", engine_id.0),
    }
    result
}

async fn read_loop(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    registry: &EngineRegistry,
    sessions: &SessionRegistry,
    engine_id: EngineId,
) -> Result<(), FrameError> {
    loop {
        let frame = match timeout(READ_TIMEOUT, read_frame_async::<_, EngineToHub>(reader)).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(FrameError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "engine heartbeat timeout",
                )));
            }
        };
        match frame {
            EngineToHub::Hello(_) => {
                return Err(FrameError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "duplicate Hello",
                )));
            }
            EngineToHub::Heartbeat => {}
            EngineToHub::Mail(m) => route_engine_mail(sessions, engine_id, m).await,
            EngineToHub::KindsChanged(kinds) => registry.update_kinds(&engine_id, kinds),
            EngineToHub::Goodbye(_) => return Ok(()),
        }
    }
}

/// Fan an `EngineToHub::Mail` frame out to the addressed session(s).
/// Unknown / disconnected tokens are logged and dropped; the engine
/// wire has no reply, so there's nowhere to surface "sessionGone" to
/// today. `Broadcast` to an empty registry is a no-op — not an error.
async fn route_engine_mail(sessions: &SessionRegistry, engine_id: EngineId, mail: EngineMailFrame) {
    let EngineMailFrame {
        address,
        kind_name,
        payload,
        origin,
    } = mail;
    match address {
        ClaudeAddress::Session(token) => match sessions.get(&token) {
            Some(record) => {
                let queued = QueuedMail {
                    engine_id,
                    kind_name,
                    payload,
                    broadcast: false,
                    origin,
                };
                if record.mail_tx.send(queued).await.is_err() {
                    eprintln!(
                        "aether-hub: engine {} mail to session {} dropped: receiver closed",
                        engine_id.0, token.0
                    );
                }
            }
            None => {
                eprintln!(
                    "aether-hub: engine {} mail dropped: unknown/expired session token {}",
                    engine_id.0, token.0
                );
            }
        },
        ClaudeAddress::Broadcast => {
            let records = sessions.list();
            if records.is_empty() {
                return;
            }
            for record in records {
                let queued = QueuedMail {
                    engine_id,
                    kind_name: kind_name.clone(),
                    payload: payload.clone(),
                    broadcast: true,
                    origin: origin.clone(),
                };
                if record.mail_tx.send(queued).await.is_err() {
                    eprintln!(
                        "aether-hub: engine {} broadcast to session {} dropped: receiver closed",
                        engine_id.0, record.token.0
                    );
                }
            }
        }
    }
}

/// Async analogue of `aether_hub_protocol::read_frame`. Keeps the
/// protocol crate tokio-free; the substrate client will grow a
/// symmetrical helper next PR.
async fn read_frame_async<R, T>(r: &mut R) -> Result<T, FrameError>
where
    R: AsyncReadExt + Unpin,
    T: serde::de::DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            size: len,
            max: MAX_FRAME_SIZE,
        });
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(postcard::from_bytes(&buf)?)
}

async fn write_frame_async<W, T>(w: &mut W, msg: &T) -> Result<(), FrameError>
where
    W: AsyncWriteExt + Unpin,
    T: serde::Serialize,
{
    let bytes = aether_hub_protocol::encode_frame(msg);
    w.write_all(&bytes).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionHandle;

    fn engine_id(n: u128) -> EngineId {
        EngineId(Uuid::from_u128(n))
    }

    fn mail(address: ClaudeAddress, payload: Vec<u8>) -> EngineMailFrame {
        EngineMailFrame {
            address,
            kind_name: "aether.observation.ping".into(),
            payload,
            origin: None,
        }
    }

    fn mail_with_origin(address: ClaudeAddress, payload: Vec<u8>, origin: &str) -> EngineMailFrame {
        EngineMailFrame {
            address,
            kind_name: "aether.observation.ping".into(),
            payload,
            origin: Some(origin.into()),
        }
    }

    #[tokio::test]
    async fn session_address_routes_to_that_session() {
        let sessions = SessionRegistry::new();
        let (a, mut rx_a) = SessionHandle::mint(&sessions);
        let (_b, mut rx_b) = SessionHandle::mint(&sessions);

        route_engine_mail(
            &sessions,
            engine_id(1),
            mail_with_origin(ClaudeAddress::Session(a.token), vec![1, 2, 3], "physics"),
        )
        .await;

        let got = rx_a.try_recv().expect("frame for a");
        assert_eq!(got.payload, vec![1, 2, 3]);
        assert_eq!(got.engine_id, engine_id(1));
        assert!(!got.broadcast, "session address should not set broadcast");
        assert_eq!(got.origin.as_deref(), Some("physics"));
        assert!(rx_b.try_recv().is_err(), "b should not have received");
    }

    #[tokio::test]
    async fn session_address_with_unknown_token_is_dropped() {
        let sessions = SessionRegistry::new();
        let (_a, mut rx_a) = SessionHandle::mint(&sessions);

        let nobody = aether_hub_protocol::SessionToken(Uuid::from_u128(0xdead));
        route_engine_mail(
            &sessions,
            engine_id(1),
            mail(ClaudeAddress::Session(nobody), vec![]),
        )
        .await;

        assert!(rx_a.try_recv().is_err());
    }

    #[tokio::test]
    async fn broadcast_fans_out_to_every_session() {
        let sessions = SessionRegistry::new();
        let (_a, mut rx_a) = SessionHandle::mint(&sessions);
        let (_b, mut rx_b) = SessionHandle::mint(&sessions);
        let (_c, mut rx_c) = SessionHandle::mint(&sessions);

        route_engine_mail(
            &sessions,
            engine_id(1),
            mail_with_origin(ClaudeAddress::Broadcast, vec![42], "render"),
        )
        .await;

        for (name, rx) in [("a", &mut rx_a), ("b", &mut rx_b), ("c", &mut rx_c)] {
            let got = rx.try_recv().unwrap_or_else(|_| panic!("{name} no frame"));
            assert_eq!(got.payload, vec![42]);
            assert!(got.broadcast, "{name}: broadcast flag should be set");
            assert_eq!(got.engine_id, engine_id(1));
            assert_eq!(got.origin.as_deref(), Some("render"), "{name}: origin");
        }
    }

    #[tokio::test]
    async fn broadcast_with_no_sessions_is_noop() {
        let sessions = SessionRegistry::new();
        // No panic, no error — just nothing happens.
        route_engine_mail(
            &sessions,
            engine_id(1),
            mail(ClaudeAddress::Broadcast, vec![]),
        )
        .await;
    }
}
