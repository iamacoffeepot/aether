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
    EngineId, EngineToHub, FrameError, HubToEngine, MAX_FRAME_SIZE, Uuid, Welcome,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::registry::{EngineRecord, EngineRegistry};

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

    eprintln!(
        "aether-hub: engine registered id={} name={} pid={} version={}",
        engine_id.0, hello.name, hello.pid, hello.version
    );

    let (mail_tx, mut mail_rx) = mpsc::channel::<HubToEngine>(MAIL_CHANNEL_CAPACITY);
    registry.insert(EngineRecord {
        id: engine_id,
        name: hello.name.clone(),
        pid: hello.pid,
        version: hello.version.clone(),
        kinds: hello.kinds,
        mail_tx,
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
    let result = read_loop(&mut reader).await;

    registry.remove(&engine_id);
    let _ = writer_task.await;

    match &result {
        Ok(()) => eprintln!("aether-hub: engine {} goodbye", engine_id.0),
        Err(e) => eprintln!("aether-hub: engine {} dropped: {e}", engine_id.0),
    }
    result
}

async fn read_loop(reader: &mut tokio::net::tcp::OwnedReadHalf) -> Result<(), FrameError> {
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
            EngineToHub::Goodbye(_) => return Ok(()),
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
