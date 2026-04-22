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
    MailByIdFrame, Uuid, Welcome,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::timeout;

use crate::log_store::LogStore;
use crate::loopback::LoopbackHandle;
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
    logs: LogStore,
    loopback: LoopbackHandle,
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
        "aether-substrate-hub: engine registered id={} name={} pid={} version={} spawned={}",
        engine_id.0, hello.name, hello.pid, hello.version, spawned
    );

    let (mail_tx, mut mail_rx) = mpsc::channel::<HubToEngine>(MAIL_CHANNEL_CAPACITY);
    registry.insert(EngineRecord {
        id: engine_id,
        name: hello.name.clone(),
        pid: hello.pid,
        version: hello.version.clone(),
        kinds: hello.kinds,
        components: std::collections::HashMap::new(),
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
                            eprintln!("aether-substrate-hub: write failed: {e}");
                            break;
                        }
                    }
                    None => break,
                },
                _ = interval.tick() => {
                    if let Err(e) = write_frame_async(&mut writer, &HubToEngine::Heartbeat).await {
                        eprintln!("aether-substrate-hub: heartbeat write failed: {e}");
                        break;
                    }
                }
            }
        }
    });

    // Reader loop: run until the engine goes silent, sends Goodbye, or
    // the socket errors. Removing the registry entry drops the only
    // remaining `mail_tx`, which lets the writer task complete.
    let result = read_loop(
        &mut reader,
        &registry,
        &sessions,
        &logs,
        &loopback,
        engine_id,
    )
    .await;

    registry.remove(&engine_id);
    let _ = writer_task.await;

    match &result {
        Ok(()) => eprintln!("aether-substrate-hub: engine {} goodbye", engine_id.0),
        Err(e) => eprintln!("aether-substrate-hub: engine {} dropped: {e}", engine_id.0),
    }
    result
}

async fn read_loop(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    registry: &EngineRegistry,
    sessions: &SessionRegistry,
    logs: &LogStore,
    loopback: &LoopbackHandle,
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
            EngineToHub::Mail(m) => route_engine_mail(sessions, engine_id, m),
            EngineToHub::KindsChanged(kinds) => registry.update_kinds(&engine_id, kinds),
            EngineToHub::LogBatch(entries) => logs.append(engine_id, entries),
            EngineToHub::MailToHubSubstrate(frame) => {
                // ADR-0037 Phase 2: attribute the bubbled-up mail
                // to the sending engine so the hub-resident
                // component's reply-to-sender has an `engine_id`
                // to route back to. The `registry` (engine
                // registry) is handed through so the loopback can
                // reach the originator if it needs to emit an
                // `aether.mail.unresolved` diagnostic (issue #185).
                loopback.deliver_bubbled_mail(engine_id, frame, registry)
            }
            EngineToHub::MailToEngineMailbox(frame) => {
                // ADR-0037 Phase 2: a remote engine's component
                // replied to an engine-mailbox sender. Route the
                // frame onward to the target engine's connection
                // as `HubToEngine::MailById`. Drops silently if the
                // target is unknown (could have disconnected
                // mid-flight).
                if let Some(record) = registry.get(&frame.target_engine_id) {
                    let by_id = HubToEngine::MailById(MailByIdFrame {
                        recipient_mailbox_id: frame.target_mailbox_id,
                        kind_id: frame.kind_id,
                        payload: frame.payload,
                        count: frame.count,
                    });
                    if let Err(e) = record.mail_tx.try_send(by_id) {
                        eprintln!(
                            "aether-substrate-hub: reply to engine {:?} dropped: {e}",
                            frame.target_engine_id,
                        );
                    }
                } else {
                    eprintln!(
                        "aether-substrate-hub: reply to unknown engine {:?} dropped",
                        frame.target_engine_id,
                    );
                }
            }
            EngineToHub::Goodbye(_) => return Ok(()),
        }
    }
}

/// Fan an `EngineToHub::Mail` frame out to the addressed session(s).
///
/// Delivery is **non-blocking** (`try_send`): if a session's mpsc is
/// full, the mail is dropped with a warn and we keep going. See issue
/// 159 — the original `send(...).await` here could block the caller
/// (the per-engine `read_loop`) waiting for a slow MCP client to drain
/// its queue. Under broadcast fan-out across N sessions, that also
/// meant one lagging session gated mail delivery to every *other*
/// session on the same loop iteration. Worst case, a queued engine
/// SIGKILL during the block delayed reap indefinitely because the
/// read loop never got back to its next `read_exact`.
///
/// Observation mail is fire-and-forget by design (ADR-0008). A
/// bounded per-session ring with drop-oldest-semantics was always the
/// implicit contract; `try_send` plus drop-on-full makes it
/// explicit and unblocks the engine path. Targeted replies matched
/// by `PendingReplies::try_deliver` bypass the queue entirely, so
/// synchronous tool calls are unaffected.
///
/// Unknown / disconnected session tokens are logged and dropped; the
/// engine wire has no reply, so there's nowhere to surface
/// "sessionGone". `Broadcast` to an empty registry is a no-op.
pub(crate) fn route_engine_mail(
    sessions: &SessionRegistry,
    engine_id: EngineId,
    mail: EngineMailFrame,
) {
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
                // Synchronous-reply diversion: if a tool call has
                // registered a waiter for this exact kind on this
                // session, the mail goes to the oneshot and skips the
                // general inbound queue. Unmatched mail falls through
                // to the non-blocking session-queue path.
                let kind_name = queued.kind_name.clone();
                if let Some(queued) = record.replies.try_deliver(&kind_name, queued) {
                    dispatch_session_mail(engine_id, &record.mail_tx, token.0, queued);
                }
            }
            None => {
                eprintln!(
                    "aether-substrate-hub: engine {} mail dropped: unknown/expired session token {} kind={}",
                    engine_id.0, token.0, kind_name
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
                dispatch_session_mail(engine_id, &record.mail_tx, record.token.0, queued);
            }
        }
    }
}

/// Non-blocking session enqueue. Logs Full (slow-client drop) and
/// Closed (receiver gone) separately — they're different failure
/// modes worth telling apart in triage.
fn dispatch_session_mail(
    engine_id: EngineId,
    mail_tx: &mpsc::Sender<QueuedMail>,
    session_token: Uuid,
    queued: QueuedMail,
) {
    match mail_tx.try_send(queued) {
        Ok(()) => {}
        Err(TrySendError::Full(queued)) => {
            eprintln!(
                "aether-substrate-hub: engine {} mail to session {} dropped: queue full (kind={}, broadcast={})",
                engine_id.0, session_token, queued.kind_name, queued.broadcast
            );
        }
        Err(TrySendError::Closed(queued)) => {
            eprintln!(
                "aether-substrate-hub: engine {} mail to session {} dropped: receiver closed (kind={}, broadcast={})",
                engine_id.0, session_token, queued.kind_name, queued.broadcast
            );
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
        );

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
        );

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
        );

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
        );
    }

    /// Issue 159 regression. Fill a session's inbound mpsc to capacity,
    /// then route one more mail. The pre-fix path called
    /// `mail_tx.send(...).await` and would block here — if that
    /// happens we'll hang the test. With `try_send` the extra mail is
    /// dropped (Full) and the call returns promptly.
    #[tokio::test]
    async fn session_queue_full_drops_instead_of_blocking() {
        let sessions = SessionRegistry::new();
        let (a, mut rx_a) = SessionHandle::mint(&sessions);

        // Fill the channel right up to capacity.
        for _ in 0..crate::session::SESSION_CHANNEL_CAPACITY {
            route_engine_mail(
                &sessions,
                engine_id(1),
                mail(ClaudeAddress::Session(a.token), vec![0]),
            );
        }

        // One more. Pre-fix: blocks forever. Post-fix: drops,
        // returns promptly. `route_engine_mail` is sync so a block
        // would hang the test itself — the bare call *is* the
        // assertion.
        route_engine_mail(
            &sessions,
            engine_id(1),
            mail(ClaudeAddress::Session(a.token), vec![99]),
        );

        // Consumer drains capacity + the dropped one *must not* be in there
        // (last_tail is the final enqueued entry, which is the 256th, not 99).
        let mut last_payload = None;
        while let Ok(frame) = rx_a.try_recv() {
            last_payload = Some(frame.payload);
        }
        assert_eq!(last_payload, Some(vec![0]));
    }

    /// Complementary: broadcast fan-out must not block on any one
    /// slow session. If session A is full and B is empty, B still
    /// receives the broadcast.
    #[tokio::test]
    async fn broadcast_skips_full_session_without_blocking_others() {
        let sessions = SessionRegistry::new();
        let (_a, mut rx_a) = SessionHandle::mint(&sessions);
        let (_b, mut rx_b) = SessionHandle::mint(&sessions);

        // Fill only session A.
        for _ in 0..crate::session::SESSION_CHANNEL_CAPACITY {
            rx_b.try_recv().ok();
            route_engine_mail(
                &sessions,
                engine_id(1),
                mail(ClaudeAddress::Broadcast, vec![0]),
            );
            // Drain B as we go so it stays empty; A fills up.
            while rx_b.try_recv().is_ok() {}
        }

        // One more broadcast. A is full, must drop; B is empty,
        // must receive. A block here would hang the test since the
        // call is sync.
        route_engine_mail(
            &sessions,
            engine_id(1),
            mail(ClaudeAddress::Broadcast, vec![77]),
        );

        let got_b = rx_b
            .try_recv()
            .expect("session B should have received the final broadcast");
        assert_eq!(got_b.payload, vec![77]);
        // A's queue is full of 0-payload sends from the fill loop; the
        // final 77 was dropped. Drain and check we never see 77.
        let mut saw_77 = false;
        while let Ok(frame) = rx_a.try_recv() {
            if frame.payload == vec![77] {
                saw_77 = true;
            }
        }
        assert!(
            !saw_77,
            "session A's queue was full; the final broadcast must not have landed"
        );
    }
}
