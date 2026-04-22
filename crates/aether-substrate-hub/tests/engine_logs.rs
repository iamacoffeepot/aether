// Integration test for ADR-0023: substrate emits a `LogBatch` frame,
// the hub appends it to its per-engine ring, and `LogStore::read`
// (the same code path the `engine_logs` MCP tool drives) returns the
// expected entries with cursor + level filter behaviour.
//
// Walks one end of the wire by hand: open a socket to the listener,
// Hello/Welcome, push a LogBatch, then query the hub's LogStore.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use aether_hub_protocol::{
    EngineId, EngineToHub, Hello, HubToEngine, LogEntry, LogLevel, encode_frame,
};
use aether_substrate_hub::{
    EngineRegistry, HUB_SELF_ENGINE_ID, LogStore, LoopbackEngine, LoopbackHandle, PendingSpawns,
    SessionRegistry, run_engine_listener,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn spawn_hub_with_logs() -> (SocketAddr, EngineRegistry, LogStore) {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let registry = EngineRegistry::new();
    let sessions = SessionRegistry::new();
    let pending = PendingSpawns::new();
    let logs = LogStore::new();
    let loopback = LoopbackEngine::boot(&registry).expect("loopback boot");
    let loopback_handle = LoopbackHandle::from_boot(&loopback.boot);
    registry.remove(&HUB_SELF_ENGINE_ID);
    tokio::spawn(run_engine_listener(
        addr,
        registry.clone(),
        sessions,
        pending,
        logs.clone(),
        loopback_handle,
    ));
    Box::leak(Box::new(loopback));
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, registry, logs)
}

async fn handshake(stream: &mut TcpStream) -> EngineId {
    let hello = EngineToHub::Hello(Hello {
        name: "log-test".into(),
        pid: 1,
        started_unix: 0,
        version: "0".into(),
        kinds: vec![],
    });
    stream.write_all(&encode_frame(&hello)).await.unwrap();

    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await.unwrap();
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut body).await.unwrap();
    let welcome: HubToEngine = postcard::from_bytes(&body).unwrap();
    match welcome {
        HubToEngine::Welcome(w) => w.engine_id,
        other => panic!("expected Welcome, got {other:?}"),
    }
}

fn entry(seq: u64, level: LogLevel, target: &str, message: &str) -> LogEntry {
    LogEntry {
        timestamp_unix_ms: seq * 1000,
        level,
        target: target.into(),
        message: message.into(),
        sequence: seq,
    }
}

#[tokio::test]
async fn log_batch_arrives_and_is_queryable() {
    let (addr, _registry, logs) = spawn_hub_with_logs().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let engine_id = handshake(&mut stream).await;

    let batch = EngineToHub::LogBatch(vec![
        entry(0, LogLevel::Info, "aether_substrate::boot", "ready"),
        entry(1, LogLevel::Error, "aether_substrate::component", "trap"),
    ]);
    stream.write_all(&encode_frame(&batch)).await.unwrap();

    // The hub appends asynchronously on the read loop; poll briefly.
    let mut got = Vec::new();
    for _ in 0..50 {
        let r = logs.read(engine_id, 100, LogLevel::Trace, 0);
        if r.entries.len() == 1 {
            got = r.entries;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // since=0 is exclusive: only sequence 1 (the Error) should come back.
    assert_eq!(got.len(), 1, "expected exactly one entry above seq 0");
    assert_eq!(got[0].sequence, 1);
    assert_eq!(got[0].level, LogLevel::Error);
}

#[tokio::test]
async fn level_filter_drops_below_min() {
    let (addr, _registry, logs) = spawn_hub_with_logs().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let engine_id = handshake(&mut stream).await;

    let batch = EngineToHub::LogBatch(vec![
        entry(1, LogLevel::Debug, "t", "noise"),
        entry(2, LogLevel::Warn, "t", "important"),
        entry(3, LogLevel::Error, "t", "critical"),
    ]);
    stream.write_all(&encode_frame(&batch)).await.unwrap();

    let mut filtered = Vec::new();
    for _ in 0..50 {
        let r = logs.read(engine_id, 100, LogLevel::Warn, 0);
        if r.entries.len() == 2 {
            filtered = r.entries;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(filtered.len(), 2);
    assert!(filtered.iter().all(|e| e.level >= LogLevel::Warn));
}

#[tokio::test]
async fn cursor_advances_with_next_since() {
    let (addr, _registry, logs) = spawn_hub_with_logs().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let engine_id = handshake(&mut stream).await;

    let batch = EngineToHub::LogBatch(vec![
        entry(1, LogLevel::Info, "t", "a"),
        entry(2, LogLevel::Info, "t", "b"),
        entry(3, LogLevel::Info, "t", "c"),
    ]);
    stream.write_all(&encode_frame(&batch)).await.unwrap();

    // Wait for delivery.
    for _ in 0..50 {
        let r = logs.read(engine_id, 100, LogLevel::Trace, 0);
        if r.entries.len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let first = logs.read(engine_id, 2, LogLevel::Trace, 0);
    assert_eq!(first.entries.len(), 2);
    assert_eq!(first.next_since, 2);

    let second = logs.read(engine_id, 100, LogLevel::Trace, first.next_since);
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].sequence, 3);
}

#[tokio::test]
async fn buffer_survives_engine_disconnect() {
    let (addr, _registry, logs) = spawn_hub_with_logs().await;
    let engine_id = {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let id = handshake(&mut stream).await;
        let batch = EngineToHub::LogBatch(vec![entry(1, LogLevel::Info, "t", "before-exit")]);
        stream.write_all(&encode_frame(&batch)).await.unwrap();
        // Wait for the hub to process the batch before dropping the socket.
        for _ in 0..50 {
            if !logs.read(id, 100, LogLevel::Trace, 0).entries.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        id
    };
    // Socket closed; engine record gone (the read loop hits EOF and
    // `registry.remove` fires). Log store must still serve the entry.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = logs.read(engine_id, 100, LogLevel::Trace, 0);
    assert_eq!(r.entries.len(), 1);
    assert_eq!(r.entries[0].message, "before-exit");
}
